// The 대화 screen (`#/chat`, `#/chat/<session id>`): one live chat with the
// server's agent. Unlike the other screens it is not a cached GET: it keeps
// an event stream open while it is shown and closes it when it is left.

import { ApiError, openStream, postJson, getJson } from "./api.js";
import {
  ANSWER_LABEL,
  STREAM_SILENCE_MS,
  answerChoices,
  approvalDeadline,
  beforeRunningTurn,
  closedText,
  createSseParser,
  formatRemaining,
  messageProblem,
  parseEvent,
  reconnectDelay,
  stopReasonText,
  streamProblem,
  toolLabel,
  unfinishedTurns,
} from "./chat-stream.js";
import { code, el, notice } from "./dom.js";
import { formatInt, formatUsd } from "./format.js";
import { linkHistory, parseCheckpoints, undoneBy, undoPlan } from "./checkpoints.js";
import { chatHash, sessionHash } from "./routes.js";
import { messageView, peek, toolBody } from "./view-session.js";
import { changePanel, confirmUndo, outcomeView, turnFooter } from "./view-undo.js";

const WRITE_TIMEOUT_MS = 30000;
/** How many of a resumed session's last messages to show above the chat. */
const HISTORY = 20;
/** The replay the server sends on connecting is taken as done after this long without a byte... */
const REPLAY_QUIET_MS = 400;
/** ...or this long after the stream opened, whichever comes first. */
const REPLAY_MAX_MS = 1500;

let mounts = 0;

/** What the person approves, shown in full: the server cuts none of it. */
const TARGET_LABEL = { bash: "실행할 명령", write_file: "쓸 파일", edit_file: "고칠 파일" };
/** From this length on, say that the box scrolls, so the end is not missed. */
const LONG_TARGET = 400;
/** How much of a long command's end to show again on its own, never scrolled away. */
const TAIL = 240;

/**
 * The parts of a question: its target (the command or path), why it is
 * asked and what a file tool would write, from the fields the server sends,
 * or its plain `summary` when it sends no `target`. Every part is shown
 * whole, wrapping and scrolling in its box; a long one says how long it is.
 */
function approvalDetails(event, idBase) {
  if (typeof event.target !== "string") {
    return el("pre", { class: "approval-summary", tabindex: "0", "aria-label": "질문 내용" }, String(event.summary ?? ""));
  }
  const label = TARGET_LABEL[event.kind] ?? "대상";
  const parts = [
    el("p", { class: "approval-label", id: `${idBase}-target` }, label),
    el("pre", { class: "approval-summary", tabindex: "0", "aria-labelledby": `${idBase}-target` }, event.target),
  ];
  if (event.target.length >= LONG_TARGET) {
    // What runs last is where a long command can hide something; show it
    // again where it cannot be scrolled out of sight.
    parts.push(
      el(
        "p",
        { class: "approval-long" },
        `${formatInt(event.target.length)}자로 깁니다. 위 상자를 끝까지 내려 전부 확인한 뒤 답해 주세요.`,
      ),
      el("p", { class: "approval-label", id: `${idBase}-tail` }, `명령의 끝 ${formatInt(TAIL)}자`),
      el("pre", { class: "approval-summary approval-tail", "aria-labelledby": `${idBase}-tail` }, `…${event.target.slice(-TAIL)}`),
    );
  }
  const reasons = Array.isArray(event.reasons) ? event.reasons.filter((r) => typeof r === "string" && r) : [];
  if (reasons.length) {
    parts.push(el("p", { class: "approval-label" }, "묻는 이유"), el("ul", { class: "approval-reasons" }, reasons.map((r) => el("li", null, r))));
  }
  if (typeof event.preview === "string" && event.preview) {
    parts.push(
      el("p", { class: "approval-label", id: `${idBase}-preview` }, "쓸 내용"),
      el("pre", { class: "approval-summary", tabindex: "0", "aria-labelledby": `${idBase}-preview` }, event.preview),
    );
  }
  return parts;
}

/** The 새 대화 link next to the title. */
export function renderControls(root, ctx) {
  // replaceChildren would print a null as the text "null".
  if (ctx.args?.id) root.replaceChildren(el("a", { class: "button button-secondary", href: chatHash(null) }, "새 대화"));
  else root.replaceChildren();
}

/**
 * Builds the screen in `root` for `args.id` (a session to go on with) or a
 * new chat, and returns a function that closes its stream and timers.
 */
export function mount(root, ctx, args) {
  const n = ++mounts;
  const view = new ChatView(root, ctx, args?.id ?? null, n);
  view.start();
  return () => view.stop();
}

class ChatView {
  constructor(root, ctx, sessionId, n) {
    this.root = root;
    this.ctx = ctx;
    this.id = sessionId;
    this.n = n;
    this.alive = true;
    this.lastSeq = null;
    this.running = false;
    this.sending = false;
    this.turns = new Map();
    this.questions = new Map();
    this.timers = new Set();
    this.stream = null;
    this.attempt = 0;
    this.connected = false;
    /** Bumped on a reset; an older stream loop then ends without reconnecting. */
    this.gen = 0;
    /** While not null, events are held here until the session history is shown. */
    this.replay = null;
    /** The chat's file checkpoints (GET .../checkpoints), oldest first. */
    this.checkpoints = [];
    /** Where a live turn's checkpoint is shown, by checkpoint number (turn_end.checkpoint). */
    this.cpSlots = new Map();
    this.cpLoads = 0;
    /** The server keeps no checkpoints (409 checkpoints_off): no undo buttons. */
    this.cpOff = false;
    this.undoing = false;
    /** The session's working folder, to show paths under it shortly. */
    this.cwd = null;
    this.historyShown = [];
    /**
     * Undos done from this tab: `Map(the undo's checkpoint -> checkpoints it
     * undid)`, since the list does not say. `pendingUndo` waits for the
     * undo's checkpoint to show up in the list.
     */
    this.undidHere = new Map();
    this.pendingUndo = null;
    /** Checkpoints of an undo whose answer never came: no undo for them until the list tells. */
    this.unsure = new Set();
    this.unsureTries = 0;
  }

