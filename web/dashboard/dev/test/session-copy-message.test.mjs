// How to run: see the 테스트 section of web/dashboard/dev/README.md.
//
// 글 복사 on the session screen: the 복사 button of a message copies the text
// blocks that message draws, and nothing else.
import assert from "node:assert/strict";
import { test } from "node:test";

import { DOM_GLOBALS, installDom, saveGlobals, visible } from "./fake-dom.mjs";
import { messageCopyResult, messageCopyText, messageView, newCopyState, render } from "../../view-session.js";

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
/** The messages that have words to copy, and what the button copies. */
const COPIED = new Map([
  [0, "배포 가이드를 써 줘"],
  [1, "먼저 파일을 읽겠습니다.\n\n읽었습니다."],
  [3, `${HIDDEN}\n\n나머지 설명`],
  [4, "긴 답변의 앞부분"],
  [6, "다시 해 줘"],
]);

const shownText = (node) => ("data" in node ? node.data : node.hidden ? "" : node.childNodes.map(shownText).join(""));

// The text, without a screen.

test("one text block is copied as it is", () => {
  assert.deepEqual(messageCopyText(talk[0]), { text: "배포 가이드를 써 줘", truncated: false });
});

test("several text blocks are joined by one empty line and tool blocks are left out", () => {
  assert.deepEqual(messageCopyText(talk[1]), { text: "먼저 파일을 읽겠습니다.\n\n읽었습니다.", truncated: false });
});

test("a message without a text block has nothing to copy", () => {
  for (const index of [2, 5, 7, 9]) assert.deepEqual(messageCopyText(talk[index]), { text: null, truncated: false }, index);
  // Text of only blanks draws nothing to copy either.
  assert.equal(messageCopyText(talk[8]).text, null);
  for (const bad of [null, undefined, {}, { blocks: "x" }, { blocks: [null, 3, {}] }]) assert.equal(messageCopyText(bad).text, null);
});

test("a block the server hid is copied as the note the screen shows, and the other text stays", () => {
  assert.equal(messageCopyText(talk[3]).text, `${HIDDEN}\n\n나머지 설명`);
  // Only a whole block that is the mark counts; a mark inside text is the text as it is.
  const inline = { blocks: [{ type: "text", text: "키는 [hidden: key] 입니다" }] };
  assert.equal(messageCopyText(inline).text, "키는 [hidden: key] 입니다");
});

test("a cut text is copied as far as it came and says so", () => {
  assert.deepEqual(messageCopyText(talk[4]), { text: "긴 답변의 앞부분", truncated: true });
  // A hidden block that carries the flag is the note, not a cut text.
  assert.equal(messageCopyText({ blocks: [{ type: "text", text: "[hidden]", truncated: true }] }).truncated, false);
});

test("a Junees notice is not copied, the person's text after it is", () => {
  assert.equal(messageCopyText(talk[6]).text, "다시 해 줘");
  assert.equal(messageCopyText({ blocks: [{ type: "text", text: "[되돌림] x", notice: true }] }).text, null);
  // Without the mark, text that looks like a notice is the person's.
  assert.equal(messageCopyText({ blocks: [{ type: "text", text: "[되돌림] x" }] }).text, "[되돌림] x");
});

test("the text is copied as a string, never read as markup", () => {
  const text = '<img src=x onerror="alert(1)"> & <b>굵게</b>';
  assert.equal(messageCopyText({ blocks: [{ type: "text", text }] }).text, text);
});

test("the result lines name the message and say what was copied", () => {
  assert.equal(messageCopyResult("3", "ok", false), "3번째 메시지의 글을 복사했습니다.");
  assert.match(messageCopyResult("3", "ok", true), /^3번째 메시지의 글 중 화면에 보이는 앞부분만 복사했습니다\. 서버가 긴 글을 잘라서 보냈습니다\.$/);
  assert.match(messageCopyResult("3", "denied", false), /^3번째 메시지의 글을 복사하지 못했습니다\./);
  assert.match(messageCopyResult("3", "missing", false), /^이 브라우저에서는 복사 기능을 쓸 수 없습니다\./);
});

