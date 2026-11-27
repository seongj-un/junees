// How to run: see the 테스트 section of web/dashboard/dev/README.md.
//
// 대화 복사 on the session screen: one button copies the words of every message
// that is on screen and not hidden, as Markdown, and the result line says what
// is not in it.
import assert from "node:assert/strict";
import { test } from "node:test";

import { DOM_GLOBALS, installDom, saveGlobals, visible } from "./fake-dom.mjs";
import {
  COPY_ALL_ID,
  COPY_ALL_STATUS_ID,
  conversationCopyEmpty,
  conversationCopyResult,
  conversationCopyText,
  messageCopyText,
  render,
} from "../../view-session.js";

const NOW = new Date("2026-11-13T09:10:00Z");
/** What the screen draws in place of a block the server hid (HIDDEN_NOTE in view-session.js). */
const HIDDEN = "비밀 값으로 보이는 내용이 있어 서버가 이 내용을 가렸습니다.";

const talk = [
  { index: 0, role: "user", blocks: [{ type: "text", text: "배포 가이드를 써 줘" }] },
  {
    index: 1,
    role: "assistant",
    blocks: [
      { type: "text", text: "먼저 파일을 읽겠습니다." },
      { type: "tool_use", name: "read_file", input_preview: "a.txt", tool_use_id: "t1" },
      { type: "text", text: "읽었습니다." },
    ],
  },
  { index: 2, role: "user", blocks: [{ type: "tool_result", tool_use_id: "t1", text_preview: "파일 내용", is_error: false }] },
  { index: 3, role: "assistant", blocks: [{ type: "text", text: "[hidden: api key]" }, { type: "text", text: "나머지 설명" }] },
  { index: 4, role: "assistant", blocks: [{ type: "text", text: "긴 답변의 앞부분", truncated: true }] },
  { index: 5, role: "assistant", blocks: [{ type: "tool_use", name: "bash", input_preview: "ls", tool_use_id: "t2" }] },
  {
    index: 6,
    role: "user",
    blocks: [{ type: "text", text: "[되돌림] 변경 기록 1을 되돌렸습니다.", notice: true }, { type: "text", text: "다시 해 줘" }],
  },
  { index: 7, role: "assistant", blocks: [{ type: "thinking" }] },
  { index: 8, role: "assistant", blocks: [{ type: "text", text: "   " }] },
  { index: 9, role: "assistant", blocks: [] },
];

/** The Markdown for the messages of `talk` that have words, in order. */
const ALL = [
  "### 1. 사용자\n\n배포 가이드를 써 줘",
  "### 2. 어시스턴트\n\n먼저 파일을 읽겠습니다.\n\n읽었습니다.",
  `### 4. 어시스턴트\n\n${HIDDEN}\n\n나머지 설명`,
  "### 5. 어시스턴트\n\n긴 답변의 앞부분",
  "### 7. 사용자\n\n다시 해 줘",
].join("\n\n");
const CUT = "서버가 긴 글을 잘라서 보냈으므로 잘린 글은 온 만큼만 들어 있습니다.";

// The text, without a screen.

test("messages with words become headed sections, numbered as the screen numbers them", () => {
  assert.deepEqual(conversationCopyText(talk), { text: ALL, count: 5, truncated: true });
  const two = conversationCopyText([talk[0], talk[1]]);
  assert.deepEqual(two, {
    text: "### 1. 사용자\n\n배포 가이드를 써 줘\n\n### 2. 어시스턴트\n\n먼저 파일을 읽겠습니다.\n\n읽었습니다.",
    count: 2,
    truncated: false,
  });
});

test("the number is the original message number, not the place in the list given", () => {
  assert.equal(conversationCopyText([talk[6]]).text, "### 7. 사용자\n\n다시 해 줘");
  assert.equal(conversationCopyText([{ index: 1233, role: "assistant", blocks: [{ type: "text", text: "x" }] }]).text, "### 1,234. 어시스턴트\n\nx");
});

