// The 글 생성 card (stage 23): what a `draft` tool call is doing, drawn inside
// its tool block in the chat, and what it made, drawn from the tool result
// in a session's history. Every server value goes in as text.

import { el } from "./dom.js";
import {
  FAILED_SECTION_NOTE,
  SECTION_CHARS,
  applyDone,
  applyLevel,
  kindLabel,
  levelRowLabel,
  newDraft,
  parseArticle,
  progressText,
  resultStatus,
  resultText,
  sectionFailed,
  stepText,
  steps,
  stopNote,
  summaryLine,
} from "./draft.js";
import { formatInt } from "./format.js";

const BADGE = {
  running: ["만드는 중", "info"],
  done: ["완료", "ok"],
  cancelled: ["취소됨", "neutral"],
  limit: ["상한에서 멈춤", "warn"],
  failed: ["실패", "fail"],
  ended: ["중단됨", "neutral"],
  unknown: ["상태 모름", "neutral"],
};

const STEP_NOTE = { done: "완료", current: "만드는 중", skipped: "하지 않음", todo: "" };

const reducedMotion = () => typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;

// ---- one section's cell ----

function makeCell(idBase, key) {
  const textId = `${idBase}-c${key}`;
  const name = el("span", { class: "draft-cell-name" });
  const idCode = el("code", { class: "draft-cell-id" });
  const kind = el("span", { class: "badge badge-neutral" });
  const flags = el("span", { class: "draft-cell-flags" });
  const text = el("div", { class: "draft-cell-text", id: textId });
  const toggle = el("button", {
    type: "button",
    class: "button button-secondary button-small draft-more",
    "aria-expanded": "false",
    "aria-controls": textId,
    hidden: true,
  });
  const note = el("p", { class: "draft-cell-note", hidden: true });
  const node = el("li", { class: "draft-cell" }, el("div", { class: "draft-cell-head" }, name, idCode, kind, flags), text, toggle, note);
  const cell = { node, name, idCode, kind, flags, text, toggle, note, open: false, label: "" };
  text.addEventListener("animationend", () => text.classList.remove("is-fresh"));
  toggle.addEventListener("click", () => {
    setOpen(cell, !cell.open);
    fitCell(cell);
  });
  return cell;
}

/** The text box grows to a scrolling box of its own height limit when opened; opened, it takes the keyboard. */
function setOpen(cell, open) {
  cell.open = open;
  cell.text.classList.toggle("is-open", open);
  if (open) {
    cell.text.setAttribute("tabindex", "0");
    cell.text.setAttribute("role", "group");
    cell.text.setAttribute("aria-label", `${cell.label} 글`);
  } else {
    cell.text.removeAttribute("tabindex");
    cell.text.removeAttribute("role");
    cell.text.removeAttribute("aria-label");
  }
  cell.toggle.setAttribute("aria-expanded", String(open));
  cell.toggle.textContent = open ? "접기" : "펼치기";
  cell.toggle.setAttribute("aria-label", `${cell.label} 글 ${open ? "접기" : "펼치기"}`);
}

/** Shows the toggle only when the closed box cuts the text; an open one always keeps it so it can be closed. */
function fitCell(cell) {
  if (cell.open) {
    cell.toggle.hidden = false;
    return;
  }
  // Not laid out now (a folded ancestor): leave it, `fit` runs again when it opens.
  if (cell.text.clientHeight === 0) return;
  cell.toggle.hidden = !(cell.text.scrollHeight > cell.text.clientHeight + 1);
}

function fillCell(cell, section, { failed, fresh }) {
  const heading = section.concept || section.id || "절";
  cell.label = section.id && section.concept ? `${section.id} ${section.concept}` : heading;
  cell.name.textContent = heading;
  // The id is the concept's tag; shown only when there is a concept to tag.
  cell.idCode.textContent = section.concept && section.id ? section.id : "";
  cell.idCode.hidden = !cell.idCode.textContent;
  cell.kind.textContent = section.kind ? kindLabel(section.kind) : "";
  cell.kind.hidden = !section.kind;
  const changed = cell.text.textContent !== section.text;
  cell.text.textContent = section.text;
  if (changed && fresh && !reducedMotion()) {
    cell.text.classList.remove("is-fresh");
    // Restart the animation for a text that changed again.
    void cell.text.offsetWidth;
    cell.text.classList.add("is-fresh");
  }
  const flags = [];
  if (section.truncated) flags.push(el("span", { class: "badge badge-neutral" }, "잘림"));
  if (failed) flags.push(el("span", { class: "badge badge-fail" }, "대신함"));
  cell.flags.replaceChildren(...flags);
  const notes = [];
  if (failed) notes.push(FAILED_SECTION_NOTE);
  if (section.truncated) notes.push(`서버가 긴 글을 ${formatInt(SECTION_CHARS)}자에서 잘라 보냈습니다. 뒷부분은 보이지 않습니다.`);
  cell.note.textContent = notes.join(" ");
  cell.note.hidden = notes.length === 0;
  cell.node.classList.toggle("is-failed", failed);
  setOpen(cell, cell.open);
}

