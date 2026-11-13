// 세션 상세: one session's conversation, 100 messages at a time.

import { code, el, notice, pathCode, timeEl } from "./dom.js";
import { formatDateTime, formatInt, parseTime } from "./format.js";

export const PAGE = 100;

const ROLES = { user: "사용자", assistant: "어시스턴트", system: "시스템" };

/** Messages fetched with 더 보기, kept across refreshes of the first page. */
const more = { id: null, messages: [] };

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

function cut(block) {
  return block.truncated
    ? el("span", { class: "badge badge-neutral", title: "서버가 긴 내용을 잘라서 보냈습니다." }, "잘림")
    : null;
}

function blockView(block, messageIndex, n) {
  // A stable id keeps a folded block open across a refresh.
  const id = `blk-${messageIndex}-${n}`;
  switch (block.type) {
    case "text":
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
            el("span", { class: "tool-peek" }, firstLine(block.input_preview)),
          ),
        ),
        el("pre", { class: "tool-body" }, block.input_preview ?? ""),
      );
    case "tool_result":
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
            el("span", { class: "tool-peek" }, firstLine(block.text_preview)),
          ),
        ),
        el("pre", { class: "tool-body" }, block.text_preview ?? ""),
      );
    case "image":
      return el("p", { class: "msg-note" }, `이미지${block.media_type ? ` (${block.media_type})` : ""}. 대시보드에는 이미지 내용을 보내지 않습니다.`);
    case "thinking":
      return el("p", { class: "msg-note" }, "생각 과정. 내용은 대시보드에 보내지 않습니다.");
    default:
      return el("p", { class: "msg-note" }, `표시하지 않는 내용(${block.type ?? "알 수 없음"})`);
  }
}

function messageView(message, now) {
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
      ? blocks.map((block, n) => blockView(block, message.index, n))
      : el("p", { class: "msg-note" }, "내용 없음"),
  );
}

/** The "back to the list" link shown next to the title. */
export function renderControls(root) {
  root.replaceChildren(el("a", { class: "button button-secondary", href: "#/sessions" }, "세션 목록으로"));
}

export function render(root, data, ctx) {
  if (more.id !== data.id) {
    more.id = data.id;
    more.messages = [];
  }
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

  const list = el("ol", { class: "msgs", "aria-label": "대화" }, messages.map((m) => messageView(m, now)));
  const count = el("p", { class: "summary", "aria-live": "polite" });
  const error = el("div", { class: "more-error", role: "alert", hidden: true });
  const button = el("button", { type: "button", class: "button button-secondary" }, "더 보기");
  const footer = el("div", { class: "more" }, count, error, button);

  const update = () => {
    const left = total - messages.length;
    count.textContent =
      left > 0
        ? `${formatInt(total)}개 중 ${formatInt(messages.length)}개를 보고 있습니다.`
        : `메시지 ${formatInt(total)}개를 모두 보고 있습니다.`;
    button.hidden = left <= 0;
    button.textContent = left > 0 ? `${formatInt(Math.min(PAGE, left))}개 더 보기` : "더 보기";
  };

  button.addEventListener("click", async () => {
    const offset = messages.length ? messages[messages.length - 1].index + 1 : 0;
    button.disabled = true;
    button.textContent = "불러오는 중";
    error.hidden = true;
    try {
      const page = await ctx.fetchJson(`/api/sessions/${encodeURIComponent(data.id)}?offset=${offset}&limit=${PAGE}`);
      const added = (Array.isArray(page.messages) ? page.messages : []).filter((m) => m.index >= offset);
      more.messages = mergePages(more.messages, added);
      messages = mergePages(messages, added);
      list.append(...added.map((m) => messageView(m, now)));
      button.disabled = false;
      update();
      // Keyboard and screen reader users land on the first new message.
      if (added.length) list.querySelector(`[data-index="${added[0].index}"]`)?.focus();
      ctx.announce(`메시지 ${formatInt(added.length)}개를 더 불러왔습니다.`);
    } catch (err) {
      button.disabled = false;
      update();
      if (ctx.isUnauthorized(err)) return;
      error.replaceChildren(el("p", null, `더 불러오지 못했습니다. ${ctx.describeError(err)}`));
      error.hidden = false;
    }
  });

  update();
  root.replaceChildren(el("div", { class: "card session-head" }, head), list, footer);
}

/** Korean text for a failed first load of this screen. */
export function describeLoadError(err) {
  if (err?.kind === "http" && (err.status === 404 || err.status === 400)) {
    return "이 세션을 찾지 못했습니다. 지워졌거나 주소가 잘못됐을 수 있습니다.";
  }
  return null;
}