test("each section's words are the ones the message's own copy gives", () => {
  for (const message of talk) {
    const own = messageCopyText(message);
    const all = conversationCopyText([message]);
    assert.equal(all.text, own.text === null ? null : `### ${message.index + 1}. ${message.role === "user" ? "사용자" : "어시스턴트"}\n\n${own.text}`);
    assert.equal(all.truncated, own.truncated);
  }
});

test("a message without words is skipped, and so are tool calls, results, notices and hidden originals", () => {
  for (const index of [2, 5, 7, 8, 9]) assert.deepEqual(conversationCopyText([talk[index]]), { text: null, count: 0, truncated: false }, index);
  const text = conversationCopyText(talk).text;
  for (const left of ["read_file", "bash", "파일 내용", "되돌림", "api key"]) assert.equal(text.includes(left), false, left);
  assert.ok(text.includes(HIDDEN), "a hidden block is the note the screen shows");
  assert.equal(text.includes("### 3."), false, "the tool result message has no section");
  // Not a list, or nothing in it: no text at all, never an empty string.
  for (const bad of [[], null, undefined, "x", [null, 3, {}]]) assert.equal(conversationCopyText(bad).text, null);
});

test("the words are not changed: their own Markdown and headings stay as they are", () => {
  const body = "# 제목\n- 항목\n```js\nconst a = 1;\n```\n### 3. 사용자\n<b>굵게</b>";
  const out = conversationCopyText([{ index: 0, role: "user", blocks: [{ type: "text", text: body }] }]);
  assert.equal(out.text, `### 1. 사용자\n\n${body}`);
});

test("only words and headings come out: no id, folder, time or cost", () => {
  const rich = {
    index: 0,
    role: "user",
    at: "2026-11-13T09:00:00Z",
    session_id: "sess-secret",
    cwd: "/home/x/private",
    cost: 1.5,
    blocks: [{ type: "text", text: "안녕" }],
  };
  assert.equal(conversationCopyText([rich]).text, "### 1. 사용자\n\n안녕");
});

test("the truncated flag is on only when a shown message was cut", () => {
  assert.equal(conversationCopyText([talk[0], talk[1]]).truncated, false);
  assert.equal(conversationCopyText([talk[0], talk[4]]).truncated, true);
});

test("the result lines say what was copied and what was left out", () => {
  const info = { count: 3, truncated: false };
  assert.equal(conversationCopyResult("ok", info, { left: 0, filtered: 0 }), "메시지 3개의 글을 Markdown으로 복사했습니다.");
  assert.equal(
    conversationCopyResult("ok", info, { left: 6, filtered: 0 }),
    "메시지 3개의 글을 Markdown으로 복사했습니다. 아직 불러오지 않은 메시지 6개는 들어 있지 않습니다.",
  );
  assert.equal(
    conversationCopyResult("ok", info, { left: 0, filtered: 9 }),
    "메시지 3개의 글을 Markdown으로 복사했습니다. 찾기에 걸리지 않은 메시지 9개는 들어 있지 않습니다.",
  );
  assert.equal(
    conversationCopyResult("ok", { count: 1200, truncated: true }, { left: 2, filtered: 1 }),
    `메시지 1,200개의 글을 Markdown으로 복사했습니다. 찾기에 걸리지 않은 메시지 1개는 들어 있지 않습니다. 아직 불러오지 않은 메시지 2개는 들어 있지 않습니다. ${CUT}`,
  );
  assert.equal(conversationCopyResult("denied", info, { left: 6, filtered: 0 }), "대화를 복사하지 못했습니다. 화면의 글을 직접 선택해 복사해 주세요.");
  assert.equal(conversationCopyResult("missing", info, { left: 0, filtered: 0 }), "이 브라우저에서는 복사 기능을 쓸 수 없습니다. 화면의 글을 직접 선택해 복사해 주세요.");
  assert.match(conversationCopyEmpty({ left: 0, filtered: 0 }), /^지금 화면에 복사할 글이 있는 메시지가 없어 아무것도 복사하지 않았습니다\.$/);
  assert.match(conversationCopyEmpty({ left: 4, filtered: 2 }), /찾기에 걸리지 않은 메시지는 복사 대상이 아닙니다\..*불러오지 않은 메시지 4개는 더 보기로 불러온 뒤에 복사할 수 있습니다\.$/);
});