// ---- the card ----

export class DraftCard {
  /**
   * `idBase` makes the ids unique on the page. `inputPreview` is the
   * `draft` call's input text (the levels asked for are read from it).
   */
  constructor(idBase, inputPreview = "") {
    this.id = idBase;
    this.state = newDraft(inputPreview);
    this.cells = new Map();
    /** Set for a card drawn from a tool result the server sent no `draft` status for: no numbers to show. */
    this.unknown = false;
    /** What a cut tool result leaves out: `{ omitted, tail }` (see resultText), or null. */
    this.gap = null;
    const titleId = `${idBase}-title`;
    this.title = el("p", { class: "draft-title", id: titleId }, "글 생성");
    this.badge = el("span", { class: "badge badge-info draft-badge" });
    this.steps = el("ol", { class: "draft-steps", "aria-label": "생성 단계" });
    // The one live region: the level reached while it runs, the summary when it ends.
    this.status = el("p", { class: "draft-status", role: "status" });
    this.error = el("p", { class: "draft-error", hidden: true });
    this.failedLine = el("p", { class: "draft-failed", hidden: true });
    this.list = el("ol", { class: "draft-cells", "aria-label": "절별 글" });
    this.gapSlot = el("div", { class: "draft-gap-slot" });
    this.tableSlot = el("div", { class: "draft-table-slot" });
    this.foldSummary = el("summary", null, "");
    this.fold = el("details", { class: "draft-fold", id: `${idBase}-fold`, open: true }, this.foldSummary, this.list, this.gapSlot, this.tableSlot);
    // Whether a cell's text needs its 펼치기 button depends on the width and on
    // being shown at all, so measure again whenever the list changes size.
    if (typeof ResizeObserver === "function") new ResizeObserver(() => this.fit()).observe(this.list);
    this.node = el(
      "section",
      { class: "draft", "aria-labelledby": titleId, tabindex: "-1", "data-state": "running" },
      el("div", { class: "draft-head" }, this.title, this.badge),
      this.steps,
      this.status,
      this.error,
      this.failedLine,
      this.fold,
    );
    this.draw(false);
  }

  /** Whether the card has stopped changing: `draft_done` came, or the answer ended. */
  get finished() {
    return this.state.done !== null || this.state.ended || this.unknown;
  }

  /** `draft_level`: the cells' text becomes this level's text. `live` is false for a replayed event. */
  level(event, live) {
    if (this.finished || !applyLevel(this.state, event)) return false;
    this.draw(live);
    return true;
  }

  /** `draft_done`: fold the card and show the summary. */
  done(event) {
    if (this.state.done || !applyDone(this.state, event)) return false;
    this.draw(false);
    this.fold.open = false;
    return true;
  }

  /**
   * The tool result came without `draft_done` (the event was older than the
   * 1,000 the server keeps, or this tab missed it): say so; a reload draws
   * the card from the session record.
   */
  lose() {
    if (this.finished) return;
    this.state.ended = true;
    this.state.lost = true;
    this.draw(false);
    this.fold.open = false;
  }

  /** Marks the card as ended when its answer ended and no `draft_done` came. */
  abandon() {
    if (this.finished) return;
    this.state.ended = true;
    this.draw(false);
  }

  /**
   * Draws the card from a session's `tool_result` block: the numbers and the
   * failed sections from its `draft` object only, the sections from the text
   * the server did not cut, and what it cut as a gap before the end.
   */
  fromHistory(block) {
    const status = resultStatus(block);
    if (status && applyDone(this.state, status)) {
      // The status counts the levels, not the seam level after them.
      this.state.level = this.state.done.levelsDone + (this.state.done.polished ? 1 : 0);
    } else {
      this.unknown = true;
    }
    const parts = resultText(block);
    const article = parseArticle(parts.head);
    if (article.title) this.state.title = article.title;
    const failedIds = this.state.done?.failedSections ?? [];
    this.state.sections = article.sections.map((s, i) => ({
      id: s.failedId ?? "",
      concept: s.concept,
      text: s.text,
      kind: null,
      level: 0,
      truncated: false,
      // A note in the text only places a failure the server's list names.
      failed: s.failedId !== null && failedIds.includes(s.failedId),
      key: i + 1,
    }));
    if (parts.omitted !== 0) this.gap = { omitted: parts.omitted, tail: parts.tail };
    this.draw(false);
    this.fold.open = false;
  }

  /** Measures the closed cells again. */
  fit() {
    for (const cell of this.cells.values()) fitCell(cell);
  }

