// 세션 상세: one session's conversation, 100 messages at a time.

import { noticeLines } from "./checkpoints.js";
import { draftResultKeys } from "./draft.js";
import { code, el, notice, pathCode, timeEl } from "./dom.js";
import { formatDateTime, formatInt, parseTime } from "./format.js";
import { chatHash } from "./routes.js";
import { draftResultCard } from "./view-draft.js";

export const PAGE = 100;

const ROLES = { user: "사용자", assistant: "어시스턴트", system: "시스템" };

/**
 * Messages fetched with 더 보기, kept across refreshes of the first page:
 * `next` is the offset of the next page, `done` that a page came back empty.
 */
const more = { id: null, messages: [], next: 0, done: false };

/**
 * Where the page after `page` starts. Offsets count messages, like
 * `offset` in the request; the `index` of each message is not used for it.
 */
export function pageEnd(page, requested) {
  const start = Number.isInteger(page?.offset) ? page.offset : requested;
  return start + (Array.isArray(page?.messages) ? page.messages.length : 0);
}

/** Server-side blocks the dashboard does not show, by `kind`. */
const OTHER_KINDS = {
  server_tool_use: "서버 도구 호출",
  web_search_tool_result: "웹 검색 결과",
  web_fetch_tool_result: "웹 페이지 가져오기 결과",
};

/** The role as shown; a user turn made only of tool results reads as one. */
export function roleLabel(message) {
  const blocks = Array.isArray(message.blocks) ? message.blocks : [];
  if (message.role === "user" && blocks.length > 0 && blocks.every((b) => b.type === "tool_result")) {
    return "도구 결과";
  }
  return ROLES[message.role] ?? message.role ?? "알 수 없음";
}

/**
 * The first page and the pages added after it, in order and without
 * repeats. A refresh replaces the first page; later pages stay, because a
 * session journal only grows at its end.
 */
export function mergePages(first, later) {
  const seen = new Set(first.map((m) => m.index));
  const tail = later.filter((m) => !seen.has(m.index));
  return [...first, ...tail].sort((a, b) => a.index - b.index);
}

/** The first non-empty line, as a hint on a folded block. */
export function firstLine(text) {
  return (
    String(text ?? "")
      .split("\n")
      .map((line) => line.trim())
      .find((line) => line) ?? ""
  );
}

/**
 * Whether the server replaced a whole block with its `[hidden: ...]` line
 * (a block that looks like it holds a credential, as in `junees doctor`).
 * A `[hidden]` inside other text is left as it is.
 */
export function isHiddenBlock(text) {
  return /^\[hidden(: [^\]\n]*)?\]$/.test(String(text ?? "").trim());
}

const HIDDEN_NOTE = "비밀 값으로 보이는 내용이 있어 서버가 이 내용을 가렸습니다.";

/** The body of a folded tool block: the text, or a note when it was hidden. */
export function toolBody(text) {
  return isHiddenBlock(text)
    ? el("p", { class: "tool-body tool-hidden" }, HIDDEN_NOTE)
    : el("pre", { class: "tool-body" }, text ?? "");
}

/** The hint on a folded block's line. */
export function peek(text) {
  return isHiddenBlock(text) ? "가린 내용" : firstLine(text);
}

function cut(block) {
  return block.truncated
    ? el("span", { class: "badge badge-neutral", title: "서버가 긴 내용을 잘라서 보냈습니다." }, "잘림")
    : null;
}