  // ---- lifecycle ----

  start() {
    this.build();
    this.ctx.setTitle(this.id ? "대화 이어 가기" : "새 대화");
    this.ctx.chatStatus().then((on) => {
      if (!this.alive) return;
      if (on === false) return this.off();
      if (this.id) this.resume();
      else {
        this.setStatus("");
        this.form.hidden = false;
      }
    });
    this.tick = setInterval(() => this.updateClocks(), 1000);
  }

  stop() {
    this.alive = false;
    this.stream?.abort();
    clearInterval(this.tick);
    for (const timer of this.timers) clearTimeout(timer);
  }

  later(fn, ms) {
    const timer = setTimeout(() => {
      this.timers.delete(timer);
      if (this.alive) fn();
    }, ms);
    this.timers.add(timer);
  }

  // ---- layout ----

  build() {
    const id = (name) => `chat${this.n}-${name}`;
    this.statusLine = el("p", { class: "chat-status", id: id("status") });
    this.history = el("div", { class: "chat-history", hidden: true });
    this.empty = el(
      "div",
      { class: "chat-empty" },
      el("p", { class: "chat-empty-title" }, this.id ? "이 세션에 이어서 보냅니다" : "무엇을 맡길까요?"),
      el(
        "p",
        null,
        "터미널에서 junees를 쓸 때와 같은 권한으로 서버의 작업 폴더에서 실행합니다. 위험할 수 있는 명령은 실행하기 전에 여기에서 허용할지 묻습니다.",
      ),
    );
    this.log = el("ol", { class: "msgs chat-log", "aria-label": "대화" });

    this.input = el("textarea", {
      id: id("input"),
      class: "chat-input",
      rows: "3",
      maxlength: "32000",
      "aria-describedby": `${id("hint")} ${id("error")}`,
      autocomplete: "off",
    });
    this.input.addEventListener("keydown", (event) => {
      // Enter sends; Shift+Enter breaks the line. Never while a Korean IME
      // is still composing, or the last syllable would be cut.
      if (event.key !== "Enter" || event.shiftKey || event.isComposing || event.keyCode === 229) return;
      event.preventDefault();
      this.send();
    });
    this.input.addEventListener("input", () => {
      this.error.hidden = true;
      this.updateCount();
    });
    this.count = el("span", { class: "chat-count", "aria-hidden": "true" });
    this.error = el("p", { class: "chat-error", id: id("error"), role: "alert", hidden: true });
    this.sendButton = el("button", { type: "submit", class: "button" }, "보내기");
    this.cancelButton = el(
      "button",
      { type: "button", class: "button button-secondary", hidden: true, onclick: () => this.cancel() },
      "취소",
    );
    this.runningNote = el("p", { class: "chat-running", hidden: true }, el("span", { class: "chat-dot", "aria-hidden": "true" }), "답을 만드는 중입니다");
    this.form = el(
      "form",
      { class: "chat-form card", hidden: true, onsubmit: (event) => (event.preventDefault(), this.send()) },
      el("label", { class: "chat-label", for: id("input") }, "메시지"),
      this.input,
      this.error,
      el(
        "div",
        { class: "chat-form-foot" },
        el("p", { class: "chat-hint", id: id("hint") }, "Enter로 보내고, Shift+Enter로 줄을 바꿉니다."),
        this.count,
        this.runningNote,
        el("div", { class: "chat-buttons" }, this.cancelButton, this.sendButton),
      ),
    );
    this.cpPanel = el("div", { class: "cp-panel-slot" });
    this.root.replaceChildren(
      el("div", { class: "chat" }, this.statusLine, this.cpPanel, this.history, this.empty, this.log, this.form),
    );
    this.setStatus("연결하는 중입니다");
  }

  setStatus(text, tone = "neutral") {
    this.statusLine.textContent = text;
    this.statusLine.className = `chat-status chat-status-${tone}`;
    this.statusLine.hidden = !text;
  }

  updateCount() {
    const length = this.input.value.length;
    this.count.textContent = length > 30000 ? `${formatInt(length)} / 32,000자` : "";
  }

  /** "Waiting for your answer" while a question is open, else "making the answer". */
  updateRunningNote() {
    const waiting = [...this.questions.values()].some((q) => !q.closed);
    // The message box stops floating while a question is open, so it never
    // covers the question's buttons.
    this.root.querySelector(".chat")?.classList.toggle("has-question", waiting);
    this.runningNote.lastChild.textContent = waiting ? "승인을 기다리는 중입니다" : "답을 만드는 중입니다";
  }

