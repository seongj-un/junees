// How to run: see the 테스트 section of web/dashboard/dev/README.md.
import assert from "node:assert/strict";
import { test } from "node:test";

import { DOM_GLOBALS, installDom, saveGlobals, visible } from "./fake-dom.mjs";
import {
  findEmptyBody,
  findMessages,
  findScope,
  findSummary,
  messageMatches,
  messageSearchText,
  render,
} from "../../view-session.js";

const text = (index, role, body) => ({ index, role, at: "2026-11-13T09:00:00Z", blocks: [{ type: "text", text: body }] });

const talk = Object.freeze([
  text(0, "user", "Dashboard 배포 가이드를 써 줘"),
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
  text(3, "assistant", "정리했습니다."),
  text(4, "user", "고마워. 한글 설명도 넣어 줘"),
]);

const indexes = (messages) => messages.map((m) => m.index);

test("an empty or whitespace-only query keeps every message in its order", () => {
  assert.equal(findMessages(talk, ""), talk);
  assert.equal(findMessages(talk, " \t "), talk);
  assert.equal(messageMatches(talk[3], ""), true);
});

test("a query matches a part of the text in any letter case", () => {
  assert.deepEqual(indexes(findMessages(talk, "ashbo")), [0]);
  assert.deepEqual(indexes(findMessages(talk, "  DASHBOARD ")), [0]);
  assert.deepEqual(indexes(findMessages(talk, "deploy")), [1]);
});

test("a query matches Korean as written, decomposed Hangul too", () => {
  assert.deepEqual(indexes(findMessages(talk, "배포")), [0, 2]);
  assert.deepEqual(indexes(findMessages(talk, "한글".normalize("NFD"))), [4]);
  assert.deepEqual(indexes(findMessages(talk, "가이드를 써")), [0]);
});

test("a query matches a tool call by name and input, a tool result by its first part and its end", () => {
  assert.deepEqual(indexes(findMessages(talk, "read_file")), [1]);
  assert.deepEqual(indexes(findMessages(talk, "docs/")), [1]);
  assert.deepEqual(indexes(findMessages(talk, "cargo build")), [2]);
  assert.deepEqual(indexes(findMessages(talk, "롤백")), [2]);
});

test("a query is literal text and looks only in what the screen shows", () => {
  assert.deepEqual(findMessages(talk, ".*"), []);
  assert.deepEqual(findMessages(talk, "없는 낱말"), []);
  // Roles, ids and field names are not message text.
  assert.deepEqual(findMessages(talk, "assistant"), []);
  assert.deepEqual(findMessages(talk, "t1"), []);
  // Two blocks never join into one match.
  assert.deepEqual(findMessages(talk, "읽겠습니다.read_file"), []);
  const unseen = {
    index: 9,
    role: "assistant",
    blocks: [
      { type: "thinking", text: "secret plan" },
      { type: "text", text: "[hidden: api key]" },
      { type: "tool_result", text_preview: "[hidden]" },
      { type: "tool_result", text_preview: "[hidden]", text_tail: "[hidden: it looks like it holds a credential]", omitted_chars: 40 },
      { type: "image", media_type: "image/png" },
    ],
  };
  assert.equal(messageSearchText(unseen), "");
  assert.equal(messageMatches(unseen, "hidden"), false);
  assert.equal(messageMatches(unseen, "credential"), false);
  // A hidden end leaves the first part findable, and the other way round.
  const hiddenEnd = { type: "tool_result", text_preview: "build log", text_tail: "[hidden: it looks like it holds a credential]", omitted_chars: 40 };
  const hiddenStart = { type: "tool_result", text_preview: "[hidden: it looks like it holds a credential]", text_tail: "exit 0", omitted_chars: 40 };
  const half = { index: 11, role: "user", blocks: [hiddenEnd, hiddenStart] };
  assert.equal(messageSearchText(half), "build log\nexit 0");
  assert.deepEqual(indexes(findMessages([half], "build log")), [11]);
  assert.deepEqual(indexes(findMessages([half], "exit 0")), [11]);
  assert.deepEqual(findMessages([half], "hidden"), []);
  assert.deepEqual(findMessages([half], "credential"), []);
  // A `[hidden]` inside a longer end is the tool's own text and stays findable.
  const quoted = { index: 12, role: "user", blocks: [{ type: "tool_result", text_preview: "a", text_tail: "token is [hidden], done" }] };
  assert.deepEqual(indexes(findMessages([quoted], "hidden")), [12]);
  assert.equal(messageMatches({ index: 10, role: "user" }, "x"), false);
});

test("the scope and result lines say how far the search reached", () => {
  assert.equal(findSummary("", 100, 100), "");
  assert.equal(findSummary("  ", 100, 100), "");
  assert.equal(findSummary("배포", 1200, 3), "불러온 메시지 1,200개 중 3개를 찾았습니다.");
  assert.equal(findSummary("배포", 100, 0), "불러온 메시지 100개 중 0개를 찾았습니다.");
  assert.match(findScope(130), /아직 불러오지 않은 130개는 아래 더 보기로/);
  assert.equal(findScope(0), "이 세션의 메시지를 모두 불러와서 전체에서 찾습니다.");
  assert.match(findEmptyBody(5), /더 보기/);
  assert.doesNotMatch(findEmptyBody(0), /더 보기/);
});

