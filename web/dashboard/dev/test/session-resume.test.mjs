// How to run: see the 테스트 section of web/dashboard/dev/README.md.
//
// 터미널에서 이어 하기 on the session screen: the command shown for a session,
// how its folder is quoted for a POSIX shell, and the copy button.
import assert from "node:assert/strict";
import { test } from "node:test";

import { DOM_GLOBALS, installDom, saveGlobals, visible } from "./fake-dom.mjs";
import { render, resumeCommand, shellQuote } from "../../view-session.js";

/**
 * Reads one POSIX shell word made only of single-quoted pieces and `\'`
 * (what shellQuote writes) back into the text it stands for. Any other
 * character outside quotes would be read by a shell as syntax, so it fails.
 * Limit: this follows POSIX rules only (a backslash inside quotes is itself).
 * fish reads `\'` and `\\` inside quotes as escapes, so `fishWord` below reads
 * every made command by that rule too. No real shell runs here (CI forbids
 * child processes, and fish is not installed on the build host).
 */
function unquote(word) {
  let out = "";
  let at = 0;
  while (at < word.length) {
    if (word[at] === "'") {
      const end = word.indexOf("'", at + 1);
      assert.notEqual(end, -1, `an unclosed quote in ${word}`);
      out += word.slice(at + 1, end);
      at = end + 1;
    } else if (word.startsWith("\\'", at)) {
      out += "'";
      at += 2;
    } else {
      assert.fail(`the character ${JSON.stringify(word[at])} is outside the quotes in ${word}`);
    }
  }
  return out;
}

/**
 * The same word as fish reads it: inside single quotes `\'` and `\\` are
 * escapes and every other backslash is itself; outside quotes only `\'` is
 * allowed here. Returns null for a word with anything else outside quotes
 * (fish would read it as syntax) or an unclosed quote.
 */
function fishWord(word) {
  let out = "";
  let at = 0;
  while (at < word.length) {
    if (word[at] === "'") {
      at += 1;
      for (;;) {
        if (at >= word.length) return null;
        const char = word[at];
        if (char === "\\" && (word[at + 1] === "'" || word[at + 1] === "\\")) {
          out += word[at + 1];
          at += 2;
        } else if (char === "'") {
          at += 1;
          break;
        } else {
          out += char;
          at += 1;
        }
      }
    } else if (word.startsWith("\\'", at)) {
      out += "'";
      at += 2;
    } else {
      return null;
    }
  }
  return out;
}

const NASTY = [
  "/work/my project",
  "/work/it's",
  "/work/'",
  "/work/''",
  "/work/$HOME",
  "/work/$(rm -rf ~)",
  "/work/`id`",
  "/work/a;b",
  "/work/a && b",
  "/work/a | b > c",
  '/work/"quoted"',
  "/work/back\\slash",
  "/work/star*?[x]",
  "/work/~user",
  "/work/!hist",
  "/work/한글 폴더",
  "/work/line\nbreak",
  "-rf",
  "--help",
  "",
  "'",
];

test("shellQuote writes single quotes and turns each inner quote into '\\''", () => {
  assert.equal(shellQuote("/a b"), "'/a b'");
  assert.equal(shellQuote("it's"), "'it'\\''s'");
  assert.equal(shellQuote("'"), "''\\'''");
  assert.equal(shellQuote(""), "''");
  assert.equal(shellQuote("$HOME `x`; \\"), "'$HOME `x`; \\'");
});

test("a quoted word reads back as exactly the text, for every dangerous character", () => {
  for (const text of NASTY) assert.equal(unquote(shellQuote(text)), text, JSON.stringify(text));
  // Every character that is special to a shell, alone and next to a quote.
  for (const char of " \t\"'$`\\;&|<>(){}[]*?!#~%^=-\n") {
    for (const text of [char, `a${char}b`, `${char}'${char}`]) assert.equal(unquote(shellQuote(text)), text, JSON.stringify(text));
  }
});