  setRunning(running) {
    this.running = running;
    this.runningNote.hidden = !running;
    this.cancelButton.hidden = !running;
    this.cancelButton.disabled = false;
    this.cancelButton.textContent = "취소";
    this.sendButton.disabled = running || this.sending || this.undoing;
    this.root.querySelector(".chat")?.setAttribute("aria-busy", String(running));
    this.updateUndoButtons();
  }

  off() {
    this.root.replaceChildren(
      notice({
        title: "대화가 꺼져 있습니다",
        body: "이 대시보드는 대화 없이 띄워졌습니다. 터미널에서 junees dashboard --chat으로 다시 띄우면 이 화면에서 대화할 수 있습니다. 채팅을 켜면 브라우저가 터미널과 같은 권한을 갖게 되니, 이 컴퓨터를 혼자 쓸 때만 켜 주세요.",
        tone: "warn",
      }),
    );
    this.ctx.setTitle("대화");
  }

  // ---- starting ----

  /** Asks the server to go on with session `this.id`, then shows its last messages and listens. */
  async resume() {
    try {
      await postJson("/api/chat", { payload: { session_id: this.id }, timeoutMs: WRITE_TIMEOUT_MS });
    } catch (err) {
      if (!this.alive || this.ctx.isUnauthorized(err)) return;
      const reason = err instanceof ApiError && err.kind === "http" ? err.message : "";
      this.root.replaceChildren(
        notice({
          title: "이 세션은 여기서 이어 갈 수 없습니다",
          body:
            err instanceof ApiError && (err.status === 404 || err.status === 400)
              ? `대화는 서버를 띄울 때 정한 작업 폴더의 세션만 이어 갈 수 있습니다.${reason ? ` 서버가 보낸 이유: ${reason}` : ""}`
              : this.ctx.describeError(err),
          tone: "fail",
          action: el(
            "div",
            { class: "notice-actions" },
            el("a", { class: "button", href: chatHash(null) }, "새 대화 시작"),
            el("a", { class: "button button-secondary", href: sessionHash(this.id) }, "세션 보기"),
          ),
        }),
      );
      return;
    }
    if (!this.alive) return;
    this.form.hidden = false;
    this.catchUp();
  }

  /**
   * Shows the session history, then only what the stream adds to it.
   * The stream first replays what the server keeps (up to 1,000 events);
   * turns that ended are in the history already, so they are held back and
   * dropped (see unfinishedTurns), and a running turn is drawn from the
   * stream after the history is cut at the person's last message.
   */
  catchUp() {
    this.replay = [];
    this.historyDone = false;
    this.replayDone = false;
    this.historyMessages = null;
    this.loadHistory().finally(() => {
      this.historyDone = true;
      this.finishReplay();
      this.refreshCheckpoints();
    });
    this.listen();
  }

  finishReplay() {
    if (!this.alive || this.replay === null || !this.historyDone || !this.replayDone) return;
    const held = this.replay;
    this.replay = null;
    // Without the history (it failed to load) everything held is new here.
    const { show, running } = this.historyMessages ? unfinishedTurns(held) : { show: held, running: false };
    if (running) this.showHistory(beforeRunningTurn(this.historyMessages), this.historyTotal);
    for (const event of show) this.render(event, true);
  }

  /**
   * The chat is gone on the server (it restarted, or closed the chat to
   * keep 16 open): open it again with its session id and draw it afresh.
   * Event numbers may start over after a restart, so nothing old is kept.
   */
  async reopen() {
    try {
      await postJson("/api/chat", { payload: { session_id: this.id }, timeoutMs: WRITE_TIMEOUT_MS });
    } catch (err) {
      if (!this.alive || this.ctx.isUnauthorized(err)) return false;
      this.connected = false;
      this.setRunning(false);
      this.setStatus(`서버에서 이 대화를 다시 열지 못했습니다. ${this.ctx.describeError(err)} 새로고침해 다시 시도해 주세요.`, "fail");
      return false;
    }
    if (!this.alive) return false;
    this.gen += 1;
    this.stream?.abort();
    this.lastSeq = null;
    this.current = null;
    this.pending = null;
    this.turns.clear();
    this.questions.clear();
    this.cpSlots.clear();
    this.log.replaceChildren();
    this.setRunning(false);
    this.setStatus("");
    this.catchUp();
    return true;
  }

  /** The resumed session's last messages, from the session API, above the live chat. */
  async loadHistory() {
    const path = (offset, limit) => `/api/sessions/${encodeURIComponent(this.id)}?offset=${offset}&limit=${limit}`;
    try {
      const head = await getJson(path(0, 1));
      const total = Number(head.total) || 0;
      if (!this.alive || total === 0) return;
      const page = total > 1 ? await getJson(path(Math.max(0, total - HISTORY), HISTORY)) : head;
      if (!this.alive) return;
      const messages = Array.isArray(page.messages) ? page.messages : [];
      this.ctx.setTitle(head.title || "대화 이어 가기");
      if (typeof head.cwd === "string") this.cwd = head.cwd;
      this.historyMessages = messages;
      this.historyTotal = total;
      this.showHistory(messages, total);
    } catch (err) {
      if (this.alive) this.ctx.isUnauthorized(err);
      // The chat works without it; the session screen has the full history.
    }
  }

