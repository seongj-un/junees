// Run with: node --test --test-concurrency=1 web/dashboard/dev/test/*.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

// view-sessions.js imports dom.js, which only touches `document` when called.
import { filterSessions, previewAfterTitle } from "../../view-sessions.js";

test("previewAfterTitle drops the title the preview starts with", () => {
  assert.equal(previewAfterTitle("Fix the login bug", "Fix the login bug\nmy key is [hidden]"), "my key is [hidden]");
  assert.equal(previewAfterTitle("README 확인", "README 확인"), "");
  assert.equal(previewAfterTitle("다른 제목", "미리보기 글"), "미리보기 글");
  assert.equal(previewAfterTitle(null, "미리보기 글"), "미리보기 글");
  assert.equal(previewAfterTitle("제목", null), "");
});

const recent = Object.freeze([
  Object.freeze({ id: "1", title: "Dashboard 점검", cwd: "/work/첫째" }),
  Object.freeze({ id: "2", title: "배포 준비", cwd: "/work/Dashboard/한글" }),
  Object.freeze({ id: "3", title: null, cwd: null, preview: "Dashboard 점검" }),
  Object.freeze({ id: "4", title: "문서", cwd: "/work/한글" }),
]);

test("empty or whitespace-only search keeps the server order", () => {
  assert.deepEqual(filterSessions(recent, ""), recent);
  assert.deepEqual(filterSessions(recent, " \t "), recent);
});

test("search matches a trimmed English substring in title or working folder without reordering", () => {
  assert.deepEqual(filterSessions(recent, "  aShBo  "), [recent[0], recent[1]]);
});

test("search matches Korean substrings in either field, including decomposed Hangul", () => {
  assert.deepEqual(filterSessions(recent, "포 준"), [recent[1]]);
  assert.deepEqual(filterSessions(recent, "한글".normalize("NFD")), [recent[1], recent[3]]);
});

test("search does not match previews, ids, or a pattern instead of literal text", () => {
  assert.deepEqual(filterSessions([recent[2]], "dashboard"), []);
  assert.deepEqual(filterSessions(recent, "3"), []);
  assert.deepEqual(filterSessions(recent, ".*"), []);
  assert.deepEqual(filterSessions([], "점검"), []);
});
