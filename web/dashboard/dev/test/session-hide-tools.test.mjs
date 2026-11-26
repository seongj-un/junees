// How to run: see the 테스트 section of web/dashboard/dev/README.md.
//
// 도구 호출과 결과 숨기기 on the session screen: which blocks and messages
// stay, and what the screen shows with the box checked.
import assert from "node:assert/strict";
import { test } from "node:test";

import { DOM_GLOBALS, installDom, saveGlobals, visible } from "./fake-dom.mjs";
import { FAILED_SECTION_NOTE, draftResultKeys } from "../../draft.js";
import { draftResultCard } from "../../view-draft.js";
import {
  HIDE_FIND_NOTE,
  conversationOnly,
  draftCardText,
  findEmptyBody,
  findScope,
  findSummary,
  hideEmptyBody,
  hideFindSummary,
  hideSummary,
  isToolBlock,
  messageMatches,
  messageView,
  newCopyState,
  render,
} from "../../view-session.js";

const STATUS_LINE = "[draft] levels 3/3, 3 sections, failed sections: none, 9 calls";

const text = (index, role, body) => ({ index, role, blocks: [{ type: "text", text: body }] });

// With every message loaded and the box checked: 0, 1, 4, 6 and 7 stay,
// 2, 3 and 5 go whole, and 5 tool blocks are hidden.
const talk = Object.freeze([
  text(0, "user", "배포 가이드를 써 줘"),
  {
    index: 1,
    role: "assistant",
    blocks: [
      { type: "text", text: "먼저 파일을 읽겠습니다." },
      { type: "tool_use", name: "read_file", tool_use_id: "t1", input_preview: "docs/Deploy.md" },
    ],
  },
  {
    index: 2,
    role: "user",
    blocks: [{ type: "tool_result", tool_use_id: "t1", text_preview: "# 배포 절차\n1. cargo build", text_tail: "끝: 롤백 방법", omitted_chars: 40 }],
  },
  {
    index: 3,
    role: "assistant",
    blocks: [{ type: "thinking" }, { type: "other", kind: "server_tool_use" }, { type: "other", kind: "web_search_tool_result" }],
  },
  {
    index: 4,
    role: "user",
    blocks: [
      { type: "text", notice: true, text: "[되돌림] notes.md를 되돌렸습니다" },
      { type: "text", text: "한글 설명도 넣어 줘" },
    ],
  },
  { index: 5, role: "assistant", blocks: [{ type: "tool_use", name: "draft", tool_use_id: "d1", input_preview: '{"request":"안내문"}' }] },
  {
    index: 6,
    role: "user",
    blocks: [
      {
        type: "tool_result",
        tool_use_id: "d1",
        // As the server sends a result it read a status from: the `draft` object, and the
        // `---` and `[draft] ...` line Junees puts at the end of the text.
        text_preview: `# 배포 안내\n## 개요\n롤백 절차를 적은 본문\n---\n${STATUS_LINE}`,
        draft: { stop: "done", levels: 3, levels_done: 3, calls: 9 },
      },
    ],
  },
  text(7, "assistant", "정리했습니다."),
]);

const indexes = (messages) => messages.map((m) => m.index);

test("a tool block is a tool call, a tool result or a server tool kind, and nothing else", () => {
  assert.equal(isToolBlock({ type: "tool_use", name: "bash" }), true);
  assert.equal(isToolBlock({ type: "tool_result", text_preview: "ok" }), true);
  assert.equal(isToolBlock({ type: "other", kind: "server_tool_use" }), true);
  assert.equal(isToolBlock({ type: "other", kind: "web_search_tool_result" }), true);
  assert.equal(isToolBlock({ type: "other", kind: "web_fetch_tool_result" }), true);
  assert.equal(isToolBlock({ type: "other", kind: "code_execution_tool_result" }), true);
  assert.equal(isToolBlock({ type: "other", kind: "mcp_tool_use" }), true);

  assert.equal(isToolBlock({ type: "text", text: "tool_use" }), false);
  assert.equal(isToolBlock({ type: "text", notice: true, text: "[되돌림] a.txt" }), false);
  assert.equal(isToolBlock({ type: "thinking" }), false);
  assert.equal(isToolBlock({ type: "image", media_type: "image/png" }), false);
  assert.equal(isToolBlock({ type: "other", kind: "container_upload" }), false);
  assert.equal(isToolBlock({ type: "other" }), false);
  // A kind that is a name on every object is not a known kind.
  assert.equal(isToolBlock({ type: "other", kind: "constructor" }), false);
  assert.equal(isToolBlock({ type: "other", kind: "toString" }), false);
  assert.equal(isToolBlock(null), false);
  assert.equal(isToolBlock(undefined), false);
});