function blockView(block, messageIndex, n, draftKeys) {
  // A stable id keeps a folded block open across a refresh.
  const id = `blk-${messageIndex}-${n}`;
  switch (block.type) {
    case "text":
      if (isHiddenBlock(block.text)) return el("p", { class: "msg-note" }, HIDDEN_NOTE);
      // What Junees put before the message after an undo, marked by the
      // server. Anything else is the person's, "[되돌림] " at its start too.
      if (block.notice === true) {
        return el(
          "div",
          { class: "undo-note" },
          el("p", { class: "undo-note-head" }, "되돌린 파일을 Junees에게 알린 내용"),
          noticeLines(block.text).map((line) => el("p", null, line)),
          // The server cuts a block past 8,000 characters, a notice too.
          block.truncated ? el("p", { class: "msg-cut" }, cut(block), " 긴 안내라 앞부분만 보여 줍니다.") : null,
        );
      }
      return el(
        "div",
        { class: "msg-text" },
        block.text ?? "",
        block.truncated ? el("p", { class: "msg-cut" }, cut(block), " 긴 글이라 앞부분만 보여 줍니다.") : null,
      );
    case "tool_use":
      return el(
        "details",
        { class: "tool", id },
        el(
          "summary",
          null,
          el(
            "span",
            { class: "tool-sum" },
            el("span", { class: "tool-kind" }, "도구 호출"),
            code(block.name ?? "-"),
            cut(block),
            el("span", { class: "tool-peek" }, peek(block.input_preview)),
          ),
        ),
        toolBody(block.input_preview),
      );
    case "tool_result":
      // The result of a `draft` call: the finished card, since its events are not sent again.
      if (draftKeys.has(`${messageIndex}:${n}`)) return draftResultCard(`draft-${messageIndex}-${n}`, block).node;
      return el(
        "details",
        { class: `tool${block.is_error ? " tool-error" : ""}`, id },
        el(
          "summary",
          null,
          el(
            "span",
            { class: "tool-sum" },
            el("span", { class: "tool-kind" }, block.is_error ? "오류" : "출력"),
            cut(block),
            el("span", { class: "tool-peek" }, peek(block.text_preview)),
          ),
        ),
        toolBody(block.text_preview),
        Array.isArray(block.images) && block.images.length
          ? el(
              "p",
              { class: "tool-images" },
              `이미지 ${block.images.length}개(${block.images.join(", ")})가 함께 있었습니다. 대시보드에는 이미지 내용을 보내지 않습니다.`,
            )
          : null,
      );
    case "image":
      return el("p", { class: "msg-note" }, `이미지${block.media_type ? ` (${block.media_type})` : ""}. 대시보드에는 이미지 내용을 보내지 않습니다.`);
    case "thinking":
      return el("p", { class: "msg-note" }, "생각 과정. 내용은 대시보드에 보내지 않습니다.");
    case "other":
      return el(
        "p",
        { class: "msg-note" },
        `${OTHER_KINDS[block.kind] ?? `기타 블록(${block.kind ?? "종류 없음"})`}. 내용은 대시보드에 보내지 않습니다.`,
      );
    default:
      return el("p", { class: "msg-note" }, `표시하지 않는 내용(${block.type ?? "알 수 없음"})`);
  }
}

const NO_DRAFTS = new Set();

/**
 * One message. `draftKeys` is {@link draftResultKeys} of the list it is in,
 * for the tool results that answer a `draft` call.
 */
export function messageView(message, now, draftKeys = NO_DRAFTS) {
  const at = parseTime(message.at);
  const role = roleLabel(message);
  const tone = role === "도구 결과" ? "tool" : message.role === "user" ? "user" : message.role === "assistant" ? "assistant" : "other";
  const headId = `msg-${message.index}`;
  const blocks = Array.isArray(message.blocks) ? message.blocks : [];
  return el(
    "li",
    { class: `msg msg-${tone}`, "aria-labelledby": headId, tabindex: "-1", "data-index": message.index },
    el(
      "p",
      { class: "msg-head", id: headId },
      el("span", { class: "msg-role" }, role),
      at ? timeEl(at, formatDateTime(at, now)) : null,
      el("span", { class: "msg-index" }, `${formatInt(message.index + 1)}번째`),
    ),
    blocks.length
      ? blocks.map((block, n) => {
          const node = blockView(block, message.index, n, draftKeys);
          // Marked, not left out: 도구 호출과 결과 숨기기 hides it in place.
          if (hiddenWithTools(block, `${message.index}:${n}`, draftKeys)) node.setAttribute("data-tool", "");
          return node;
        })
      : el("p", { class: "msg-note" }, "내용 없음"),
  );
}