  draw(live) {
    const state = this.state;
    const stop = state.done ? state.done.stop : state.ended ? "ended" : this.unknown ? "unknown" : "running";
    const [badgeText, tone] = BADGE[stop] ?? [stop, "neutral"];
    this.node.dataset.state = stop;
    this.badge.textContent = badgeText;
    this.badge.className = `badge badge-${tone} draft-badge`;
    this.title.textContent = state.title || "글 생성";

    const hideSteps = this.unknown || (state.level === 0 && state.done === null && state.ended);
    this.steps.hidden = hideSteps;
    this.steps.replaceChildren(
      ...steps(state).map((step) => {
        const note = STEP_NOTE[step.state];
        return el(
          "li",
          { class: `draft-step is-${step.state}`, "aria-current": step.state === "current" ? "step" : null },
          el("span", { class: "draft-step-text" }, stepText(step)),
          note ? el("span", { class: "sr-only" }, `, ${note}`) : null,
        );
      }),
    );

    const done = state.done;
    const note = done ? stopNote(done) : null;
    const text = this.unknown ? "서버가 이 결과의 상태를 보내지 않아 몇 단계까지 만들었는지, 호출 수와 걸린 시간은 알 수 없습니다." : progressText(state);
    // replaceChildren prints a null as the text "null": push only real nodes.
    const line = [];
    if (note) line.push(el("span", { class: `draft-stop draft-stop-${done.stop}` }, note), " ");
    line.push(el("span", null, text));
    this.status.replaceChildren(...line);
    const reason = done?.error ? `서버가 보낸 이유: ${done.error}` : "";
    this.error.textContent = reason;
    this.error.hidden = reason === "";

    const names = done ? done.failedSections : [];
    this.failedLine.textContent = names.length ? `실패한 절: ${names.join(", ")}. 앞 단계의 글로 대신했습니다.` : "";
    this.failedLine.hidden = names.length === 0;

    for (const section of state.sections) {
      const key = section.key ?? section.id;
      let cell = this.cells.get(key);
      if (!cell) {
        cell = makeCell(this.id, this.cells.size + 1);
        this.cells.set(key, cell);
        this.list.append(cell.node);
      }
      fillCell(cell, section, { failed: section.failed === true || sectionFailed(state, section.id), fresh: live });
    }

    this.gapSlot.replaceChildren(...this.gapView());

    const count = state.sections.length;
    const rows = done?.byLevel ?? [];
    this.tableSlot.replaceChildren(...(rows.length ? [this.table(rows)] : []));
    this.fold.hidden = count === 0 && rows.length === 0 && !this.gap;
    this.foldSummary.textContent = this.finished
      ? rows.length
        ? `절 ${formatInt(count)}개의 글과 단계별 호출 보기`
        : count === 0
          ? "글 보기"
          : `절 ${formatInt(count)}개의 글 보기`
      : `진행 중인 글, 절 ${formatInt(count)}개`;
    this.fit();
  }

  /** What follows the cells when the server cut the result: the omitted length, then the end it kept. */
  gapView() {
    if (!this.gap) return [];
    const { omitted, tail } = this.gap;
    const parts = [
      el(
        "p",
        { class: "draft-gap" },
        omitted === null ? "서버가 이 뒤의 글을 보내지 않았습니다." : `가운데 ${formatInt(omitted)}자 생략`,
      ),
    ];
    if (tail) {
      const id = `${this.id}-tail`;
      parts.push(
        el("p", { class: "draft-gap-label", id }, "글의 끝부분"),
        el("pre", { class: "draft-tail", tabindex: "0", "aria-labelledby": id }, tail),
      );
    }
    return parts;
  }

  table(rows) {
    return el(
      "table",
      { class: "draft-table" },
      el("caption", { class: "draft-table-caption" }, "단계별 호출"),
      el(
        "thead",
        null,
        el("tr", null, ...["단계", "호출", "실패", "입력 토큰", "출력 토큰"].map((name) => el("th", { scope: "col" }, name))),
      ),
      el(
        "tbody",
        null,
        rows.map((row) =>
          el(
            "tr",
            null,
            el("th", { scope: "row" }, levelRowLabel(row)),
            el("td", null, formatInt(row.calls)),
            el("td", null, row.failedCalls > 0 ? formatInt(row.failedCalls) : "0"),
            el("td", null, formatInt(row.inputTokens)),
            el("td", null, formatInt(row.outputTokens)),
          ),
        ),
      ),
    );
  }
}

/**
 * The card for a `draft` tool result found in a session's history, where
 * the events do not come again. `block` is the `tool_result` block, with the
 * `draft` status and the `text_tail` the session API adds.
 */
export function draftResultCard(idBase, block) {
  const card = new DraftCard(idBase, "");
  card.fromHistory(block);
  return card;
}