// The screen. Each test uses its own session id: the screen keeps the pages
// loaded with 더 보기 per session.

const SWAPPED = Object.freeze([...DOM_GLOBALS, "localStorage", "sessionStorage", "location", "history", "navigator", "fetch"]);

/**
 * `clipboard`: `"works"` records what is written, `"refuses"` rejects,
 * `"throws"` throws at once, `"none"` leaves `navigator` without the API, a
 * function is `writeText` itself.
 */
function screen(t, id, { clipboard = "works", loaded = talk.length, list = talk } = {}) {
  t.after(saveGlobals(SWAPPED));
  const document = installDom();
  const root = document.createElement("div");
  document.body.append(root);
  const calls = { fetched: [], announced: [], stored: 0, network: 0, copied: [] };
  for (const name of ["localStorage", "sessionStorage", "location", "history"]) {
    Object.defineProperty(globalThis, name, { configurable: true, get: () => ((calls.stored += 1), undefined) });
  }
  Object.defineProperty(globalThis, "fetch", { configurable: true, value: () => void (calls.network += 1) });
  const writeText =
    typeof clipboard === "function"
      ? clipboard
      : {
          works: async (text) => void calls.copied.push(text),
          refuses: async () => {
            throw new Error("NotAllowedError");
          },
          throws: () => {
            throw new Error("SecurityError");
          },
        }[clipboard];
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: clipboard === "none" ? {} : { clipboard: { writeText } } });
  const ctx = {
    now: NOW,
    setTitle() {},
    announce: (line) => calls.announced.push(line),
    isUnauthorized: () => false,
    describeError: (err) => String(err),
    fetchJson: async (url) => {
      calls.fetched.push(url);
      const offset = Number(new URL(url, "http://x").searchParams.get("offset"));
      return { id, offset, total: list.length, messages: list.slice(offset) };
    },
  };
  const data = { id, title: "배포 가이드", cwd: "/work/x", updated_at: "2026-11-13T09:05:00Z", total: list.length, messages: list.slice(0, loaded) };
  const draw = (over = {}) => render(root, { ...data, ...over }, ctx);
  draw();
  return {
    document,
    root,
    calls,
    draw,
    input: root.querySelector("#session-find"),
    toggle: root.querySelector("#session-hide-tools"),
    button: () => root.querySelector(`#${COPY_ALL_ID}`),
    result: () => root.querySelector(`#${COPY_ALL_STATUS_ID}`).textContent,
    shown: () => visible(root, ".msg").map((li) => Number(li.getAttribute("data-index"))),
    more: () => visible(root, "button").find((b) => b.textContent.includes("더 보기")) ?? null,
    check: (on) => {
      const box = root.querySelector("#session-hide-tools");
      box.checked = on;
      return box.dispatch("change");
    },
  };
}