test("hiding tools keeps the text of a mixed message and drops a message with nothing left", () => {
  const view = conversationOnly(talk, draftResultKeys(talk));
  assert.deepEqual(indexes(view.messages), [0, 1, 4, 6, 7]);
  assert.equal(view.hiddenMessages, 3);
  assert.equal(view.hiddenBlocks, 5);
  // Untouched messages are the same objects; a trimmed one is a copy.
  assert.equal(view.messages[0], talk[0]);
  assert.equal(view.messages[2], talk[4]);
  assert.notEqual(view.messages[1], talk[1]);
  assert.deepEqual(view.messages[1].blocks, [{ type: "text", text: "먼저 파일을 읽겠습니다." }]);
  assert.equal(view.messages[1].role, "assistant");
  assert.equal(talk[1].blocks.length, 2);
});

test("hiding tools keeps a Junees notice and the result of a draft call", () => {
  const view = conversationOnly(talk, draftResultKeys(talk));
  assert.equal(view.messages[2].blocks[0].notice, true);
  // The draft result is the same message object: nothing of it was taken out.
  assert.equal(view.messages[3], talk[6]);
  // Its call is hidden like any other call.
  assert.equal(indexes(view.messages).includes(5), false);
  // A result that is not known to be a draft result is a plain tool result.
  const plain = conversationOnly(talk);
  assert.deepEqual(indexes(plain.messages), [0, 1, 4, 7]);
  assert.equal(plain.hiddenMessages, 4);
  assert.equal(plain.hiddenBlocks, 6);
});

test("hiding tools never drops a message that had no tool block", () => {
  const messages = [
    { index: 0, role: "assistant", blocks: [] },
    { index: 1, role: "user" },
    { index: 2, role: "assistant", blocks: [{ type: "thinking" }] },
    { index: 3, role: "assistant", blocks: [{ type: "thinking" }, { type: "tool_use", name: "bash" }] },
    { index: 4, role: "assistant", blocks: [{ type: "thinking" }, { type: "text", text: "답" }, { type: "tool_use", name: "bash" }] },
    { index: 5, role: "user", blocks: [{ type: "image", media_type: "image/png" }, { type: "tool_result", text_preview: "ok" }] },
    { index: 6, role: "assistant", blocks: [{ type: "other", kind: "container_upload" }] },
  ];
  const view = conversationOnly(messages);
  assert.deepEqual(indexes(view.messages), [0, 1, 2, 4, 5, 6]);
  assert.equal(view.hiddenMessages, 1);
  assert.equal(view.hiddenBlocks, 3);
  assert.deepEqual(
    view.messages[3].blocks.map((b) => b.type),
    ["thinking", "text"],
  );
  assert.deepEqual(
    view.messages[4].blocks.map((b) => b.type),
    ["image"],
  );
  assert.deepEqual(conversationOnly([]), { messages: [], hiddenMessages: 0, hiddenBlocks: 0 });
});