/**
 * Whether a block is a tool call, a tool result or a server tool block (the
 * kinds the screen names, and any other kind named `..._tool_use` or
 * `..._tool_result`). Text, a Junees notice, an image, thinking and a kind
 * the screen does not know are not.
 */
export function isToolBlock(block) {
  const type = block?.type;
  if (type === "tool_use" || type === "tool_result") return true;
  if (type !== "other" || typeof block.kind !== "string") return false;
  return Object.hasOwn(OTHER_KINDS, block.kind) || /_tool_(use|result)$/.test(block.kind);
}

/**
 * Whether 도구 호출과 결과 숨기기 takes the block at `key`
 * (`"<message index>:<block number>"`) out. The result of a `draft` call stays:
 * its card is the text the person asked for, not a tool's output to the model.
 */
function hiddenWithTools(block, key, draftKeys) {
  return isToolBlock(block) && !draftKeys.has(key);
}

/**
 * The messages as read with tool calls and results hidden:
 * `{ messages, hiddenMessages, hiddenBlocks }`. A message keeps its other
 * blocks; one that no tool block was taken from is the same object. A message
 * is left out whole when nothing to read remains after its tool blocks: no
 * block at all, or only "생각 과정" lines, which carry no content. A message
 * without any tool block is never left out.
 */
export function conversationOnly(messages, draftKeys = NO_DRAFTS) {
  const kept = [];
  let hiddenMessages = 0;
  let hiddenBlocks = 0;
  for (const message of messages) {
    const blocks = Array.isArray(message?.blocks) ? message.blocks : [];
    const left = blocks.filter((block, n) => !hiddenWithTools(block, `${message.index}:${n}`, draftKeys));
    hiddenBlocks += blocks.length - left.length;
    if (left.length === blocks.length) kept.push(message);
    else if (left.every((block) => block?.type === "thinking")) hiddenMessages += 1;
    else kept.push({ ...message, blocks: left });
  }
  return { messages: kept, hiddenMessages, hiddenBlocks };
}

/** Added to the 찾기 lines while tool calls and results are hidden. */
export const HIDE_FIND_NOTE = "숨긴 도구 호출과 결과 안은 찾지 않습니다.";

/** The status line while tool calls and results are hidden and nothing is looked for. */
export function hideSummary(loaded, hiddenMessages, hiddenBlocks) {
  if (hiddenBlocks <= 0) return "불러온 메시지에 숨길 도구 호출이나 결과가 없습니다.";
  const blocks = `도구 호출과 결과 ${formatInt(hiddenBlocks)}개를 숨겼습니다.`;
  return hiddenMessages > 0
    ? `${blocks} 불러온 메시지 ${formatInt(loaded)}개 중 ${formatInt(hiddenMessages)}개는 남는 글이 없어 통째로 숨겼습니다.`
    : blocks;
}

/** The status line while tool calls and results are hidden and a query is in the input. */
export function hideFindSummary(loaded, hiddenMessages, matched) {
  return hiddenMessages > 0
    ? `불러온 메시지 ${formatInt(loaded)}개 중 도구 호출과 결과만 든 ${formatInt(hiddenMessages)}개를 뺀 ${formatInt(loaded - hiddenMessages)}개에서 ${formatInt(matched)}개를 찾았습니다.`
    : `도구 호출과 결과를 숨기고, 불러온 메시지 ${formatInt(loaded)}개 중 ${formatInt(matched)}개를 찾았습니다.`;
}

/** What to do when hiding tool calls and results leaves no message on screen. */
export function hideEmptyBody(left) {
  return left > 0
    ? "불러온 메시지가 모두 도구 호출이나 결과뿐입니다. 아래 더 보기로 메시지를 더 불러오거나 체크를 풀어 보세요."
    : "이 세션의 메시지가 모두 도구 호출이나 결과뿐입니다. 체크를 풀면 다시 볼 수 있습니다.";
}