test("the command is one cd and one junees, with the folder as a single quoted word", () => {
  const { command, note } = resumeCommand("/work/x", "20261113-010203-abcd");
  assert.equal(command, "cd -- '/work/x' && junees --resume 20261113-010203-abcd");
  assert.match(note, /실행하지 않습니다/);
  for (const cwd of NASTY.filter((path) => path.startsWith("/") && !/[\n\\]/.test(path))) {
    const got = resumeCommand(cwd, "abc").command;
    const [head, tail, ...rest] = got.split(" && junees --resume ");
    assert.deepEqual(rest, [], cwd);
    assert.equal(tail, "abc");
    assert.ok(head.startsWith("cd -- "), cwd);
    const word = head.slice("cd -- ".length);
    // The POSIX reader below and the fish reader both give back the folder, as one word.
    assert.equal(unquote(word), cwd);
    assert.equal(fishWord(word), cwd, `fish: ${cwd}`);
  }
  // A name that starts with a dash is after `--` and inside quotes.
  assert.equal(resumeCommand("/-rf", "a").command, "cd -- '/-rf' && junees --resume a");
});

test("a path with a line break or another control character gets no command and a reason", () => {
  for (const cwd of ["/work/a\nb", "/work/a\rb", "/work/a\u0000b", "/work/\u001b[31m", "/work/a\tb", "/work/\u007f", "/work/\u0085", "/work/\u2028"]) {
    const got = resumeCommand(cwd, "abc");
    assert.equal(got.command, null, JSON.stringify(cwd));
    assert.match(got.note, /한 줄 명령으로 안전하게 보여 줄 수 없어 명령을 만들지 않았습니다/);
  }
});

test("a path with a backslash gets no cd command, because fish reads escapes inside single quotes", () => {
  // The hazard: shellQuote is right for POSIX shells and wrong for fish when a backslash
  // comes before a quote. fish ends the quote early and runs what follows the `;`.
  const hostile = "/work/a\\'; touch x #";
  assert.equal(unquote(shellQuote(hostile)), hostile);
  assert.notEqual(fishWord(shellQuote(hostile)), hostile);
  assert.equal(fishWord(shellQuote(hostile)), null);
  for (const cwd of [hostile, "/work/a\\'b", "/work/end\\", "/work/two\\\\", "/work/\\\\'", "/work/a\\b", "/\\", "/\\'\\'", "/work/a\\'\\'; id #"]) {
    const got = resumeCommand(cwd, "abc");
    assert.equal(got.command, null, cwd);
    assert.match(got.note, /역슬래시/);
    assert.match(got.note, /한 줄 명령으로 안전하게 보여 줄 수 없어 명령을 만들지 않았습니다/);
  }
  // No backslash, a quote anywhere: both readers agree on every shape that is made.
  for (const cwd of ["/work/'", "/work/a'b", "/work/''x''", "/work/'; touch x #", "/'/'/'", "/work/it's a $HOME"]) {
    const word = resumeCommand(cwd, "abc").command.split(" && ")[0].slice("cd -- ".length);
    assert.equal(unquote(word), cwd, cwd);
    assert.equal(fishWord(word), cwd, `fish: ${cwd}`);
  }
});

test("a path with a direction control character gets no command; zero-width joiners are fine", () => {
  const unsafe = ["\u061c", "\u200e", "\u200f", "\u202a", "\u202b", "\u202c", "\u202d", "\u202e", "\u2066", "\u2067", "\u2068", "\u2069"];
  for (const char of unsafe) {
    const got = resumeCommand(`/work/a${char}b`, "abc");
    assert.equal(got.command, null, JSON.stringify(char));
    assert.match(got.note, /글자 방향을 바꾸는 문자/);
  }
  for (const char of ["\u200c", "\u200d", "\u00ad"]) {
    assert.equal(resumeCommand(`/work/a${char}b`, "abc").command, `cd -- '/work/a${char}b' && junees --resume abc`, JSON.stringify(char));
  }
});

test("a long path is quoted whole up to 4,096 characters and gets no command beyond it", () => {
  const long = `/${"a".repeat(4000)}`;
  assert.equal(resumeCommand(long, "abc").command, `cd -- '${long}' && junees --resume abc`);
  const quotes = `/${"'".repeat(1000)}`;
  assert.equal(unquote(resumeCommand(quotes, "abc").command.split(" && ")[0].slice("cd -- ".length)), quotes);
  const tooLong = `/${"a".repeat(4096)}`;
  assert.equal(resumeCommand(tooLong, "abc").command, null);
  assert.match(resumeCommand(tooLong, "abc").note, /너무 길어서/);
});

test("without a usable folder the command has no cd and says why", () => {
  for (const cwd of [null, undefined, "", 7, "[hidden]", "[hidden: key]", "/work/[hidden]/x", "relative/dir", "~/work", "C:\\work"]) {
    const got = resumeCommand(cwd, "abc");
    assert.equal(got.command, "junees --resume abc", JSON.stringify(cwd));
    assert.match(got.note, /폴더 이동 없이 만들었습니다/);
    assert.match(got.note, /기록 속 상대 경로는 세션을 시작한 폴더 기준/);
  }
});