  showHistory(messages, total) {
    const left = total - (this.historyMessages?.length ?? messages.length);
    const shown = left + messages.length;
    this.history.replaceChildren(
      el(
        "p",
        { class: "chat-history-head" },
        shown > messages.length ? `이전 대화 ${formatInt(shown)}개 중 마지막 ${formatInt(messages.length)}개` : `이전 대화 ${formatInt(messages.length)}개`,
        el("a", { href: sessionHash(this.id) }, "전체 보기"),
      ),
      el("ol", { class: "msgs", "aria-label": "이전 대화" }, messages.map((m) => messageView(m, new Date()))),
    );
    this.history.hidden = messages.length === 0;
    if (messages.length) this.empty.hidden = true;
    this.historyShown = messages;
    this.renderCheckpoints();
  }

  // ---- the event stream ----

  async listen() {
    if (!this.alive || !this.id) return;
    const gen = this.gen;
    const controller = new AbortController();
    this.stream = controller;
    let silence = null;
    let quiet = null;
    let cap = null;
    const replayOver = () => {
      clearTimeout(quiet);
      clearTimeout(cap);
      if (gen !== this.gen || this.replay === null) return;
      this.replayDone = true;
      this.finishReplay();
    };
    const resetSilence = () => {
      clearTimeout(silence);
      silence = setTimeout(() => controller.abort("silent"), STREAM_SILENCE_MS);
    };
    const after = this.lastSeq === null ? "" : `?after=${this.lastSeq}`;
    let problem = null;
    try {
      const response = await openStream(`/api/chat/${encodeURIComponent(this.id)}/events${after}`, {
        signal: controller.signal,
      });
      if (this.attempt > 0) this.ctx.announce("대화에 다시 연결했습니다.");
      this.attempt = 0;
      this.connected = true;
      this.setStatus("");
      resetSilence();
      if (this.replay !== null) {
        quiet = setTimeout(replayOver, REPLAY_QUIET_MS);
        cap = setTimeout(replayOver, REPLAY_MAX_MS);
      }
      const parser = createSseParser((raw) => {
        const event = parseEvent(raw);
        if (event) this.handle(event);
      });
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        resetSilence();
        parser.push(value);
        if (this.replay !== null) {
          clearTimeout(quiet);
          quiet = setTimeout(replayOver, REPLAY_QUIET_MS);
        }
      }
    } catch (err) {
      if (!this.alive || gen !== this.gen) return;
      if (this.ctx.isUnauthorized(err)) return;
      if (err instanceof ApiError && err.status === 404) {
        this.connected = false;
        this.setStatus("서버에서 이 대화가 닫혀 다시 여는 중입니다.", "warn");
        this.reopen();
        return;
      }
      problem = streamProblem(err);
    } finally {
      clearTimeout(silence);
      clearTimeout(quiet);
      clearTimeout(cap);
    }
    if (!this.alive || gen !== this.gen) return;
    // The stream ended or broke: pick up after the last event seen.
    this.connected = false;
    const wait = reconnectDelay(this.attempt);
    this.attempt += 1;
    const again = `${Math.round(wait / 1000)}초 뒤 다시 연결합니다.`;
    this.setStatus(problem ? `${problem} ${again}` : `연결이 끊겼습니다. ${again}`, "warn");
    if (this.attempt === 1) this.ctx.announce(problem ?? "대화 연결이 끊겨 다시 연결하는 중입니다.");
    this.later(() => this.listen(), wait);
  }

  handle(event) {
    if (event.seq !== null) {
      if (this.lastSeq !== null && event.seq <= this.lastSeq) return;
      this.lastSeq = event.seq;
    }
    if (this.replay !== null) {
      this.replay.push(event);
      return undefined;
    }
    return this.render(event, false);
  }

  /** Draws one event; `replayed` says it came in the replay on connecting. */
  render(event, replayed) {
    this.empty.hidden = true;
    switch (event.type) {
      case "turn_start":
        return this.turnStart(event);
      case "text":
        return this.text(event);
      case "tool_use":
        return this.toolUse(event);
      case "tool_result":
        return this.toolResult(event);
      case "approval":
        return this.approval(event, replayed);
      case "approval_closed":
        return this.approvalClosed(event);
      case "usage":
        return this.usage(event);
      case "turn_end":
        return this.turnEnd(event);
      case "error":
        return this.failed(event);
      default:
        // A newer server's event: nothing to show for it here.
        return undefined;
    }
  }

  /** The assistant item of turn `turn`, made when first needed. */
  turn(number) {
    const key = number ?? "current";
    let entry = this.turns.get(key);
    if (!entry) {
      const body = el("div", { class: "chat-turn-body" });
      const node = el(
        "li",
        { class: "msg msg-assistant chat-turn", tabindex: "-1" },
        el("p", { class: "msg-head" }, el("span", { class: "msg-role" }, "Junees")),
        body,
      );
      this.log.append(node);
      entry = { node, body, text: null, tools: [] };
      this.turns.set(key, entry);
      this.current = entry;
    }
    return entry;
  }

  userItem(text) {
    const node = el(
      "li",
      { class: "msg msg-user", tabindex: "-1" },
      el("p", { class: "msg-head" }, el("span", { class: "msg-role" }, "나")),
      el("div", { class: "msg-text" }, text),
    );
    this.log.append(node);
    return node;
  }

  turnStart(event) {
    // A message sent from this tab is shown already; one from another tab
    // or before a reload comes back in the event, when the server sends it.
    if (this.pending) {
      this.pending.dataset.turn = String(event.turn ?? "");
      this.pending = null;
    } else if (typeof event.text === "string" && event.text) {
      this.userItem(event.text);
    }
    this.turns.delete(event.turn ?? "current");
    this.turn(event.turn);
    this.setRunning(true);
  }

  text(event) {
    const entry = this.current ?? this.turn(null);
    if (!entry.text) {
      entry.text = el("div", { class: "msg-text" });
      entry.body.append(entry.text);
    }
    entry.text.append(String(event.delta ?? ""));
    this.follow();
  }

  toolUse(event) {
    const entry = this.current ?? this.turn(null);
    entry.text = null;
    const head = el("span", { class: "tool-kind" }, toolLabel(event.name));
    const node = el(
      "details",
      { class: "tool chat-tool" },
      el(
        "summary",
        null,
        el("span", { class: "tool-sum" }, head, code(event.name ?? "-"), el("span", { class: "tool-peek" }, peek(event.input_preview))),
      ),
      el("p", { class: "chat-tool-label" }, "입력"),
      toolBody(event.input_preview),
    );
    entry.body.append(node);
    entry.tools.push({ name: event.name, node, head, done: false });
    this.follow();
  }

  toolResult(event) {
    const entry = this.current ?? this.turn(null);
    const tool = entry.tools.find((t) => !t.done && t.name === event.name) ?? null;
    const result = [
      el("p", { class: "chat-tool-label" }, event.is_error ? "오류" : "결과"),
      toolBody(event.text_preview),
    ];
    if (tool) {
      tool.done = true;
      tool.node.append(...result);
      if (event.is_error) {
        tool.node.classList.add("tool-error");
        tool.head.textContent = `${toolLabel(event.name)} 오류`;
      }
    } else {
      entry.text = null;
      entry.body.append(
        el(
          "details",
          { class: `tool chat-tool${event.is_error ? " tool-error" : ""}` },
          el("summary", null, el("span", { class: "tool-sum" }, el("span", { class: "tool-kind" }, event.is_error ? "오류" : "결과"), code(event.name ?? "-"))),
          ...result,
        ),
      );
    }
    this.follow();
  }

  approval(event, replayed = false) {
    const entry = this.current ?? this.turn(null);
    entry.text = null;
    const qid = String(event.question_id ?? "");
    if (this.questions.has(qid)) return;
    const n = this.questions.size + 1;
    const titleId = `chat${this.n}-q${n}-title`;
    const bodyId = `chat${this.n}-q${n}-body`;
    // A replayed question was asked some time ago; without the server's
    // expires_at its time left is not known, so no countdown for it.
    const deadline = replayed && !event.expires_at ? null : approvalDeadline(event, Date.now());
    const clock = el("p", { class: "approval-clock" });
    const status = el("p", { class: "approval-status", role: "status" });
    const buttons = answerChoices(event.options).map((answer) =>
      el(
        "button",
        {
          type: "button",
          class: answer === "n" ? "button button-secondary" : answer === "a" ? "button button-secondary" : "button",
          onclick: () => this.answer(qid, answer),
        },
        ANSWER_LABEL[answer],
      ),
    );
    const card = el(
      "div",
      { class: "approval", role: "alertdialog", "aria-labelledby": titleId, "aria-describedby": bodyId, tabindex: "-1" },
      el("p", { class: "approval-title", id: titleId }, `승인이 필요합니다: ${toolLabel(event.kind)}`),
      el(
        "div",
        { id: bodyId },
        approvalDetails(event, `chat${this.n}-q${n}`),
        el(
          "p",
          { class: "approval-help", "data-open-only": "" },
          buttons.length === 3
            ? "항상 허용을 고르면 이 대화에서는 같은 종류를 다시 묻지 않습니다. 시간 안에 답하지 않으면 거절합니다."
            : "시간 안에 답하지 않으면 거절합니다.",
        ),
      ),
      clock,
      el("div", { class: "approval-actions" }, buttons),
      status,
    );
    entry.body.append(card);
    const question = { card, clock, status, buttons, deadline, closed: false };
    this.questions.set(qid, question);
    this.updateClock(question, Date.now());
    this.updateRunningNote();
    // Bring it into view and to the keyboard, but on the card, not on a
    // button: an Enter meant for the message box must not answer it.
    // After the rest of this piece of the stream: a replayed question that
    // closed right after it must not take the focus.
    this.later(() => {
      if (question.closed) return;
      card.focus({ preventScroll: true });
      card.scrollIntoView({ block: "nearest", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
    }, 0);
  }

  updateClock(question, now) {
    if (question.closed) return;
    question.clock.textContent =
      question.deadline === null ? "질문이 열린 뒤 5분 안에 답하지 않으면 거절합니다." : formatRemaining(question.deadline, now);
  }

  updateClocks() {
    const now = Date.now();
    for (const question of this.questions.values()) this.updateClock(question, now);
  }

  async answer(qid, answer) {
    const question = this.questions.get(qid);
    if (!question || question.closed) return;
    // Keep focus in the card: a disabled, then removed, button would drop it to the page.
    question.card.focus({ preventScroll: true });
    for (const button of question.buttons) button.disabled = true;
    question.status.textContent = `${ANSWER_LABEL[answer]} 답을 보내는 중입니다`;
    try {
      await postJson(`/api/chat/${encodeURIComponent(this.id)}/approvals/${encodeURIComponent(qid)}`, {
        payload: { answer },
        timeoutMs: WRITE_TIMEOUT_MS,
      });
      if (!question.closed) question.status.textContent = `${ANSWER_LABEL[answer]} 답을 보냈습니다.`;
    } catch (err) {
      if (!this.alive || this.ctx.isUnauthorized(err) || question.closed) return;
      if (err instanceof ApiError && err.status === 409) {
        question.status.textContent = "이미 닫힌 질문입니다. 결과를 기다리는 중입니다.";
        return;
      }
      if (err instanceof ApiError && err.status === 404) {
        question.status.textContent = "서버에 이 질문이 없습니다. 이미 끝난 질문일 수 있습니다.";
        return;
      }
      for (const button of question.buttons) button.disabled = false;
      question.status.textContent = `답을 보내지 못했습니다. ${this.ctx.describeError(err)}`;
    }
  }

  approvalClosed(event) {
    const question = this.questions.get(String(event.question_id ?? ""));
    if (!question || question.closed) return;
    question.closed = true;
    const hadFocus = question.card.contains(document.activeElement);
    question.card.setAttribute("role", "group");
    const tone = event.by === "cancel" ? "approval-cancel" : event.answer === "n" || event.by === "timeout" ? "approval-no" : "approval-yes";
    question.card.classList.add("approval-closed", tone);
    question.clock.remove();
    question.buttons[0]?.parentElement?.remove();
    question.card.querySelector("[data-open-only]")?.remove();
    const said = closedText(event.answer, event.by);
    question.status.textContent = said;
    if (hadFocus) question.card.focus({ preventScroll: true });
    this.updateRunningNote();
    // question.status is a live region already; no second announcement.
  }

  usage(event) {
    // A turn cancelled before the model answered used nothing: no line for it.
    if (!event.input_tokens && !event.output_tokens) return;
    const entry = this.current ?? this.turn(null);
    const tokens = `입력 ${formatInt(event.input_tokens)} · 출력 ${formatInt(event.output_tokens)} 토큰`;
    const cost = typeof event.cost_usd === "number" ? formatUsd(event.cost_usd) : "가격 정보 없음";
    entry.usage?.remove();
    entry.usage = el("p", { class: "chat-usage" }, `${tokens} · ${cost}`);
    entry.node.append(entry.usage);
  }

  turnEnd(event) {
    const entry = this.current;
    const note = stopReasonText(event.stop_reason);
    if (entry && note) entry.body.append(el("p", { class: "msg-note chat-stop" }, note));
    if (entry && !entry.body.childElementCount) entry.body.append(el("p", { class: "msg-note" }, "글 없이 끝났습니다."));
    // The turn changed files: its undo goes under it once the list says what changed.
    if (entry && Number.isSafeInteger(event.checkpoint) && event.checkpoint > 0) {
      const slot = el("div", { class: "cp-slot" });
      entry.node.insertBefore(slot, entry.usage ?? null);
      this.cpSlots.set(event.checkpoint, slot);
      this.refreshCheckpoints();
    }
    // A question still open when its turn ends will not be answered.
    for (const question of this.questions.values()) {
      if (!question.closed && entry?.body.contains(question.card)) {
        question.closed = true;
        question.clock.remove();
        question.buttons[0]?.parentElement?.remove();
        question.card.querySelector("[data-open-only]")?.remove();
        question.card.setAttribute("role", "group");
        question.card.classList.add("approval-closed", "approval-cancel");
        question.status.textContent = "답하기 전에 이 답이 끝나서 실행하지 않았습니다.";
      }
    }
    this.current = null;
    this.setRunning(false);
    this.updateRunningNote();
    if (this.connected) this.ctx.announce(note ? `답이 끝났습니다. ${note}` : "답이 끝났습니다.");
  }

  failed(event) {
    const entry = this.current ?? this.turn(null);
    entry.text = null;
    entry.body.append(
      el("div", { class: "callout callout-fail chat-failed" }, el("p", null, `오류: ${String(event.message ?? "알 수 없는 오류")}`)),
    );
    // usage and turn_end follow and still belong to this turn.
    this.setRunning(false);
    if (this.connected) this.ctx.announce("오류로 답을 마치지 못했습니다.");
  }

  /** Keeps the newest output in view while the reader is at the bottom. */
  follow() {
    const doc = document.scrollingElement;
    if (!doc) return;
    const nearBottom = doc.scrollHeight - doc.scrollTop - window.innerHeight < 160;
    if (nearBottom) doc.scrollTop = doc.scrollHeight;
  }

  // ---- sending ----

  showError(text) {
    this.error.textContent = text;
    this.error.hidden = false;
  }

  async send() {
    const text = this.input.value;
    const problem = messageProblem(text);
    if (problem) return this.showError(problem);
    if (this.running) return this.showError("답을 받는 중에는 보낼 수 없습니다. 끝날 때까지 기다리거나 취소해 주세요.");
    if (this.sending || this.undoing) return;
    this.sending = true;
    this.sendButton.disabled = true;
    this.updateUndoButtons();
    this.sendButton.textContent = "보내는 중";
    this.error.hidden = true;
    try {
      if (!this.id) {
        const started = await postJson("/api/chat", { payload: {}, timeoutMs: WRITE_TIMEOUT_MS });
        if (typeof started?.session_id !== "string") throw new ApiError("parse", "no session_id");
        if (!this.alive) return;
        this.id = started.session_id;
        this.ctx.setArgs({ id: this.id });
        this.listen();
      }
      const post = async () => {
        const item = this.userItem(text);
        this.pending = item;
        this.empty.hidden = true;
        try {
          await postJson(`/api/chat/${encodeURIComponent(this.id)}/messages`, {
            payload: { text },
            timeoutMs: WRITE_TIMEOUT_MS,
          });
        } catch (err) {
          if (this.pending === item) {
            item.remove();
            this.pending = null;
          }
          throw err;
        }
      };
      try {
        await post();
      } catch (err) {
        // The server closed this chat (restart, or 16 open): open it again, send once more.
        if (!(err instanceof ApiError && err.status === 404 && err.code === "not_found")) throw err;
        if (!(await this.reopen())) return;
        await post();
      }
      if (!this.alive) return;
      this.input.value = "";
      this.updateCount();
      this.follow();
    } catch (err) {
      if (!this.alive || this.ctx.isUnauthorized(err)) return;
      if (err instanceof ApiError && err.status === 409 && err.code === "session_busy") {
        this.showError("다른 곳에서 실행 중인 Junees가 이 세션을 쓰고 있어 보내지 않았습니다. 그쪽이 끝난 뒤 다시 보내 주세요.");
      } else if (err instanceof ApiError && err.status === 409) {
        this.showError("이전 답이 아직 끝나지 않아 보내지 않았습니다. 대시보드에서는 한 번에 한 대화만 답을 만듭니다. 끝날 때까지 기다리거나 취소해 주세요.");
      } else if (err instanceof ApiError && (err.kind === "timeout" || err.kind === "network")) {
        this.showError(`서버의 답을 받지 못해 보냈는지 확인하지 못했습니다. 대화에 답이 나타나지 않으면 다시 보내 주세요. ${this.ctx.describeError(err)}`);
      } else {
        const reason = err instanceof ApiError && err.kind === "http" && err.message ? ` 서버가 보낸 이유: ${err.message}` : "";
        this.showError(`보내지 못했습니다. ${this.ctx.describeError(err)}${reason}`);
      }
    } finally {
      this.sending = false;
      this.sendButton.textContent = "보내기";
      this.sendButton.disabled = this.running || this.undoing;
      this.updateUndoButtons();
    }
  }

  async cancel() {
    if (!this.id || !this.running) return;
    this.cancelButton.disabled = true;
    this.cancelButton.textContent = "취소하는 중";
    try {
      const answer = await postJson(`/api/chat/${encodeURIComponent(this.id)}/cancel`, {
        payload: {},
        timeoutMs: WRITE_TIMEOUT_MS,
      });
      // turn_end follows on the stream and puts the buttons back; with no
      // turn running there will be none.
      if (this.alive && answer?.cancelled === false) this.setRunning(false);
    } catch (err) {
      if (!this.alive || this.ctx.isUnauthorized(err)) return;
      this.cancelButton.disabled = false;
      this.cancelButton.textContent = "취소";
      this.showError(`취소하지 못했습니다. ${this.ctx.describeError(err)}`);
    }
  }

  // ---- file checkpoints (stage 20) ----

  /**
   * Loads the chat's checkpoint list and redraws what shows it; a failure
   * keeps the last one. Resolves whether a new list was drawn.
   */
  async refreshCheckpoints() {
    if (!this.id || !this.alive) return false;
    const load = ++this.cpLoads;
    let list;
    try {
      list = parseCheckpoints(await getJson(`/api/chat/${encodeURIComponent(this.id)}/checkpoints`));
    } catch (err) {
      if (this.alive) this.ctx.isUnauthorized(err);
      if (this.alive && this.unsure.size) this.checkUnsureLater();
      return false;
    }
    if (!this.alive || load !== this.cpLoads) return false;
    const pending = this.pendingUndo;
    if (pending) {
      // The undo's own checkpoint, if it changed anything: the first one
      // after the newest the list had before it.
      const made = list.find((item) => item.turn > pending.newest);
      if (made) {
        this.undidHere.set(made.turn, pending.turns);
        if (pending.slot) this.cpSlots.set(made.turn, pending.slot);
      }
      if (pending.unknown) {
        const said = made
          ? "답은 받지 못했지만 서버는 파일을 되돌렸습니다. 파일 변경 기록에서 결과를 확인해 주세요."
          : "서버에 새 되돌리기 기록이 없어, 되돌린 파일이 없는 것으로 확인했습니다.";
        this.setStatus(said, made ? "warn" : "neutral");
        this.ctx.announce(said);
        this.unsure.clear();
      }
      this.pendingUndo = null;
    }
    for (const item of list) if (!item.undid.length && this.undidHere.has(item.turn)) item.undid = this.undidHere.get(item.turn);
    this.checkpoints = list;
    if (this.cwd === null && list.length) {
      // A new chat: its working folder comes with the session.
      try {
        const head = await getJson(`/api/sessions/${encodeURIComponent(this.id)}?offset=0&limit=1`);
        if (typeof head?.cwd === "string") this.cwd = head.cwd;
      } catch {
        // Paths are shown whole then.
      }
      if (!this.alive) return false;
    }
    this.renderCheckpoints();
    return true;
  }

  /** Asks for the list again while an unanswered undo is not settled (5 s, then 10 s, up to a minute). */
  checkUnsureLater() {
    if (this.unsureTimer) return;
    const wait = Math.min(5000 * 2 ** this.unsureTries, 60000);
    this.unsureTries += 1;
    this.unsureTimer = true;
    this.later(() => {
      this.unsureTimer = false;
      if (this.unsure.size) this.refreshCheckpoints();
    }, wait);
  }

  /** Draws the change list and the end of every turn that left a checkpoint. */
  renderCheckpoints() {
    const list = this.checkpoints;
    const actions = this.cpOff
      ? null
      : { onUndo: (turn, button) => this.undo(turn, button), list, by: undoneBy(list), unsure: this.unsure };
    // Keep what the person opened across the redraw.
    const open = new Set([...this.root.querySelectorAll(".cp-files[open]")].map((d) => d.id));
    const panelOpen = Boolean(this.cpPanel.querySelector("details[open]"));
    const focused = document.activeElement?.dataset?.undo ?? null;

    const slots = new Map();
    for (const [index, turns] of linkHistory(this.historyShown, list)) {
      const anchor = this.history.querySelector(`li[data-index="${index}"]`);
      if (!anchor) continue;
      let slot = anchor.querySelector(":scope > .cp-slot");
      if (!slot) {
        slot = el("div", { class: "cp-slot" });
        anchor.append(slot);
      }
      slot.replaceChildren();
      for (const turn of turns) slots.set(turn, slot);
    }
    for (const [turn, slot] of this.cpSlots) {
      slot.replaceChildren();
      slots.set(turn, slot);
    }
    for (const [turn, slot] of slots) {
      const item = list.find((cp) => cp.turn === turn);
      if (item) slot.append(turnFooter(item, this.cwd, actions));
    }
    for (const id of open) document.getElementById(id)?.setAttribute("open", "");

    this.cpPanel.replaceChildren(...(list.length ? [changePanel(list, this.cwd, actions, panelOpen)] : []));
    this.updateUndoButtons();
    if (focused) this.root.querySelector(`.cp-slot [data-undo="${focused}"]`)?.focus();
  }

  /** Undo waits while a turn runs, a message goes out or another undo is open. */
  updateUndoButtons() {
    const busy = this.running || this.sending || this.undoing;
    for (const button of this.root.querySelectorAll("[data-undo]")) button.disabled = busy;
  }

  async undo(turn, button) {
    if (this.running || this.sending || this.undoing || !this.id) return;
    this.undoing = true;
    this.updateUndoButtons();
    this.sendButton.disabled = true;
    // Ask with the list as it is now: another tab may have undone or
    // changed files since it was drawn.
    if (!(await this.refreshCheckpoints())) {
      this.undoing = false;
      this.sendButton.disabled = this.running || this.sending;
      this.updateUndoButtons();
      this.setStatus("파일 변경 기록을 다시 받지 못해 되돌리기 창을 열지 않았습니다. 잠시 뒤 다시 시도해 주세요.", "warn");
      return;
    }
    const current = this.root.querySelector(`[data-undo="${turn}"]`);
    if (!current) {
      // The new list offers no undo here any more (undone in another tab, or nothing left to put back).
      this.undoing = false;
      this.sendButton.disabled = this.running || this.sending;
      this.updateUndoButtons();
      const said = "그사이 파일 변경 기록이 바뀌어 이 기록은 되돌릴 수 없게 됐습니다. 새로 그린 기록을 확인해 주세요.";
      this.setStatus(said, "warn");
      this.ctx.announce(said);
      return;
    }
    button = current;
    const newest = Math.max(0, ...this.checkpoints.map((item) => item.turn));
    const turns = undoPlan(this.checkpoints, turn).turns.map((item) => item.turn);
    const result = await confirmUndo({
      chatId: this.id,
      list: this.checkpoints,
      from: turn,
      cwd: this.cwd,
      opener: button,
      isUnauthorized: (err) => this.ctx.isUnauthorized(err),
    });
    this.undoing = false;
    this.sendButton.disabled = this.running || this.sending;
    this.updateUndoButtons();
    if (!this.alive) return;
    switch (result.outcome) {
      case "done": {
        const { node, headline, slot, changed } = outcomeView(result.answer, this.cwd);
        // An undo that changed files is a checkpoint too; it shows up after `newest`.
        if (changed) this.pendingUndo = { newest, turns, slot };
        this.log.append(node);
        this.empty.hidden = true;
        node.focus({ preventScroll: true });
        const show = () => node.scrollIntoView({ block: "nearest", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
        show();
        this.ctx.announce(headline);
        // The undo's own checkpoint then comes into the card; keep it all in view.
        this.refreshCheckpoints().then(() => node.isConnected && show());
        break;
      }
      case "failed":
        this.setStatus(result.message, "warn");
        this.ctx.announce(result.message);
        break;
      case "off":
        this.cpOff = true;
        this.renderCheckpoints();
        this.setStatus("이 대시보드 서버는 파일 체크포인트가 꺼져 있어 되돌릴 수 없습니다.", "warn");
        break;
      case "reload":
        this.refreshCheckpoints();
        break;
      case "unknown":
        // The files may have been undone: hold these checkpoints' undo
        // until the list shows whether the undo left its own checkpoint.
        for (const t of turns) this.unsure.add(t);
        this.unsureTries = 0;
        this.pendingUndo = { newest, turns, slot: null, unknown: true };
        this.setStatus("되돌리기의 답을 받지 못했습니다. 파일을 이미 되돌렸을 수 있어 확인하는 중입니다.", "warn");
        this.renderCheckpoints();
        this.refreshCheckpoints();
        break;
      case "cancel":
        if (button.isConnected) button.focus();
        break;
      default:
        break;
    }
  }
}