/** The "back to the list" link shown next to the title. */
export function renderControls(root, ctx) {
  // replaceChildren would print a null as the text "null".
  const links = [];
  if (ctx?.chat === true && ctx.args?.id) links.push(el("a", { class: "button", href: chatHash(ctx.args.id) }, "이 세션 이어서 대화"));
  links.push(el("a", { class: "button button-secondary", href: "#/sessions" }, "세션 목록으로"));
  root.replaceChildren(...links);
}

const searchText = (text) => String(text ?? "").normalize("NFC").toLowerCase();

/** The query as compared: trimmed, any letter case. Empty means no 찾기. */
const needleOf = (query) => searchText(query).trim();

/**
 * The text of a message that 찾기 looks in: what the person and the model
 * wrote, a tool call's name and input, a tool result's first part and its
 * end. A part the server hid (a block, a result's first part or its end) is
 * left out: its `[hidden: ...]` line is the server's, not the message's.
 */
export function messageSearchText(message) {
  const parts = [];
  for (const block of Array.isArray(message?.blocks) ? message.blocks : []) {
    if (block?.type === "text") {
      if (!isHiddenBlock(block.text)) parts.push(block.text);
    } else if (block?.type === "tool_use") {
      parts.push(block.name);
      if (!isHiddenBlock(block.input_preview)) parts.push(block.input_preview);
    } else if (block?.type === "tool_result") {
      if (!isHiddenBlock(block.text_preview)) parts.push(block.text_preview);
      if (!isHiddenBlock(block.text_tail)) parts.push(block.text_tail);
    }
  }
  // A line break between parts, so a word never matches across two blocks.
  return parts.filter((part) => typeof part === "string" && part).join("\n");
}

/** Whether a message holds the query: a literal part of its text, any letter case. */
export function messageMatches(message, query) {
  const needle = needleOf(query);
  if (!needle) return true;
  return searchText(messageSearchText(message)).includes(needle);
}

/** The messages that hold the query, in their order; all of them for an empty query. */
export function findMessages(messages, query) {
  if (!needleOf(query)) return messages;
  return messages.filter((message) => messageMatches(message, query));
}

/** The line under the input: how far 찾기 reaches. `left` is the count not loaded yet. */
export function findScope(left) {
  return left > 0
    ? `이미 불러온 메시지에서만 찾습니다. 아직 불러오지 않은 ${formatInt(left)}개는 아래 더 보기로 불러온 뒤에 찾을 수 있습니다.`
    : "이 세션의 메시지를 모두 불러와서 전체에서 찾습니다.";
}

/** The result line while a query is in the input; empty without one. */
export function findSummary(query, loaded, matched) {
  if (!needleOf(query)) return "";
  return `불러온 메시지 ${formatInt(loaded)}개 중 ${formatInt(matched)}개를 찾았습니다.`;
}

/** What to do when nothing matched. */
export function findEmptyBody(left) {
  return left > 0
    ? "아래 더 보기로 메시지를 더 불러오거나 다른 낱말을 입력해 보세요."
    : "다른 낱말을 입력하거나 입력을 지워 보세요.";
}

// Only a still-mounted 찾기 of the same session is reused. Leaving the screen
// drops the query: it is kept nowhere but in the input.
const mountedFinds = new WeakMap();

function mountFind(root, id) {
  const input = el("input", {
    id: "session-find",
    type: "search",
    class: "session-filter-input",
    autocomplete: "off",
    spellcheck: "false",
    "aria-describedby": "session-find-scope",
  });
  const clear = el("button", { type: "button", class: "button button-secondary", disabled: true }, "지우기");
  // Off at first and kept nowhere but in this node, like the query.
  const toggle = el("input", { id: "session-hide-tools", type: "checkbox" });
  const scope = el("p", { class: "summary", id: "session-find-scope" });
  const status = el("p", { class: "summary", role: "status" });
  const headCard = el("div", { class: "card session-head" });
  const body = el("div");
  const find = el(
    "div",
    { class: "session-filter" },
    el("label", { for: input.id }, "불러온 메시지 안에서 찾기"),
    el("div", { class: "session-filter-fields" }, input, clear),
    scope,
    el("span", { class: "check-toggle" }, toggle, el("label", { for: toggle.id }, "도구 호출과 결과 숨기기")),
  );
  const shell = { id, input, clear, toggle, scope, status, headCard, body, paint: () => {} };
  input.addEventListener("input", () => shell.paint());
  toggle.addEventListener("change", () => shell.paint());
  clear.addEventListener("click", () => {
    input.value = "";
    input.focus();
    shell.paint();
  });
  mountedFinds.set(find, shell);
  root.replaceChildren(headCard, find, status, body);
  return shell;
}