test("a session id that is not [A-Za-z0-9_-] 1 to 64 characters gets no command", () => {
  for (const id of ["", "a b", "a;b", "a/b", "..", "a$(x)", "a`x`", "a\nb", "한글", "a".repeat(65), null, undefined, 5, "a'b", "a\"b"]) {
    const got = resumeCommand("/work/x", id);
    assert.equal(got.command, null, JSON.stringify(id));
    assert.match(got.note, /예상한 형식/);
  }
  for (const id of ["a", "A_b-9", "20261113-010203-abcd", "a".repeat(64), "-lead"]) {
    assert.equal(resumeCommand("/work/x", id).command, `cd -- '/work/x' && junees --resume ${id}`, id);
  }
});

// Rendering. Each test uses its own session id: the screen keeps the pages
// loaded with 더 보기 per session.

const SWAPPED = Object.freeze([...DOM_GLOBALS, "localStorage", "sessionStorage", "location", "history", "navigator"]);
const swappedNow = () => SWAPPED.map((name) => Object.getOwnPropertyDescriptor(globalThis, name));

const NOW = new Date("2026-11-13T09:10:00Z");
const ID = "20261113-010203-abcd";

const messages = [
  { index: 0, role: "user", blocks: [{ type: "text", text: "배포 가이드를 써 줘" }] },
  { index: 1, role: "assistant", blocks: [{ type: "text", text: "네, 쓰겠습니다." }] },
  { index: 2, role: "user", blocks: [{ type: "text", text: "고마워요" }] },
];

/**
 * `clipboard`: `"works"` records what is written, `"refuses"` rejects,
 * `"throws"` throws at once, `"none"` leaves `navigator` without the API,
 * `"no navigator"` removes `navigator`, a function is `writeText` itself.
 */
function screen(t, id, { cwd = "/work/x", clipboard = "works", loaded = 2 } = {}) {
  t.after(saveGlobals(SWAPPED));
  const document = installDom();
  const root = document.createElement("div");
  document.body.append(root);
  const calls = { fetched: [], announced: [], stored: 0, copied: [] };
  for (const name of ["localStorage", "sessionStorage", "location", "history"]) {
    Object.defineProperty(globalThis, name, { configurable: true, get: () => ((calls.stored += 1), undefined) });
  }
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
  const value = clipboard === "none" ? {} : clipboard === "no navigator" ? undefined : { clipboard: { writeText } };
  Object.defineProperty(globalThis, "navigator", { configurable: true, value });
  const ctx = {
    now: NOW,
    setTitle() {},
    announce: (line) => calls.announced.push(line),
    isUnauthorized: () => false,
    describeError: (err) => String(err),
    fetchJson: async (url) => {
      calls.fetched.push(url);
      const offset = Number(new URL(url, "http://x").searchParams.get("offset"));
      return { id, offset, total: messages.length, messages: messages.slice(offset) };
    },
  };
  const data = { id, title: "배포 가이드", cwd, updated_at: "2026-11-13T09:05:00Z", total: messages.length, messages: messages.slice(0, loaded) };
  const draw = (over = {}) => render(root, { ...data, ...over }, ctx);
  draw();
  const find = (selector) => root.querySelector(selector);
  return {
    document,
    root,
    calls,
    draw,
    ctx,
    input: find("#session-find"),
    toggle: find("#session-hide-tools"),
    command: () => find("#session-resume-command"),
    button: () => find("#session-resume-copy"),
    result: () => find("output").textContent,
    note: () => find("#session-resume-note").textContent,
    more: () => visible(root, "button").find((b) => b.textContent.includes("더 보기")) ?? null,
  };
}