test("a message view without copy state has no button, as in the chat history", (t) => {
  t.after(saveGlobals(DOM_GLOBALS));
  installDom();
  const item = messageView(talk[0], NOW);
  assert.equal(item.querySelector("button"), null);
  assert.equal(item.querySelector("output"), null);
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
    button: (index) => root.querySelector(`#msg-copy-${index}`),
    result: (index) => root.querySelector(`#msg-copy-status-${index}`).textContent,
    item: (index) => root.querySelector(`[data-index="${index}"]`),
    shown: () => visible(root, ".msg").map((li) => Number(li.getAttribute("data-index"))),
    more: () => visible(root, "button").find((b) => b.textContent.includes("더 보기")) ?? null,
    check: (on) => {
      const box = root.querySelector("#session-hide-tools");
      box.checked = on;
      return box.dispatch("change");
    },
  };
}

test("only a message with words to copy has a labelled button and its own result line", (t) => {
  const s = screen(t, "cm-render");
  const withButton = talk.filter((m) => s.button(m.index) !== null).map((m) => m.index);
  assert.deepEqual(withButton, [...COPIED.keys()]);
  for (const index of COPIED.keys()) {
    const button = s.button(index);
    assert.equal(button.tagName, "BUTTON");
    assert.equal(button.getAttribute("type"), "button");
    assert.equal(button.textContent, "복사");
    assert.equal(button.getAttribute("aria-label"), `${index + 1}번째 메시지 글 복사`);
    // It sits in the message's head row, and the message is still named by the head alone.
    const item = s.item(index);
    assert.ok(item.contains(button));
    assert.equal(item.getAttribute("aria-labelledby"), `msg-${index}`);
    assert.equal(s.root.querySelector(`#msg-${index}`).contains(button), false);
    const line = s.root.querySelector(`#msg-copy-status-${index}`);
    assert.equal(line.tagName, "OUTPUT");
    assert.equal(line.getAttribute("for"), button.id);
    assert.equal(line.textContent, "", "empty and mounted from the start");
    assert.ok(item.contains(line));
  }
  for (const index of [2, 5, 7, 8, 9]) assert.equal(s.root.querySelector(`#msg-copy-status-${index}`), null, index);
  // Ids are the original message numbers and appear once.
  assert.equal(s.root.querySelectorAll("#msg-copy-1").length, 1);
});

test("every copied part is drawn on the message, and what is left out is not copied", (t) => {
  const s = screen(t, "cm-drawn");
  for (const [index, copied] of COPIED) {
    const drawn = shownText(s.item(index));
    for (const part of copied.split("\n\n")) assert.ok(drawn.includes(part), `${index}: ${part}`);
  }
  assert.equal(COPIED.get(1).includes("read_file"), false);
  assert.equal(COPIED.get(6).includes("되돌림"), false);
  assert.equal(shownText(s.item(3)).includes("api key"), false, "the screen shows the note, never the mark's content");
});

test("a press copies that message's words, says so on its own line, and sends nothing anywhere", async (t) => {
  const s = screen(t, "cm-copy");
  for (const [index, copied] of COPIED) {
    await s.button(index).click();
    assert.equal(s.calls.copied.at(-1), copied, index);
    const n = index + 1;
    assert.equal(s.result(index), index === 4 ? `${n}번째 메시지의 글 중 화면에 보이는 앞부분만 복사했습니다. 서버가 긴 글을 잘라서 보냈습니다.` : `${n}번째 메시지의 글을 복사했습니다.`);
  }
  assert.equal(s.calls.copied.length, COPIED.size);
  assert.deepEqual(s.calls.fetched, []);
  assert.equal(s.calls.network, 0);
  assert.equal(s.calls.stored, 0);
  assert.deepEqual(s.calls.announced, []);
  // Each result stays on its own message.
  assert.equal(s.result(0), "1번째 메시지의 글을 복사했습니다.");
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.");
});