test("the status lines count what was hidden", () => {
  assert.equal(hideSummary(8, 3, 5), "도구 호출과 결과 5개를 숨겼습니다. 불러온 메시지 8개 중 3개는 남는 글이 없어 통째로 숨겼습니다.");
  assert.equal(hideSummary(8, 0, 2), "도구 호출과 결과 2개를 숨겼습니다.");
  assert.equal(hideSummary(8, 0, 0), "불러온 메시지에 숨길 도구 호출이나 결과가 없습니다.");
  assert.match(hideSummary(1200, 1000, 2500), /2,500개를 숨겼습니다\. 불러온 메시지 1,200개 중 1,000개는/);
  assert.equal(hideFindSummary(8, 3, 2), "불러온 메시지 8개 중 도구 호출과 결과만 든 3개를 뺀 5개에서 2개를 찾았습니다.");
  assert.equal(hideFindSummary(8, 0, 2), "도구 호출과 결과를 숨기고, 불러온 메시지 8개 중 2개를 찾았습니다.");
  assert.match(hideEmptyBody(3), /더 보기/);
  assert.doesNotMatch(hideEmptyBody(0), /더 보기/);
});

// Rendering. Each test uses its own session id: the screen keeps the pages
// loaded with 더 보기 per session.

// What a rendering test swaps on `globalThis`; each test puts these back.
const SWAPPED = Object.freeze([...DOM_GLOBALS, "localStorage", "sessionStorage", "location", "history"]);
const swappedNow = () => SWAPPED.map((name) => Object.getOwnPropertyDescriptor(globalThis, name));

const NOW = new Date("2026-11-13T09:10:00Z");

/** The text under `node` that is on screen: hidden elements are skipped. */
const shownText = (node) => ("data" in node ? node.data : node.hidden ? "" : node.childNodes.map(shownText).join(""));

function screen(t, id, { messages = talk, loaded = messages.length, total = messages.length } = {}) {
  t.after(saveGlobals(SWAPPED));
  const document = installDom();
  const root = document.createElement("div");
  document.body.append(root);
  const calls = { fetched: [], announced: [], stored: 0 };
  // Any use of browser storage or the address by the screen is counted.
  for (const name of ["localStorage", "sessionStorage", "location", "history"]) {
    Object.defineProperty(globalThis, name, { configurable: true, get: () => ((calls.stored += 1), undefined) });
  }
  const ctx = {
    now: NOW,
    setTitle() {},
    announce: (line) => calls.announced.push(line),
    isUnauthorized: () => false,
    describeError: (err) => String(err),
    fetchJson: async (url) => {
      calls.fetched.push(url);
      const offset = Number(new URL(url, "http://x").searchParams.get("offset"));
      return { id, offset, total, messages: messages.slice(offset) };
    },
  };
  const data = { id, title: "배포 가이드", cwd: "/work/x", updated_at: "2026-11-13T09:05:00Z", total, messages: messages.slice(0, loaded) };
  const draw = () => render(root, data, ctx);
  draw();
  const toggle = root.querySelector("#session-hide-tools");
  return {
    document,
    root,
    calls,
    draw,
    toggle,
    input: root.querySelector("#session-find"),
    // `.msg`, not `li`: a draft card has list items of its own.
    shown: () => visible(root, ".msg").map((li) => Number(li.getAttribute("data-index"))),
    status: () => root.querySelector("#session-find-status").textContent,
    scope: () => root.querySelector("#session-find-scope").textContent,
    count: () => root.querySelector(".more .summary").textContent,
    more: () => visible(root, "button").find((b) => b.textContent.includes("더 보기")) ?? null,
    /** The text on screen of the message at `index`. */
    text: (index) => shownText(root.querySelector(`[data-index="${index}"]`)),
    /** What a click on the box does: flip it, then fire `change`. */
    check: (on) => {
      toggle.checked = on;
      return toggle.dispatch("change");
    },
  };
}