test("the session card shows the command in a code element with a labelled copy button", (t) => {
  const s = screen(t, "resume-card");
  const command = s.command();
  assert.equal(command.tagName, "CODE");
  assert.equal(command.textContent, "cd -- '/work/x' && junees --resume resume-card");
  assert.equal(command.childNodes.length, 1);
  const button = s.button();
  assert.equal(button.tagName, "BUTTON");
  assert.equal(button.getAttribute("type"), "button");
  assert.equal(button.textContent, "복사");
  // The name starts from the visible word and says what is copied.
  assert.match(button.getAttribute("aria-label"), /복사$/);
  const group = s.root.querySelector(".session-resume");
  assert.equal(group.getAttribute("role"), "group");
  assert.equal(s.root.querySelector(`#${group.getAttribute("aria-labelledby")}`).textContent, "터미널에서 이어 하기");
  // It sits in the card with the folder and the id, and says the page does not run it.
  assert.ok(s.root.children[0].contains(command));
  assert.ok(s.root.children[0].contains(button));
  assert.match(s.note(), /대시보드는 이 명령을 실행하지 않습니다/);
  // The result line exists from the start, empty: a live region that appears with its text may not be read.
  assert.equal(s.root.querySelectorAll("output").length, 1);
  assert.equal(s.result(), "");
  assert.equal(visible(s.root, "#session-resume-command").length, 1);
});

test("a folder full of shell characters is shown as one quoted word", (t) => {
  const cwd = "/work/it's $HOME `id`; rm -rf ~ && x | y";
  const s = screen(t, "resume-nasty", { cwd });
  const want = `cd -- ${shellQuote(cwd)} && junees --resume resume-nasty`;
  assert.equal(s.command().textContent, want);
  assert.equal(s.command().childNodes.length, 1);
  assert.equal(want, "cd -- '/work/it'\\''s $HOME `id`; rm -rf ~ && x | y' && junees --resume resume-nasty");
});

test("a hidden or missing folder gives the command without cd and a note", async (t) => {
  for (const [name, cwd] of [["hidden", "[hidden]"], ["null", null], ["empty", ""]]) {
    // One screen at a time: each puts back the globals it swapped when its subtest ends.
    await t.test(name, (inner) => {
      const s = screen(inner, `resume-nodir-${name}`, { cwd });
      assert.equal(s.command().textContent, `junees --resume resume-nodir-${name}`, name);
      assert.equal(visible(s.root, "#session-resume-command").length, 1);
      assert.match(s.note(), /폴더 이동 없이 만들었습니다/);
    });
  }
});

test("a path that cannot be one safe line shows the reason and no command or button", (t) => {
  const s = screen(t, "resume-newline", { cwd: "/work/a\nrm -rf ~" });
  assert.equal(s.command().textContent, "");
  assert.deepEqual(visible(s.root, "#session-resume-command"), []);
  assert.deepEqual(visible(s.root, "#session-resume-copy"), []);
  assert.match(s.note(), /한 줄 명령으로 안전하게 보여 줄 수 없어 명령을 만들지 않았습니다/);
  assert.equal(s.root.textContent.includes("rm -rf ~"), true, "the folder fact on the card still shows what the server sent");
  assert.equal(s.command().textContent.includes("rm -rf"), false);
});

test("a session id in another shape shows the reason and no command", (t) => {
  const s = screen(t, "bad id; rm");
  assert.equal(s.command().textContent, "");
  assert.deepEqual(visible(s.root, "#session-resume-copy"), []);
  assert.match(s.note(), /예상한 형식/);
});

test("copy writes the shown command to the clipboard and says so, sending nothing anywhere", async (t) => {
  const s = screen(t, "resume-copy");
  await s.button().click();
  assert.deepEqual(s.calls.copied, ["cd -- '/work/x' && junees --resume resume-copy"]);
  assert.equal(s.result(), "명령을 복사했습니다.");
  assert.deepEqual(s.calls.fetched, []);
  assert.equal(s.calls.stored, 0);
  assert.deepEqual(s.calls.announced, []);
});

test("copying again empties the result line first, so the same result is announced again", async (t) => {
  const seen = [];
  let s = null;
  s = screen(t, "resume-twice", {
    clipboard: async () => {
      // What the line says at the moment the browser is asked.
      seen.push(s.result());
    },
  });
  await s.button().click();
  assert.equal(s.result(), "명령을 복사했습니다.");
  await s.button().click();
  assert.deepEqual(seen, ["", ""]);
  assert.equal(s.result(), "명령을 복사했습니다.");
});

test("a refused or failed copy says so and tells the person to copy by hand", async (t) => {
  for (const mode of ["refuses", "throws"]) {
    await t.test(mode, async (inner) => {
      const s = screen(inner, `resume-fail-${mode}`, { clipboard: mode });
      await s.button().click();
      assert.equal(s.result(), "복사하지 못했습니다. 위 명령을 직접 선택해 복사해 주세요.", mode);
      assert.equal(s.calls.stored, 0);
    });
  }
});