test("a refused or failed copy says so and points to copying by hand", async (t) => {
  for (const mode of ["refuses", "throws"]) {
    await t.test(mode, async (inner) => {
      const s = screen(inner, `cm-fail-${mode}`, { clipboard: mode });
      await s.button(1).click();
      assert.equal(s.result(1), "2번째 메시지의 글을 복사하지 못했습니다. 화면의 글을 직접 선택해 복사해 주세요.");
      assert.equal(s.result(0), "");
      assert.equal(s.calls.stored, 0);
    });
  }
});

test("without a clipboard API a second press empties the line before the words come back", async (t) => {
  const s = screen(t, "cm-missing", { clipboard: "none" });
  const words = "이 브라우저에서는 복사 기능을 쓸 수 없습니다. 화면의 글을 직접 선택해 복사해 주세요.";
  await s.button(0).click();
  assert.equal(s.result(0), words);
  const second = s.button(0).click();
  assert.equal(s.result(0), "", "emptied at the press, not refilled in the same turn");
  await Promise.resolve();
  assert.equal(s.result(0), "", "still empty after the microtasks");
  await second;
  assert.equal(s.result(0), words);
});

test("pressing again with the same result empties the line first", async (t) => {
  const seen = [];
  let s = null;
  s = screen(t, "cm-twice", { clipboard: async () => void seen.push(s.result(1)) });
  await s.button(1).click();
  await s.button(1).click();
  assert.deepEqual(seen, ["", ""]);
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.");
});

test("a result for an older press is dropped when a newer press was made", async (t) => {
  const waits = [];
  const s = screen(t, "cm-overlap", { clipboard: () => new Promise((resolve, reject) => waits.push({ resolve, reject })) });
  const first = s.button(0).click();
  const second = s.button(0).click();
  waits[1].resolve();
  await second;
  assert.equal(s.result(0), "1번째 메시지의 글을 복사했습니다.");
  waits[0].reject(new Error("NotAllowedError"));
  await first;
  assert.equal(s.result(0), "1번째 메시지의 글을 복사했습니다.");
});

test("with a search on, the button of the message that is left copies that message", async (t) => {
  const s = screen(t, "cm-find");
  await s.input.type("읽었습니다");
  assert.deepEqual(s.shown(), [1]);
  await s.button(1).click();
  assert.deepEqual(s.calls.copied, [COPIED.get(1)]);
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.");
  assert.equal(s.calls.copied.length, 1);
  // Another word, another message: its button copies its own words under its own number.
  await s.input.type("나머지");
  assert.deepEqual(s.shown(), [3]);
  await s.button(3).click();
  assert.equal(s.calls.copied.at(-1), COPIED.get(3));
  assert.equal(s.result(3), "4번째 메시지의 글을 복사했습니다.");
  assert.equal(s.item(1).hidden, true);
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.", "a hidden message keeps its line");
});

test("with tool calls hidden the same messages copy the same words", async (t) => {
  const s = screen(t, "cm-hide");
  await s.check(true);
  // Only the messages that were only tool blocks go (2 and 5); a message with no tool block stays.
  assert.deepEqual(s.shown(), [0, 1, 3, 4, 6, 7, 8, 9]);
  assert.equal(s.button(1).hidden, false);
  for (const [index, copied] of COPIED) {
    if (s.item(index).hidden) continue;
    await s.button(index).click();
    assert.equal(s.calls.copied.at(-1), copied, index);
  }
  // Both on: the search narrows, the button still copies that message.
  await s.input.type("읽었습니다");
  assert.deepEqual(s.shown(), [1]);
  await s.button(1).click();
  assert.equal(s.calls.copied.at(-1), COPIED.get(1));
  assert.equal(s.root.querySelectorAll("#msg-copy-1").length, 1);
});