test("the box is a labelled checkbox that starts off, and the list is as before", (t) => {
  const s = screen(t, "hide-start");
  assert.equal(s.toggle.getAttribute("type"), "checkbox");
  assert.equal(s.toggle.checked, false);
  const label = s.root.querySelector(".check-toggle label");
  assert.equal(label.textContent, "도구 호출과 결과 숨기기");
  assert.equal(label.getAttribute("for"), s.toggle.id);
  assert.equal(s.root.querySelectorAll("#session-hide-tools").length, 1);

  assert.deepEqual(s.shown(), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.equal(s.status(), "");
  assert.equal(s.scope(), findScope(0));
  // Every message reads as the message view alone draws it, and no block is hidden.
  const keys = draftResultKeys(talk);
  for (const message of talk) {
    assert.equal(s.text(message.index), shownText(messageView(message, NOW, keys, newCopyState())));
  }
  assert.equal(s.root.querySelectorAll("[data-tool]").length, 5);
  assert.equal(visible(s.root, "[data-tool]").length, 5);
  assert.equal(visible(s.root, ".draft").length, 1);
});

test("checking the box hides tool blocks and emptied messages in place, and unchecking brings them back", async (t) => {
  const s = screen(t, "hide-toggle");
  const firstItem = s.root.querySelector("li");
  const fold = s.root.querySelector("#blk-1-1");
  fold.open = true;
  const countLine = s.count();

  await s.check(true);
  assert.deepEqual(s.shown(), [0, 1, 4, 6, 7]);
  assert.equal(visible(s.root, "[data-tool]").length, 0);
  // A mixed message keeps its text only.
  assert.match(s.text(1), /먼저 파일을 읽겠습니다\./);
  assert.doesNotMatch(s.text(1), /read_file|Deploy/);
  // The Junees notice and the text after it stay.
  assert.match(s.text(4), /되돌린 파일을 Junees에게 알린 내용/);
  assert.match(s.text(4), /notes\.md를 되돌렸습니다/);
  assert.match(s.text(4), /한글 설명도 넣어 줘/);
  // The draft result card stays; the draft call went with its message.
  assert.equal(visible(s.root, ".draft").length, 1);
  assert.match(s.text(6), /롤백 절차를 적은 본문/);
  // One status line says what was hidden; the other lines keep their numbers.
  assert.equal(s.status(), "도구 호출과 결과 5개를 숨겼습니다. 불러온 메시지 8개 중 3개는 남는 글이 없어 통째로 숨겼습니다.");
  assert.equal(s.scope(), `${findScope(0)} ${HIDE_FIND_NOTE}`);
  assert.equal(s.count(), countLine);
  assert.deepEqual(s.calls.announced, []);

  await s.check(false);
  assert.deepEqual(s.shown(), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.equal(visible(s.root, "[data-tool]").length, 5);
  assert.equal(s.status(), "");
  assert.equal(s.scope(), findScope(0));
  assert.match(s.text(1), /read_file/);
  // Nothing was rebuilt: the same nodes, and the block opened before is still open.
  assert.equal(s.root.querySelector("li"), firstItem);
  assert.equal(firstItem.detached, 0);
  assert.equal(s.root.querySelector("#blk-1-1"), fold);
  assert.equal(fold.open, true);
  assert.equal(fold.hidden, false);
  assert.deepEqual(s.calls.announced, []);
  assert.equal(s.calls.fetched.length, 0);
  assert.equal(s.calls.stored, 0);
});

test("a session without tool blocks says there is nothing to hide", async (t) => {
  const messages = [text(0, "user", "안녕"), text(1, "assistant", "안녕하세요.")];
  const s = screen(t, "hide-nothing", { messages });
  await s.check(true);
  assert.deepEqual(s.shown(), [0, 1]);
  assert.equal(s.status(), "불러온 메시지에 숨길 도구 호출이나 결과가 없습니다.");
});

test("when every loaded message is hidden a notice takes the place of the list and 더 보기 stays", async (t) => {
  const messages = [
    { index: 0, role: "user", blocks: [{ type: "tool_result", text_preview: "ok" }] },
    { index: 1, role: "assistant", blocks: [{ type: "thinking" }, { type: "tool_use", name: "bash", input_preview: "ls" }] },
  ];
  const s = screen(t, "hide-all", { messages, total: 5 });
  await s.check(true);
  assert.deepEqual(s.shown(), []);
  const [title] = visible(s.root, ".notice-title");
  assert.equal(title.textContent, "도구 호출과 결과를 숨기면 보일 메시지가 없습니다");
  assert.equal(visible(s.root, ".notice-body")[0].textContent, hideEmptyBody(3));
  assert.equal(s.status(), "도구 호출과 결과 2개를 숨겼습니다. 불러온 메시지 2개 중 2개는 남는 글이 없어 통째로 숨겼습니다.");
  assert.equal(s.more().textContent, "3개 더 보기");

  // A query does not turn it into the no-match notice: there is nothing to look in.
  await s.input.type("ok");
  assert.equal(visible(s.root, ".notice-title")[0].textContent, "도구 호출과 결과를 숨기면 보일 메시지가 없습니다");

  await s.input.type("");
  await s.check(false);
  assert.equal(visible(s.root, ".notice-title").length, 0);
  assert.deepEqual(s.shown(), [0, 1]);
});

test("with the box checked 찾기 looks only in what is on screen", async (t) => {
  const s = screen(t, "hide-find");
  await s.check(true);

  // Words that are only in a hidden call or result find nothing.
  // The status line of the kept draft card is cut from the card, so it is not found either.
  for (const word of ["read_file", "docs/", "cargo build", "롤백 방법", "draft", "[draft]", "levels 3/3", "9 calls", "failed sections"]) {
    await s.input.type(word);
    assert.deepEqual(s.shown(), [], word);
  }
  assert.equal(s.status(), "불러온 메시지 8개 중 도구 호출과 결과만 든 3개를 뺀 5개에서 0개를 찾았습니다.");
  assert.equal(visible(s.root, ".notice-title")[0].textContent, "찾는 낱말이 든 메시지가 없습니다");
  assert.equal(visible(s.root, ".notice-body")[0].textContent, `${findEmptyBody(0)} ${HIDE_FIND_NOTE}`);

  // Text, a notice and the kept draft result are still found.
  await s.input.type("배포");
  assert.deepEqual(s.shown(), [0, 6]);
  assert.equal(s.status(), "불러온 메시지 8개 중 도구 호출과 결과만 든 3개를 뺀 5개에서 2개를 찾았습니다.");
  await s.input.type("되돌렸습니다");
  assert.deepEqual(s.shown(), [4]);
  // A mixed message found by its text still shows no tool block.
  await s.input.type("읽겠습니다");
  assert.deepEqual(s.shown(), [1]);
  assert.doesNotMatch(s.text(1), /read_file/);

  // Unchecked, the same query reaches the tool blocks again, with the old wording.
  await s.input.type("read_file");
  assert.deepEqual(s.shown(), []);
  await s.check(false);
  assert.deepEqual(s.shown(), [1]);
  assert.equal(s.status(), findSummary("read_file", 8, 1));
  assert.match(s.text(1), /read_file/);
  await s.input.type("배포");
  assert.deepEqual(s.shown(), [0, 2, 6]);
});

test("with the box checked a draft card is searched as it draws, and unchecked as the server sent it", async (t) => {
  const s = screen(t, "hide-find-draft");
  await s.check(true);
  // The card shows the article, not the status line the server read its numbers from.
  assert.doesNotMatch(s.text(6), /\[draft\]|levels 3\/3/);
  await s.input.type("롤백 절차");
  assert.deepEqual(s.shown(), [6]);
  await s.input.type("[draft]");
  assert.deepEqual(s.shown(), []);

  // Unchecked, the old search is as before: it reads the text the server sent.
  await s.check(false);
  assert.deepEqual(s.shown(), [6]);
  assert.equal(s.status(), findSummary("[draft]", 8, 1));
});

test("with the box checked a cut draft result is searched only in the part the card draws", async (t) => {
  const messages = [
    { index: 0, role: "assistant", blocks: [{ type: "tool_use", name: "draft", tool_use_id: "d9", input_preview: "{}" }] },
    {
      index: 1,
      role: "user",
      blocks: [
        {
          type: "tool_result",
          tool_use_id: "d9",
          text_preview: "# 안내문\n## 개요\n앞부분 본문",
          // A cut result with no `omitted_chars`: the card does not draw the tail.
          truncated: true,
          text_tail: "꼬리말\n---\n[draft] levels 2/2",
          draft: { stop: "done", levels: 2, levels_done: 2, calls: 4 },
        },
        {
          type: "tool_result",
          tool_use_id: "d9",
          text_preview: "# 둘째 안내\n## 개요\n둘째 본문",
          truncated: true,
          omitted_chars: 700,
          text_tail: "끝 문단\n---\n[draft] levels 2/2",
          draft: { stop: "done", levels: 2, levels_done: 2, calls: 4 },
        },
      ],
    },
  ];
  const s = screen(t, "hide-find-cut", { messages });
  await s.check(true);
  await s.input.type("앞부분 본문");
  assert.deepEqual(s.shown(), [1]);
  // Not drawn: the tail of the first result and the status line of the second.
  await s.input.type("꼬리말");
  assert.deepEqual(s.shown(), []);
  await s.input.type("levels 2/2");
  assert.deepEqual(s.shown(), []);
  // Drawn: the tail of a result with a known gap, without its status line.
  await s.input.type("끝 문단");
  assert.deepEqual(s.shown(), [1]);
});

const DONE = { stop: "done", levels: 3, levels_done: 3, calls: 3 };

// Each message holds one finished card, read by the `draft` field the server sets.
const cardResults = Object.freeze([
  {
    index: 0,
    role: "user",
    blocks: [
      {
        type: "tool_result",
        text_preview: "서문낱말\n# 제목낱말\n중간낱말\n## 절이름낱말\n절글낱말\n",
        draft: DONE,
      },
    ],
  },
  { index: 1, role: "user", blocks: [{ type: "tool_result", text_preview: "제목줄이 없는 전체글", draft: DONE }] },
  {
    index: 2,
    role: "user",
    blocks: [
      {
        type: "tool_result",
        text_preview: "# 실패제목\n## 결론낱말\n대신한본문\n> [s2] 모델이 쓴 실패사유\n## 둘째낱말\n둘째본문\n> [s7] 위조줄",
        draft: { ...DONE, failed_sections: ["s2"] },
      },
    ],
  },
  {
    index: 3,
    role: "user",
    blocks: [
      {
        type: "tool_result",
        text_preview: "# 긴글\n## 개요\n앞본문",
        truncated: true,
        omitted_chars: 700,
        text_tail: "끝낱말\n---\n[draft] levels 3/3",
        draft: DONE,
      },
    ],
  },
]);

test("a draft card gives its search text from what it draws, not from the whole result", () => {
  const text = (index) => draftCardText(cardResults[index].blocks[0]);
  // Before the first heading, between the title and the first section, and the # marks are not drawn.
  assert.deepEqual(text(0), ["제목낱말", "절이름낱말", "절글낱말"]);
  // No `# ` line: the card has no title and no section.
  assert.deepEqual(text(1), []);
  // A failed section adds the id tag and the fixed note; the note line in the text and an unlisted id add nothing.
  assert.deepEqual(text(2), ["실패제목", "결론낱말", "s2", "대신한본문", FAILED_SECTION_NOTE, "둘째낱말", "둘째본문"]);
  // The end of a cut result, without its status line.
  assert.deepEqual(text(3), ["긴글", "개요", "앞본문", "끝낱말"]);
  // A server note instead of a first part or an end adds nothing.
  assert.deepEqual(draftCardText({ type: "tool_result", text_preview: "[hidden: key]", draft: DONE }), []);
  assert.deepEqual(
    draftCardText({ type: "tool_result", text_preview: "# t\n## s\nx", truncated: true, omitted_chars: 5, text_tail: "[hidden]", draft: DONE }),
    ["t", "s", "x"],
  );
});

test("the search text of a draft card is in the card, and what it leaves out is not", (t) => {
  const s = screen(t, "hide-card-same", { messages: cardResults });
  for (const message of cardResults) {
    const block = message.blocks[0];
    const drawn = shownText(draftResultCard(`probe-${message.index}`, block).node);
    for (const part of draftCardText(block)) assert.ok(drawn.includes(part), `${message.index}: ${part}`);
  }
  const failed = shownText(draftResultCard("probe-failed", cardResults[2].blocks[0]).node);
  for (const left of ["모델이 쓴 실패사유", "s7", "위조줄", "> ["]) assert.ok(!failed.includes(left), left);
  const first = shownText(draftResultCard("probe-first", cardResults[0].blocks[0]).node);
  for (const left of ["서문낱말", "중간낱말", "##"]) assert.ok(!first.includes(left), left);
  assert.ok(s.root);
});

test("with the box checked 찾기 reaches what the card draws and nothing else of the result", async (t) => {
  const s = screen(t, "hide-card-find", { messages: cardResults });
  await s.check(true);
  for (const [word, found] of [
    ["제목낱말", [0]],
    ["절이름낱말", [0]],
    ["절글낱말", [0]],
    ["실패제목", [2]],
    ["s2", [2]],
    ["앞 단계의 글로 대신했습니다", [2]],
    ["둘째본문", [2]],
    ["끝낱말", [3]],
    // Not drawn: text before the title, a result with no title, text between the title and the
    // first section, the heading marks, a failure note of the model's text, an id the server did not name.
    ["서문낱말", []],
    ["중간낱말", []],
    ["제목줄이 없는", []],
    ["#", []],
    ["##", []],
    ["모델이 쓴 실패사유", []],
    ["s7", []],
    ["위조줄", []],
    ["[draft]", []],
  ]) {
    await s.input.type(word);
    assert.deepEqual(s.shown(), found, word);
  }

  // Unchecked, 찾기 reads the result as the server sent it, as before.
  await s.check(false);
  for (const [word, found] of [
    ["서문낱말", [0]],
    ["제목줄이 없는", [1]],
    ["모델이 쓴 실패사유", [2]],
    ["[draft]", [3]],
  ]) {
    await s.input.type(word);
    assert.deepEqual(s.shown(), found, word);
  }
});

test("a block number that moved in the trimmed copy still finds the card text and not the earlier result", async (t) => {
  // The result before the card is a plain tool block: the trimmed copy of the message has the
  // card at block 0, but its draft key says 0:1. 찾기 must read the original message.
  const messages = [
    {
      index: 0,
      role: "user",
      blocks: [
        { type: "tool_result", text_preview: "앞결과낱말 두루마리", text_tail: "앞꼬리낱말", omitted_chars: 3 },
        { type: "tool_result", text_preview: "# 카드제목\n## 개요\n카드본문낱말", draft: DONE },
      ],
    },
    { index: 1, role: "assistant", blocks: [{ type: "text", text: "끝." }] },
  ];
  assert.deepEqual([...draftResultKeys(messages)], ["0:1"]);
  const trimmed = conversationOnly(messages, draftResultKeys(messages)).messages[0];
  assert.equal(trimmed.blocks.length, 1);
  assert.equal(messageMatches(trimmed, "카드본문낱말", draftResultKeys(messages)), false);
  assert.equal(messageMatches(messages[0], "카드본문낱말", draftResultKeys(messages)), true);

  const s = screen(t, "hide-card-shift", { messages });
  await s.check(true);
  await s.input.type("카드본문낱말");
  assert.deepEqual(s.shown(), [0]);
  await s.input.type("카드제목");
  assert.deepEqual(s.shown(), [0]);
  for (const word of ["앞결과낱말", "앞꼬리낱말", "두루마리"]) {
    await s.input.type(word);
    assert.deepEqual(s.shown(), [], word);
  }
  // Unchecked, the earlier result is found like any tool block.
  await s.check(false);
  assert.deepEqual(s.shown(), [0]);
});

test("a block kind named like an object property reads as an unknown kind", (t) => {
  const kinds = ["constructor", "toString", "__proto__", "hasOwnProperty"];
  const messages = [...kinds, "web_search_tool_result"].map((kind, index) => ({ index, role: "assistant", blocks: [{ type: "other", kind }] }));
  const s = screen(t, "hide-kinds", { messages });
  kinds.forEach((kind, index) => {
    assert.match(s.text(index), new RegExp(`기타 블록\\(${kind}\\)\\. 내용은 대시보드에 보내지 않습니다\\.`), kind);
  });
  assert.match(s.text(4), /웹 검색 결과\. 내용은/);
});

test("더 보기 keeps the box checked, hides the tools of the new messages and sends the state nowhere", async (t) => {
  const s = screen(t, "hide-more", { loaded: 3 });
  await s.check(true);
  assert.deepEqual(s.shown(), [0, 1]);
  assert.equal(s.status(), "도구 호출과 결과 2개를 숨겼습니다. 불러온 메시지 3개 중 1개는 남는 글이 없어 통째로 숨겼습니다.");
  assert.equal(s.scope(), `${findScope(5)} ${HIDE_FIND_NOTE}`);

  await s.more().click();
  assert.deepEqual(s.calls.fetched, ["/api/sessions/hide-more?offset=3&limit=100"]);
  assert.equal(s.root.querySelector("#session-hide-tools"), s.toggle);
  assert.equal(s.toggle.checked, true);
  assert.deepEqual(s.shown(), [0, 1, 4, 6, 7]);
  assert.equal(visible(s.root, "[data-tool]").length, 0);
  // The draft result that came with the new page is a card, and counted as shown.
  assert.equal(visible(s.root, ".draft").length, 1);
  assert.equal(s.status(), "도구 호출과 결과 5개를 숨겼습니다. 불러온 메시지 8개 중 3개는 남는 글이 없어 통째로 숨겼습니다.");
  assert.deepEqual(s.calls.announced, ["메시지 5개를 더 불러왔습니다. 그중 도구 호출과 결과만 든 2개는 숨겼습니다."]);
  // Focus lands on the first new message that is on screen, not on a hidden one.
  assert.equal(s.document.activeElement.getAttribute("data-index"), "4");
  assert.equal(s.more(), null);
  assert.equal(s.calls.stored, 0);

  // A refresh after it keeps the pages and the state.
  s.draw();
  assert.equal(s.toggle.checked, true);
  assert.deepEqual(s.shown(), [0, 1, 4, 6, 7]);
});

test("a refresh keeps the same box mounted, checked and focused, with the query, and hides tools in the new list", async (t) => {
  const s = screen(t, "hide-refresh");
  s.toggle.focus();
  await s.check(true);
  await s.input.type("배포");
  const before = s.root.querySelector("li");
  const status = s.status();

  s.draw();
  assert.equal(s.root.querySelector("#session-hide-tools"), s.toggle);
  assert.equal(s.root.querySelectorAll("#session-hide-tools").length, 1);
  assert.equal(s.toggle.detached, 0);
  assert.equal(s.input.detached, 0);
  assert.equal(s.document.activeElement, s.toggle);
  assert.equal(s.toggle.checked, true);
  assert.equal(s.input.value, "배포");
  // The list is new; the box and the query hold on it.
  assert.notEqual(s.root.querySelector("li"), before);
  assert.deepEqual(s.shown(), [0, 6]);
  assert.equal(visible(s.root, "[data-tool]").length, 0);
  assert.equal(s.status(), status);

  // The box acts on the list now on screen.
  await s.input.type("");
  assert.deepEqual(s.shown(), [0, 1, 4, 6, 7]);
  await s.check(false);
  assert.deepEqual(s.shown(), [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.deepEqual(s.calls.announced, []);
  assert.equal(s.calls.fetched.length, 0);
  assert.equal(s.calls.stored, 0);
});

test("another session starts with the box off", async (t) => {
  const s = screen(t, "hide-a");
  await s.check(true);
  render(
    s.root,
    { id: "hide-b", title: "다른 세션", total: 2, messages: talk.slice(1, 3) },
    { now: NOW, setTitle() {} },
  );
  const toggle = s.root.querySelector("#session-hide-tools");
  assert.notEqual(toggle, s.toggle);
  assert.equal(toggle.checked, false);
  assert.equal(s.root.querySelectorAll("#session-hide-tools").length, 1);
  assert.deepEqual(s.shown(), [1, 2]);
  assert.equal(visible(s.root, "[data-tool]").length, 2);
  assert.equal(s.status(), "");
});

test("a hide-tools rendering test puts back the globals it swapped", async (t) => {
  const before = swappedNow();
  await t.test("while it runs, the stand-ins are in place", (inner) => {
    const s = screen(inner, "hide-globals");
    assert.equal(globalThis.document, s.document);
    assert.notDeepEqual(swappedNow(), before);
  });
  assert.deepEqual(swappedNow(), before);
});