test("without a clipboard API the person is told to select and copy the text", async (t) => {
  const missing = "이 브라우저에서는 복사 기능을 쓸 수 없습니다. 위 명령을 직접 선택해 복사해 주세요.";
  for (const mode of ["none", "no navigator"]) {
    await t.test(mode, async (inner) => {
      const s = screen(inner, `resume-none-${mode.replace(" ", "-")}`, { clipboard: mode });
      await s.button().click();
      assert.equal(s.result(), missing, mode);
      assert.equal(s.command().textContent.startsWith("cd -- "), true);
    });
  }
  // A navigator whose getter throws reads as no API.
  await t.test("navigator getter throws", async (inner) => {
    const s = screen(inner, "resume-getter");
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      get: () => {
        throw new Error("blocked");
      },
    });
    await s.button().click();
    assert.equal(s.result(), missing);
  });
});

test("a refresh keeps the copy button mounted and focused, with its result, and the 찾기 state", async (t) => {
  const s = screen(t, "resume-refresh");
  const button = s.button();
  const command = s.command();
  await s.input.type("가이드");
  s.toggle.checked = true;
  await s.toggle.dispatch("change");
  button.focus();
  await button.click();
  assert.equal(s.result(), "명령을 복사했습니다.");

  s.draw();
  assert.equal(s.button(), button);
  assert.equal(s.command(), command);
  assert.equal(button.detached, 0);
  assert.equal(command.detached, 0);
  assert.equal(s.document.activeElement, button);
  // The same command: the result still describes what is on screen.
  assert.equal(s.result(), "명령을 복사했습니다.");
  assert.equal(s.input.value, "가이드");
  assert.equal(s.toggle.checked, true);
  assert.equal(s.input.detached, 0);
  assert.equal(s.toggle.detached, 0);
  assert.equal(s.root.querySelectorAll("#session-resume-command").length, 1);
  assert.equal(s.root.querySelectorAll("output").length, 1);
});

test("a refresh that changes the folder updates the command in place and clears the old result", async (t) => {
  const s = screen(t, "resume-change");
  const button = s.button();
  button.focus();
  await button.click();
  assert.equal(s.result(), "명령을 복사했습니다.");

  s.draw({ cwd: "/work/y z" });
  assert.equal(s.button(), button);
  assert.equal(s.document.activeElement, button);
  assert.equal(s.command().textContent, "cd -- '/work/y z' && junees --resume resume-change");
  assert.equal(s.result(), "");
  await button.click();
  assert.deepEqual(s.calls.copied.at(-1), "cd -- '/work/y z' && junees --resume resume-change");
});

test("더 보기 leaves the 이어 하기 block as it was", async (t) => {
  const s = screen(t, "resume-more", { loaded: 2 });
  await s.button().click();
  const button = s.button();
  await s.more().click();
  assert.deepEqual(s.calls.fetched, ["/api/sessions/resume-more?offset=2&limit=100"]);
  assert.equal(s.button(), button);
  assert.equal(button.detached, 0);
  assert.equal(s.result(), "명령을 복사했습니다.");
  assert.equal(s.root.querySelectorAll("#session-resume-command").length, 1);
});

test("an answer that comes after the command changed is dropped", async (t) => {
  let release;
  const slow = (text) => new Promise((resolve) => (release = () => resolve(text)));
  const s = screen(t, "resume-stale", { clipboard: slow });
  const click = s.button().click();
  s.draw({ cwd: "/work/other" });
  release();
  await click;
  assert.equal(s.result(), "");
  assert.equal(s.command().textContent, "cd -- '/work/other' && junees --resume resume-stale");
});

test("another session mounts its own block with an empty result", async (t) => {
  const s = screen(t, "resume-a");
  await s.button().click();
  const first = s.button();
  render(s.root, { id: "resume-b", title: "다른 세션", cwd: "/work/b", total: 2, messages: messages.slice(0, 2) }, s.ctx);
  assert.notEqual(s.button(), first);
  assert.equal(s.root.querySelectorAll("#session-resume-command").length, 1);
  assert.equal(s.command().textContent, "cd -- '/work/b' && junees --resume resume-b");
  assert.equal(s.result(), "");
});

test("a resume rendering test puts back the globals it swapped", async (t) => {
  const before = swappedNow();
  await t.test("while it runs, the stand-ins are in place", (inner) => {
    const s = screen(inner, "resume-globals");
    assert.equal(globalThis.document, s.document);
    assert.notDeepEqual(swappedNow(), before);
  });
  assert.deepEqual(swappedNow(), before);
});