test("a refresh keeps the result and the focus on the same message's button", async (t) => {
  const s = screen(t, "cm-refresh");
  const before = s.button(1);
  before.focus();
  await before.click();
  assert.equal(s.document.activeElement, before);

  s.draw();
  const after = s.button(1);
  assert.equal(after.id, "msg-copy-1");
  assert.equal(s.document.activeElement, after, "focus is on the new button, not the page");
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.", "the line just shown is not wiped");
  assert.equal(s.result(0), "");
  assert.equal(s.root.querySelectorAll("#msg-copy-1").length, 1);
  assert.equal(s.root.querySelectorAll("#msg-copy-status-1").length, 1);
  // The new button works.
  await after.click();
  assert.equal(s.calls.copied.length, 2);
});

test("a refresh with the box checked and a search keeps the button and its result", async (t) => {
  const s = screen(t, "cm-refresh-find");
  await s.check(true);
  await s.input.type("읽었습니다");
  s.button(1).focus();
  await s.button(1).click();
  s.draw();
  assert.deepEqual(s.shown(), [1]);
  assert.equal(s.document.activeElement, s.button(1));
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.");
  assert.equal(s.input.value, "읽었습니다");
  assert.equal(s.toggle.checked, true);
});

test("when a refresh leaves the focused message off the screen, focus goes to the search box", async (t) => {
  const s = screen(t, "cm-refresh-gone");
  const button = s.button(1);
  button.focus();
  await s.input.type("배포");
  assert.deepEqual(s.shown(), [0]);
  s.draw();
  assert.equal(s.document.activeElement, s.input);
});

test("a refresh does not take focus from anything else", async (t) => {
  const s = screen(t, "cm-refresh-other");
  s.input.focus();
  s.draw();
  assert.equal(s.document.activeElement, s.input);
});

test("an answer that comes after a refresh lands on the line now on screen", async (t) => {
  let release;
  const s = screen(t, "cm-late", { clipboard: () => new Promise((resolve) => (release = resolve)) });
  const press = s.button(1).click();
  s.draw();
  release();
  await press;
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.");
  assert.equal(s.root.querySelectorAll("#msg-copy-status-1").length, 1);
});

test("an answer for text that changed in a refresh is dropped and the old result is cleared", async (t) => {
  let release;
  const s = screen(t, "cm-changed", { clipboard: () => new Promise((resolve) => (release = resolve)) });
  const press = s.button(0).click();
  const edited = talk.map((m) => (m.index === 0 ? { ...m, blocks: [{ type: "text", text: "다른 글" }] } : m));
  s.draw({ messages: edited });
  release();
  await press;
  assert.equal(s.result(0), "");
});

test("a result already shown is cleared when a refresh brings other words for that message", async (t) => {
  const s = screen(t, "cm-changed-2");
  await s.button(0).click();
  assert.equal(s.result(0), "1번째 메시지의 글을 복사했습니다.");
  s.draw({ messages: talk.map((m) => (m.index === 0 ? { ...m, blocks: [{ type: "text", text: "다른 글" }] } : m)) });
  assert.equal(s.result(0), "");
});

test("더 보기 keeps the buttons already on screen and their results, and the new messages get buttons", async (t) => {
  const s = screen(t, "cm-more", { loaded: 4 });
  const first = s.button(1);
  first.focus();
  await first.click();
  assert.equal(s.button(6), null);
  await s.more().click();
  assert.deepEqual(s.calls.fetched, ["/api/sessions/cm-more?offset=4&limit=100"]);
  assert.equal(s.button(1), first);
  assert.equal(first.detached, 0);
  assert.equal(s.result(1), "2번째 메시지의 글을 복사했습니다.");
  assert.equal(s.button(6).getAttribute("aria-label"), "7번째 메시지 글 복사");
  await s.button(6).click();
  assert.equal(s.calls.copied.at(-1), "다시 해 줘");
  assert.equal(s.root.querySelectorAll("#msg-copy-1").length, 1);
  assert.equal(s.calls.stored, 0);
});

test("the head keeps its id, which names the item, with and without the button", (t) => {
  t.after(saveGlobals(DOM_GLOBALS));
  installDom();
  const item = messageView(talk[0], NOW, undefined, newCopyState());
  assert.equal(item.querySelector("#msg-copy-0").textContent, "복사");
  const plain = messageView(talk[0], NOW);
  assert.equal(plain.children[0].id, "msg-0");
});