export function render(root, data, ctx) {
  if (more.id !== data.id) {
    Object.assign(more, { id: data.id, messages: [], next: 0, done: false });
  }
  more.next = Math.max(more.next, pageEnd(data, 0));
  const first = Array.isArray(data.messages) ? data.messages : [];
  let messages = mergePages(first, more.messages);
  const total = Math.max(Number(data.total) || 0, messages.length);
  const now = ctx.now;
  ctx.setTitle(data.title || "제목 없음");

  const updated = parseTime(data.updated_at);
  const head = el(
    "dl",
    { class: "facts facts-inline" },
    el("dt", null, "폴더"),
    el("dd", null, pathCode(data.cwd)),
    el("dt", null, "갱신"),
    el("dd", null, updated ? timeEl(updated, formatDateTime(updated, now)) : "-"),
    el("dt", null, "메시지"),
    el("dd", null, `${formatInt(total)}개`),
    el("dt", null, "ID"),
    el("dd", null, code(data.id)),
  );

  if (total === 0) {
    root.replaceChildren(el("div", { class: "card" }, head), notice({ title: "이 세션에는 메시지가 없습니다" }));
    return;
  }

  // Keep the input node itself across refreshes, with its caret and IME state.
  const mounted = mountedFinds.get(root.children[1]);
  const shell = mounted?.id === data.id ? mounted : mountFind(root, data.id);
  const { input, clear, toggle, scope, status } = shell;

  const draftKeys = draftResultKeys(messages);
  // The draft results as the list drew them: a card on screen is never counted as hidden.
  const drawn = new Set(draftKeys);
  const list = el("ol", { class: "msgs", "aria-label": "대화" }, messages.map((m) => messageView(m, now, draftKeys)));
  const empty = el("div", { hidden: true });
  const count = el("p", { class: "summary", "aria-live": "polite" });
  const error = el("div", { class: "more-error", role: "alert", hidden: true });
  const button = el("button", { type: "button", class: "button button-secondary" }, "더 보기");
  const footer = el("div", { class: "more" }, count, error, button);

  const setText = (node, text) => {
    if (node.textContent !== text) node.textContent = text;
  };

  /**
   * Shows only the messages that hold the input's text, without their tool
   * blocks while the box is checked; returns those of `among` left on screen.
   */
  const applyFind = (among = []) => {
    const query = input.value;
    const hide = toggle.checked === true;
    // With tools hidden, 찾기 looks only in what stays on screen.
    const view = hide ? conversationOnly(messages, drawn) : { messages, hiddenMessages: 0, hiddenBlocks: 0 };
    const matched = new Set(findMessages(view.messages, query).map((m) => m.index));
    // Hiding instead of rebuilding keeps folded blocks and draft cards as they are.
    for (const item of list.children) {
      item.hidden = !matched.has(Number(item.getAttribute("data-index")));
      for (const part of item.children) if (part.hasAttribute("data-tool")) part.hidden = hide;
    }
    const left = more.done ? 0 : Math.max(0, total - more.next);
    const none = matched.size === 0;
    list.hidden = none;
    empty.hidden = !none;
    const nothing = [];
    if (none && hide && view.messages.length === 0) {
      nothing.push(notice({ title: "도구 호출과 결과를 숨기면 보일 메시지가 없습니다", body: hideEmptyBody(left) }));
    } else if (none) {
      const body = hide ? `${findEmptyBody(left)} ${HIDE_FIND_NOTE}` : findEmptyBody(left);
      nothing.push(notice({ title: "찾는 낱말이 든 메시지가 없습니다", body }));
    }
    empty.replaceChildren(...nothing);
    // The shared lines belong to the list on screen, not to one a refresh replaced.
    if (list.isConnected) {
      // One line for both controls, left in place and emptied, so a change is announced once.
      const summary = !hide
        ? findSummary(query, messages.length, matched.size)
        : needleOf(query)
          ? hideFindSummary(messages.length, view.hiddenMessages, matched.size)
          : hideSummary(messages.length, view.hiddenMessages, view.hiddenBlocks);
      setText(status, summary);
      setText(scope, hide ? `${findScope(left)} ${HIDE_FIND_NOTE}` : findScope(left));
      clear.disabled = !input.value;
    }
    return among.filter((m) => matched.has(m.index));
  };

  const update = () => {
    const left = more.done ? 0 : Math.max(0, total - more.next);
    count.textContent =
      left > 0
        ? `${formatInt(total)}개 중 ${formatInt(messages.length)}개를 보고 있습니다.`
        : `메시지 ${formatInt(messages.length)}개를 모두 보고 있습니다.`;
    button.hidden = left <= 0;
    button.textContent = left > 0 ? `${formatInt(Math.min(PAGE, left))}개 더 보기` : "더 보기";
  };

  button.addEventListener("click", async () => {
    // Twice while loading, or after the last page: nothing to ask for.
    if (button.disabled || button.hidden) return;
    const offset = more.next;
    button.disabled = true;
    button.textContent = "불러오는 중";
    error.hidden = true;
    try {
      const page = await ctx.fetchJson(`/api/sessions/${encodeURIComponent(data.id)}?offset=${offset}&limit=${PAGE}`);
      const got = Array.isArray(page.messages) ? page.messages : [];
      const shown = new Set(messages.map((m) => m.index));
      const added = got.filter((m) => !shown.has(m.index));
      more.next = Math.max(more.next, pageEnd(page, offset));
      // An empty page ends the list even if `total` says more: no endless button.
      if (got.length === 0) more.done = true;
      more.messages = mergePages(more.messages, added);
      messages = mergePages(messages, added);
      const pairs = draftResultKeys(messages);
      const fresh = new Set(added.map((m) => m.index));
      for (const key of pairs) if (fresh.has(Number(key.split(":")[0]))) drawn.add(key);
      list.append(...added.map((m) => messageView(m, now, pairs)));
      button.disabled = false;
      update();
      // The text being looked for applies to the new messages too.
      const finding = Boolean(needleOf(input.value));
      const visible = applyFind(added);
      // Keyboard and screen reader users land on the first new message on
      // screen; with none, on the button, or the input once the button is gone.
      if (visible.length) list.querySelector(`[data-index="${visible[0].index}"]`)?.focus();
      else if (added.length && list.isConnected) (button.hidden ? input : button).focus();
      if (list.isConnected) {
        const unseen = added.length - visible.length;
        ctx.announce(
          finding
            ? `메시지 ${formatInt(added.length)}개를 더 불러왔습니다. 그중 ${formatInt(visible.length)}개에 찾는 낱말이 있습니다.`
            : unseen > 0
              ? `메시지 ${formatInt(added.length)}개를 더 불러왔습니다. 그중 도구 호출과 결과만 든 ${formatInt(unseen)}개는 숨겼습니다.`
              : `메시지 ${formatInt(added.length)}개를 더 불러왔습니다.`,
        );
      }
    } catch (err) {
      button.disabled = false;
      update();
      if (ctx.isUnauthorized(err)) return;
      error.replaceChildren(el("p", null, `더 불러오지 못했습니다. ${ctx.describeError(err)}`));
      error.hidden = false;
    }
  });

  update();
  shell.headCard.replaceChildren(head);
  shell.body.replaceChildren(list, empty, footer);
  shell.paint = () => applyFind();
  applyFind();
}

/** Korean text for a failed first load of this screen. */
export function describeLoadError(err) {
  if (err?.kind === "http" && (err.status === 404 || err.status === 400)) {
    return "이 세션을 찾지 못했습니다. 지워졌거나 주소가 잘못됐을 수 있습니다.";
  }
  return null;
}