// Rendering. Each test uses its own session id: the screen keeps the pages
// loaded with 더 보기 per session.

// What a rendering test swaps on `globalThis`. Every test file runs in one
// process, so each test puts these back when it ends.
const SWAPPED = Object.freeze([...DOM_GLOBALS, "localStorage", "sessionStorage", "location", "history"]);
const swappedNow = () => SWAPPED.map((name) => Object.getOwnPropertyDescriptor(globalThis, name));

/** Installs the DOM stand-in for the test `t` and takes it out again after it. */
function dom(t) {
  t.after(saveGlobals(SWAPPED));
  return installDom();
}

function screen(t, id, { loaded = 3, total = talk.length } = {}) {
  const document = dom(t);
  const root = document.createElement("div");
  document.body.append(root);
  const calls = { fetched: [], announced: [], stored: 0 };
  // Any use of browser storage or the address by the screen is counted.
  for (const name of ["localStorage", "sessionStorage", "location", "history"]) {
    Object.defineProperty(globalThis, name, { configurable: true, get: () => ((calls.stored += 1), undefined) });
  }
  const ctx = {
    now: new Date("2026-11-13T09:10:00Z"),
    setTitle() {},
    announce: (line) => calls.announced.push(line),
    isUnauthorized: () => false,
    describeError: (err) => String(err),
    fetchJson: async (url) => {
      calls.fetched.push(url);
      const offset = Number(new URL(url, "http://x").searchParams.get("offset"));
      return { id, offset, total, messages: talk.slice(offset) };
    },
  };
  const data = { id, title: "배포 가이드", cwd: "/work/x", updated_at: "2026-11-13T09:05:00Z", total, messages: talk.slice(0, loaded) };
  const draw = () => render(root, data, ctx);
  draw();
  const shown = () => visible(root, "li").map((li) => Number(li.getAttribute("data-index")));
  const status = () => root.querySelector('[role="status"]').textContent;
  const more = () => visible(root, "button").find((b) => b.textContent.includes("더 보기")) ?? null;
  return { document, root, calls, draw, shown, status, more, input: root.querySelector("#session-find") };
}

test("the screen starts with a labelled, empty input and every loaded message", (t) => {
  const s = screen(t, "find-start");
  const label = s.root.querySelector("label");
  assert.equal(label.textContent, "불러온 메시지 안에서 찾기");
  assert.equal(label.getAttribute("for"), s.input.id);
  assert.equal(s.input.getAttribute("type"), "search");
  assert.equal(s.root.querySelector(`#${s.input.getAttribute("aria-describedby")}`).textContent, findScope(2));
  assert.equal(s.root.querySelector(".session-filter button").disabled, true);
  assert.deepEqual(s.shown(), [0, 1, 2]);
  assert.equal(s.status(), "");
  assert.equal(s.more().textContent, "2개 더 보기");
});

test("typing shows only the messages that match, and 지우기 brings the list back", async (t) => {
  const s = screen(t, "find-type");
  await s.input.type("배포");
  assert.deepEqual(s.shown(), [0, 2]);
  assert.equal(s.status(), "불러온 메시지 3개 중 2개를 찾았습니다.");
  await s.input.type("READ_file");
  assert.deepEqual(s.shown(), [1]);
  // The count line of 더 보기 still speaks of loaded messages.
  assert.equal(s.root.querySelector(".more .summary").textContent, "5개 중 3개를 보고 있습니다.");

  const clear = s.root.querySelector(".session-filter button");
  assert.equal(clear.disabled, false);
  await clear.click();
  assert.equal(s.input.value, "");
  assert.equal(s.document.activeElement, s.input);
  assert.deepEqual(s.shown(), [0, 1, 2]);
  assert.equal(s.status(), "");
  assert.equal(clear.disabled, true);
  assert.equal(s.calls.fetched.length, 0);
});

test("no match shows its own notice, not the empty-session one, and keeps 더 보기", async (t) => {
  const s = screen(t, "find-none");
  await s.input.type("한글");
  assert.deepEqual(s.shown(), []);
  assert.equal(s.status(), "불러온 메시지 3개 중 0개를 찾았습니다.");
  const [title] = visible(s.root, ".notice-title");
  assert.equal(title.textContent, "찾는 낱말이 든 메시지가 없습니다");
  assert.notEqual(title.textContent, "이 세션에는 메시지가 없습니다");
  assert.equal(visible(s.root, ".notice-body")[0].textContent, findEmptyBody(2));
  assert.equal(s.more().textContent, "2개 더 보기");

  await s.input.type("");
  assert.equal(visible(s.root, ".notice-title").length, 0);
  assert.deepEqual(s.shown(), [0, 1, 2]);
});