/** The Markdown for some of the messages of `talk`, by their numbers. */
const sections = (...indexes) => ALL.split("\n\n### ").map((part, at) => (at === 0 ? part : `### ${part}`)).filter((part) => indexes.includes(Number(part.match(/^### (\d+)\./)[1]) - 1)).join("\n\n");

test("the button and its result line have fixed ids, a name, and are mounted from the start", (t) => {
  const s = screen(t, "cc-render");
  const button = s.button();
  assert.equal(button.tagName, "BUTTON");
  assert.equal(button.getAttribute("type"), "button");
  assert.equal(button.id, "session-copy-conversation");
  assert.equal(button.textContent, "대화 복사");
  assert.equal(button.disabled, false);
  const line = s.root.querySelector("#session-copy-conversation-status");
  assert.equal(line.tagName, "OUTPUT");
  assert.equal(line.getAttribute("for"), button.id);
  assert.equal(line.textContent, "", "empty and mounted from the start");
  assert.equal(s.root.querySelectorAll("#session-copy-conversation").length, 1);
  // The screen's one status attribute is still the 찾기 line, found by its id.
  assert.equal(s.root.querySelector('[role="status"]').id, "session-find-status");
  // The button is in the controls block, not inside a message.
  assert.equal(s.root.querySelector(".session-filter").contains(button), true);
  assert.equal(s.root.querySelector(".msgs").contains(button), false);
});

test("a session with no messages has no button", (t) => {
  const s = screen(t, "cc-empty", { list: [] });
  assert.equal(s.button(), null);
  assert.equal(s.root.querySelector("#session-copy-conversation-status"), null);
});

test("a press copies every message on screen as Markdown and sends nothing anywhere", async (t) => {
  const s = screen(t, "cc-copy");
  await s.button().click();
  assert.deepEqual(s.calls.copied, [ALL]);
  assert.equal(s.result(), `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`);
  assert.deepEqual(s.calls.fetched, []);
  assert.equal(s.calls.network, 0);
  assert.equal(s.calls.stored, 0);
  assert.deepEqual(s.calls.announced, []);
});

test("messages not loaded yet are not copied, and the line says how many are left", async (t) => {
  const s = screen(t, "cc-left", { loaded: 4 });
  await s.button().click();
  assert.deepEqual(s.calls.copied, [sections(0, 1, 3)]);
  assert.equal(s.result(), "메시지 3개의 글을 Markdown으로 복사했습니다. 아직 불러오지 않은 메시지 6개는 들어 있지 않습니다.");
  // After 더 보기 the rest is in, and the line no longer mentions unloaded messages.
  await s.more().click();
  await s.button().click();
  assert.deepEqual(s.calls.copied.at(-1), ALL);
  assert.equal(s.result(), `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`);
});

test("with a search on, only the messages left on screen are copied, and the line says so", async (t) => {
  const s = screen(t, "cc-find");
  await s.input.type("읽었습니다");
  assert.deepEqual(s.shown(), [1]);
  await s.button().click();
  assert.deepEqual(s.calls.copied, [sections(1)]);
  assert.equal(s.result(), "메시지 1개의 글을 Markdown으로 복사했습니다. 찾기에 걸리지 않은 메시지 9개는 들어 있지 않습니다.");
  // A search that keeps the cut message adds the cut note.
  await s.input.type("긴 답변");
  assert.deepEqual(s.shown(), [4]);
  await s.button().click();
  assert.deepEqual(s.calls.copied.at(-1), sections(4));
  assert.equal(s.result(), `메시지 1개의 글을 Markdown으로 복사했습니다. 찾기에 걸리지 않은 메시지 9개는 들어 있지 않습니다. ${CUT}`);
  // Clearing the search copies everything again.
  await s.input.type("");
  await s.button().click();
  assert.deepEqual(s.calls.copied.at(-1), ALL);
  assert.equal(s.result().includes("찾기"), false);
});

test("a search and unloaded messages are both named, search first", async (t) => {
  const s = screen(t, "cc-find-left", { loaded: 4 });
  await s.input.type("나머지");
  assert.deepEqual(s.shown(), [3]);
  await s.button().click();
  assert.deepEqual(s.calls.copied, [sections(3)]);
  assert.equal(
    s.result(),
    "메시지 1개의 글을 Markdown으로 복사했습니다. 찾기에 걸리지 않은 메시지 3개는 들어 있지 않습니다. 아직 불러오지 않은 메시지 6개는 들어 있지 않습니다.",
  );
});

test("with tool calls hidden the same words are copied, and the search counts only what is on screen", async (t) => {
  const s = screen(t, "cc-hide");
  await s.check(true);
  assert.deepEqual(s.shown(), [0, 1, 3, 4, 6, 7, 8, 9]);
  await s.button().click();
  assert.deepEqual(s.calls.copied, [ALL]);
  assert.equal(s.result(), `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`, "tool-only messages hidden by the box are not named as left out");
  await s.input.type("읽었습니다");
  assert.deepEqual(s.shown(), [1]);
  await s.button().click();
  assert.deepEqual(s.calls.copied.at(-1), sections(1));
  assert.equal(s.result(), "메시지 1개의 글을 Markdown으로 복사했습니다. 찾기에 걸리지 않은 메시지 7개는 들어 있지 않습니다.");
});

// One screen per subtest: each registers its own restore of the globals.
test("with nothing on screen to copy the line says so and nothing is written", async (t) => {
  await t.test("a search that finds nothing", async (inner) => {
    const s = screen(inner, "cc-nothing");
    await s.input.type("없는낱말");
    assert.deepEqual(s.shown(), []);
    await s.button().click();
    assert.deepEqual(s.calls.copied, []);
    assert.equal(s.result(), "지금 화면에 복사할 글이 있는 메시지가 없어 아무것도 복사하지 않았습니다. 찾기에 걸리지 않은 메시지는 복사 대상이 아닙니다.");
  });
  await t.test("a session of tool blocks only", async (inner) => {
    const tools = screen(inner, "cc-tools-only", { list: [talk[2], talk[5]] });
    await tools.button().click();
    assert.deepEqual(tools.calls.copied, []);
    assert.equal(tools.result(), "지금 화면에 복사할 글이 있는 메시지가 없어 아무것도 복사하지 않았습니다.");
  });
  await t.test("with some left to load it says how to get them", async (inner) => {
    const part = screen(inner, "cc-tools-left", { list: [talk[2], talk[5], ...talk.slice(6)], loaded: 2 });
    await part.button().click();
    assert.deepEqual(part.calls.copied, []);
    assert.match(part.result(), /아직 불러오지 않은 메시지 \d+개는 더 보기로 불러온 뒤에 복사할 수 있습니다\.$/);
  });
});

test("pressing with nothing to copy twice empties the line before it is written again", async (t) => {
  const s = screen(t, "cc-nothing-twice", { list: [talk[2], talk[5]] });
  const words = "지금 화면에 복사할 글이 있는 메시지가 없어 아무것도 복사하지 않았습니다.";
  await s.button().click();
  assert.equal(s.result(), words);
  const second = s.button().click();
  assert.equal(s.result(), "", "emptied at the press, not refilled in the same turn");
  await Promise.resolve();
  assert.equal(s.result(), "");
  await second;
  assert.equal(s.result(), words);
  assert.deepEqual(s.calls.copied, []);
});

test("a refused or failed copy says so and points to copying by hand", async (t) => {
  for (const mode of ["refuses", "throws"]) {
    await t.test(mode, async (inner) => {
      const s = screen(inner, `cc-fail-${mode}`, { clipboard: mode });
      await s.button().click();
      assert.equal(s.result(), "대화를 복사하지 못했습니다. 화면의 글을 직접 선택해 복사해 주세요.");
      assert.equal(s.calls.stored, 0);
    });
  }
});

test("without a clipboard API the line says so, and a second press empties it first", async (t) => {
  const s = screen(t, "cc-missing", { clipboard: "none" });
  const words = "이 브라우저에서는 복사 기능을 쓸 수 없습니다. 화면의 글을 직접 선택해 복사해 주세요.";
  await s.button().click();
  assert.equal(s.result(), words);
  const second = s.button().click();
  assert.equal(s.result(), "", "emptied at the press, not refilled in the same turn");
  await Promise.resolve();
  assert.equal(s.result(), "", "still empty after the microtasks");
  await second;
  assert.equal(s.result(), words);
});

test("pressing again with the same result empties the line before the clipboard is asked", async (t) => {
  const seen = [];
  let s = null;
  s = screen(t, "cc-twice", { clipboard: async () => void seen.push(s.result()) });
  await s.button().click();
  await s.button().click();
  assert.deepEqual(seen, ["", ""]);
  assert.equal(s.result(), `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`);
});

test("a result for an older press is dropped when a newer press was made", async (t) => {
  const waits = [];
  const s = screen(t, "cc-overlap", { clipboard: () => new Promise((resolve, reject) => waits.push({ resolve, reject })) });
  const first = s.button().click();
  const second = s.button().click();
  waits[1].resolve();
  await second;
  const done = `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`;
  assert.equal(s.result(), done);
  waits[0].reject(new Error("NotAllowedError"));
  await first;
  assert.equal(s.result(), done);
});

test("a refresh keeps the same button, its focus and the result just shown", async (t) => {
  const s = screen(t, "cc-refresh");
  const before = s.button();
  before.focus();
  await before.click();
  const line = s.result();
  assert.notEqual(line, "");
  s.draw();
  assert.equal(s.button(), before, "the button is the same node, not rebuilt");
  assert.equal(before.detached, 0);
  assert.equal(s.document.activeElement, before);
  assert.deepEqual(before.focusCalls, [null], "the refresh did not call focus again, so it cannot scroll the page");
  assert.equal(s.result(), line, "the line just shown is not wiped");
  assert.equal(s.root.querySelectorAll("#session-copy-conversation").length, 1);
  assert.equal(s.root.querySelectorAll("#session-copy-conversation-status").length, 1);
});

test("a press after a refresh copies the list as it is then", async (t) => {
  const s = screen(t, "cc-refresh-new");
  await s.button().click();
  assert.deepEqual(s.calls.copied, [ALL]);
  const edited = talk.map((m) => (m.index === 0 ? { ...m, blocks: [{ type: "text", text: "다른 글" }] } : m));
  s.draw({ messages: edited });
  await s.button().click();
  assert.deepEqual(s.calls.copied.at(-1), ALL.replace("배포 가이드를 써 줘", "다른 글"));
  assert.equal(s.result(), `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`);
});

test("a refresh with a search and the box checked keeps the button, the result and the scope", async (t) => {
  const s = screen(t, "cc-refresh-find");
  await s.check(true);
  await s.input.type("읽었습니다");
  s.button().focus();
  await s.button().click();
  const line = s.result();
  s.draw();
  assert.deepEqual(s.shown(), [1]);
  assert.equal(s.document.activeElement, s.button());
  assert.equal(s.result(), line);
  assert.equal(s.input.value, "읽었습니다");
  assert.equal(s.toggle.checked, true);
  await s.button().click();
  assert.deepEqual(s.calls.copied, [sections(1), sections(1)]);
});

test("typing in the search box or checking the box keeps the button's focus and result", async (t) => {
  const s = screen(t, "cc-keep");
  const button = s.button();
  button.focus();
  await button.click();
  const line = s.result();
  await s.input.type("배포");
  assert.equal(s.document.activeElement, button);
  assert.equal(s.result(), line);
  await s.check(true);
  assert.equal(s.document.activeElement, button);
  assert.equal(s.result(), line);
  assert.equal(s.button(), button);
  assert.equal(button.detached, 0);
  assert.deepEqual(button.focusCalls, [null]);
});

test("더 보기 keeps the button and its result, and the new messages are in the next copy", async (t) => {
  const s = screen(t, "cc-more", { loaded: 4 });
  const button = s.button();
  button.focus();
  await button.click();
  const line = s.result();
  await s.more().click();
  assert.deepEqual(s.calls.fetched, ["/api/sessions/cc-more?offset=4&limit=100"]);
  assert.equal(s.button(), button);
  assert.equal(button.detached, 0);
  assert.equal(s.result(), line);
  await button.click();
  assert.deepEqual(s.calls.copied.at(-1), ALL);
  assert.equal(s.calls.stored, 0);
});

test("an answer that comes after a refresh lands on the line now on screen", async (t) => {
  let release;
  const s = screen(t, "cc-late", { clipboard: () => new Promise((resolve) => (release = resolve)) });
  const press = s.button().click();
  s.draw();
  release();
  await press;
  assert.equal(s.result(), `메시지 5개의 글을 Markdown으로 복사했습니다. ${CUT}`);
  assert.equal(s.root.querySelectorAll("#session-copy-conversation-status").length, 1);
});

test("another session gets its own button: it copies only its words, starts with an empty line, and an old answer never reaches it", async (t) => {
  const copied = [];
  const waits = [];
  // The first copy (session A) waits for the test; later ones answer at once.
  const s = screen(t, "cc-switch-a", {
    clipboard: (text) => {
      copied.push(text);
      return copied.length === 1 ? new Promise((resolve) => waits.push(resolve)) : Promise.resolve();
    },
  });
  const buttonA = s.button();
  const pressA = buttonA.click();
  assert.equal(copied.length, 1);
  assert.equal(copied[0], ALL);
  // Session B is drawn in the same place while A's copy is still out.
  const listB = [
    { index: 0, role: "user", blocks: [{ type: "text", text: "B의 글" }] },
    { index: 1, role: "assistant", blocks: [{ type: "text", text: "B의 답" }] },
  ];
  s.draw({ id: "cc-switch-b", title: "다른 세션", total: 2, messages: listB });
  const buttonB = s.button();
  assert.notEqual(buttonB, buttonA, "a new session mounts its own button");
  assert.equal(s.root.contains(buttonA), false);
  assert.equal(s.root.querySelectorAll("#session-copy-conversation").length, 1);
  assert.equal(s.result(), "", "B starts with an empty result line");
  await buttonB.click();
  assert.deepEqual(copied.at(-1), "### 1. 사용자\n\nB의 글\n\n### 2. 어시스턴트\n\nB의 답", "only B's words, nothing of A");
  assert.equal(copied.at(-1).includes("배포 가이드"), false);
  const lineB = "메시지 2개의 글을 Markdown으로 복사했습니다.";
  assert.equal(s.result(), lineB);
  // A's answer comes late: it does not touch B's line.
  waits[0]();
  await pressA;
  assert.equal(s.result(), lineB);
  assert.equal(s.root.querySelectorAll("#session-copy-conversation-status").length, 1);
});

test("the stylesheet never hides the result line with display: none", async (t) => {
  const { readFileSync } = await import("node:fs");
  const css = readFileSync(new URL("../../style.css", import.meta.url), "utf8");
  // Every rule whose selector names the line: none may set display: none (it would leave the accessibility tree while empty).
  const rules = [...css.matchAll(/([^{}]*session-copy-all-status[^{}]*)\{([^}]*)\}/g)];
  assert.ok(rules.length >= 1, "the line has a rule");
  for (const [, selector, body] of rules) assert.equal(/display\s*:\s*none/.test(body), false, selector.trim());
  assert.equal(/session-copy-all-status:empty/.test(css), false);
});

test("the copied words are never put in markup: the line is set as text", async (t) => {
  const evil = [{ index: 0, role: "user", blocks: [{ type: "text", text: '<img src=x onerror="alert(1)">' }] }];
  const s = screen(t, "cc-markup", { list: evil });
  await s.button().click();
  assert.deepEqual(s.calls.copied, ['### 1. 사용자\n\n<img src=x onerror="alert(1)">']);
  const line = s.root.querySelector("#session-copy-conversation-status");
  assert.equal(line.children.length, 0);
  assert.equal(line.textContent, "메시지 1개의 글을 Markdown으로 복사했습니다.");
});