test("더 보기 applies the query to the new messages and sends it nowhere", async (t) => {
  const s = screen(t, "find-more");
  await s.input.type("한글");
  await s.more().click();
  assert.deepEqual(s.calls.fetched, ["/api/sessions/find-more?offset=3&limit=100"]);
  assert.deepEqual(s.shown(), [4]);
  assert.equal(s.status(), "불러온 메시지 5개 중 1개를 찾았습니다.");
  assert.equal(s.root.querySelector("#session-find-scope").textContent, findScope(0));
  assert.equal(s.more(), null);
  assert.equal(s.root.querySelector(".more .summary").textContent, "메시지 5개를 모두 보고 있습니다.");
  assert.deepEqual(s.calls.announced, ["메시지 2개를 더 불러왔습니다. 그중 1개에 찾는 낱말이 있습니다."]);
  // Focus lands on the first new message that is on screen.
  assert.equal(s.document.activeElement.getAttribute("data-index"), "4");
  assert.equal(s.calls.stored, 0);

  await s.input.type("");
  assert.deepEqual(s.shown(), [0, 1, 2, 3, 4]);
});

test("더 보기 without a match among the new messages keeps focus on a control", async (t) => {
  const s = screen(t, "find-more-none", { loaded: 2 });
  await s.input.type("dashboard");
  await s.more().click();
  assert.deepEqual(s.shown(), [0]);
  assert.deepEqual(s.calls.announced, ["메시지 3개를 더 불러왔습니다. 그중 0개에 찾는 낱말이 있습니다."]);
  // The button is gone with the last page, so the input takes focus.
  assert.equal(s.document.activeElement, s.input);
});

test("더 보기 without a query behaves as before", async (t) => {
  const s = screen(t, "find-more-plain");
  await s.more().click();
  assert.deepEqual(s.shown(), [0, 1, 2, 3, 4]);
  assert.deepEqual(s.calls.announced, ["메시지 2개를 더 불러왔습니다."]);
  assert.equal(s.document.activeElement.getAttribute("data-index"), "3");
  assert.equal(s.status(), "");
});

test("a refresh keeps the same input node mounted, with its value and focus, and filters the new list", async (t) => {
  const s = screen(t, "find-refresh");
  s.input.focus();
  await s.input.type("배포");
  const bar = s.root.querySelector(".session-filter");
  const before = s.root.querySelector("li");

  s.draw();
  assert.equal(s.root.querySelector("#session-find"), s.input);
  assert.equal(s.root.querySelectorAll("#session-find").length, 1);
  // Never taken out of the page: a re-inserted input loses focus, caret and
  // IME state. The stand-in has no caret, so the same node with zero detaches
  // is what stands for it here.
  assert.equal(bar.detached, 0);
  assert.equal(s.input.detached, 0);
  assert.equal(s.document.activeElement, s.input);
  assert.equal(s.input.value, "배포");
  // The list itself is new, and the query holds on it.
  assert.notEqual(s.root.querySelector("li"), before);
  assert.deepEqual(s.shown(), [0, 2]);
  assert.equal(s.status(), "불러온 메시지 3개 중 2개를 찾았습니다.");

  // Typing after the refresh filters the list on screen.
  await s.input.type("read_file");
  assert.deepEqual(s.shown(), [1]);
  assert.equal(s.calls.stored, 0);
  assert.equal(s.calls.fetched.length, 0);
});

test("a refresh after 더 보기 keeps the loaded pages and the query", async (t) => {
  const s = screen(t, "find-refresh-more");
  await s.more().click();
  await s.input.type("한글");
  s.draw();
  assert.deepEqual(s.shown(), [4]);
  assert.equal(s.status(), "불러온 메시지 5개 중 1개를 찾았습니다.");
});

test("another session starts with an empty input", async (t) => {
  const s = screen(t, "find-a");
  await s.input.type("배포");
  const root = s.root;
  render(
    root,
    { id: "find-b", title: "다른 세션", total: 1, messages: [text(0, "user", "배포 말고 다른 이야기")] },
    { now: new Date("2026-11-13T09:10:00Z"), setTitle() {} },
  );
  const input = root.querySelector("#session-find");
  assert.notEqual(input, s.input);
  assert.equal(input.value, "");
  assert.equal(root.querySelectorAll("#session-find").length, 1);
});

test("a session without messages has no input", (t) => {
  const document = dom(t);
  const root = document.createElement("div");
  document.body.append(root);
  render(root, { id: "find-empty", total: 0, messages: [] }, { now: new Date(), setTitle() {} });
  assert.equal(root.querySelector("#session-find"), null);
  assert.equal(root.querySelector(".notice-title").textContent, "이 세션에는 메시지가 없습니다");
});

test("a rendering test puts back the globals it swapped", async (t) => {
  const before = swappedNow();
  await t.test("while it runs, the stand-ins are in place", (inner) => {
    const s = screen(inner, "find-globals");
    assert.equal(globalThis.document, s.document);
    assert.notDeepEqual(swappedNow(), before);
  });
  assert.deepEqual(swappedNow(), before);
});
