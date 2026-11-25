// Run with: node --test --test-concurrency=1 web/dashboard/dev/test/*.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

// view-sessions.js imports dom.js, which only touches `document` when called;
// the render tests below install a stand-in from sessions-dom.mjs first.
import { filterSessions, previewAfterTitle, render, SORTS, sortSessions } from "../../view-sessions.js";
import { findAll, fire, installDom } from "./sessions-dom.mjs";

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

// Server order (newest first) is a, b, c, d, e.
const loaded = Object.freeze(
  [
    { id: "s-a", title: "Dashboard 점검", cwd: "/work/one", messages: 4, cost_usd: 0.5 },
    { id: "s-b", title: "Dashboard 배포", cwd: "/work/two", messages: 30, cost_usd: null },
    { id: "s-c", title: "문서", cwd: "/work/three", messages: 4, cost_usd: 2 },
    { id: "s-d", title: "dashboard 정리", cwd: "/work/four", messages: 9, cost_usd: 0.5 },
    { id: "s-e", title: "기타", cwd: "/work/five", messages: 1, cost_usd: 0 },
  ].map((session) =>
    Object.freeze({ updated_at: "2026-11-20T01:00:00Z", input_tokens: 10, output_tokens: 5, ...session }),
  ),
);
const ids = (sessions) => sessions.map((session) => session.id);

test("the sort choices are recent (default), cost and messages", () => {
  assert.deepEqual(
    SORTS.map((option) => [option.value, option.label]),
    [
      ["recent", "최근 순"],
      ["cost", "비용 큰 순"],
      ["messages", "메시지 많은 순"],
    ],
  );
});

test("the default and an unknown sort return the server order untouched", () => {
  assert.equal(sortSessions(loaded, "recent"), loaded);
  assert.equal(sortSessions(loaded, undefined), loaded);
  assert.equal(sortSessions(loaded, "constructor"), loaded);
});

test("cost sort puts the largest first, keeps ties in server order and unknown cost last", () => {
  assert.deepEqual(ids(sortSessions(loaded, "cost")), ["s-c", "s-a", "s-d", "s-e", "s-b"]);
  // The frozen input was not reordered in place.
  assert.deepEqual(ids(loaded), ["s-a", "s-b", "s-c", "s-d", "s-e"]);
});

test("an unknown cost is not counted as zero, whatever shape it has", () => {
  const mixed = [
    { id: "none", cost_usd: null },
    { id: "zero", cost_usd: 0 },
    { id: "missing" },
    { id: "nan", cost_usd: Number.NaN },
    { id: "tiny", cost_usd: 0.001 },
  ];
  assert.deepEqual(ids(sortSessions(mixed, "cost")), ["tiny", "zero", "none", "missing", "nan"]);
});

test("message sort puts the largest first and keeps ties in server order", () => {
  assert.deepEqual(ids(sortSessions(loaded, "messages")), ["s-b", "s-d", "s-a", "s-c", "s-e"]);
});

test("sorting applies to the search result only", () => {
  assert.deepEqual(ids(sortSessions(filterSessions(loaded, "dashboard"), "cost")), ["s-a", "s-d", "s-b"]);
  assert.deepEqual(ids(sortSessions(filterSessions(loaded, "dashboard"), "messages")), ["s-b", "s-d", "s-a"]);
});

// Render tests run on the DOM stand-in next to this file; no browser is involved.
const NOW = new Date("2026-11-25T00:00:00Z");
const byId = (dom, id) => dom.document.getElementById(id);
const rowOrder = (dom) =>
  findAll(dom.root, (node) => node.tagName === "TR" && node.getAttribute("data-key") !== null).map((row) =>
    row.getAttribute("data-key"),
  );

function mount(t, sessions = loaded) {
  const dom = installDom();
  t.after(dom.restore);
  render(dom.root, { sessions }, { now: NOW });
  return dom;
}

function choose(dom, value) {
  const select = byId(dom, "session-sort");
  select.value = value;
  fire(select, "change");
}

function search(dom, text) {
  const input = byId(dom, "session-search");
  input.value = text;
  fire(input, "input");
}

test("the sort control is a labelled select that starts on the server order", (t) => {
  const dom = mount(t);
  const select = byId(dom, "session-sort");
  assert.equal(select.tagName, "SELECT");
  assert.deepEqual(
    select.children.map((option) => [option.getAttribute("value"), option.textContent]),
    SORTS.map((option) => [option.value, option.label]),
  );
  const labels = findAll(dom.root, (node) => node.tagName === "LABEL" && node.getAttribute("for") === "session-sort");
  assert.deepEqual(
    labels.map((label) => label.textContent),
    ["정렬"],
  );
  // The scope note covers both controls and names the loaded limit.
  const scope = byId(dom, select.getAttribute("aria-describedby"));
  assert.equal(scope.textContent, "이미 불러온 최근 세션 최대 50개에서만 검색하고 정렬합니다.");
  assert.equal(select.value, "recent");
  assert.deepEqual(rowOrder(dom), ids(loaded));
});

test("choosing a sort redraws the rows in that order and back", (t) => {
  const dom = mount(t);
  choose(dom, "cost");
  assert.deepEqual(rowOrder(dom), ["s-c", "s-a", "s-d", "s-e", "s-b"]);
  choose(dom, "messages");
  assert.deepEqual(rowOrder(dom), ["s-b", "s-d", "s-a", "s-c", "s-e"]);
  choose(dom, "recent");
  assert.deepEqual(rowOrder(dom), ids(loaded));
});

test("the sort applies to the search result and stays when the search is cleared", (t) => {
  const dom = mount(t);
  choose(dom, "cost");
  search(dom, "dashboard");
  assert.deepEqual(rowOrder(dom), ["s-a", "s-d", "s-b"]);
  search(dom, "");
  assert.equal(byId(dom, "session-sort").value, "cost");
  assert.deepEqual(rowOrder(dom), ["s-c", "s-a", "s-d", "s-e", "s-b"]);

  // The clear button keeps the chosen order as well.
  search(dom, "dashboard");
  const clear = findAll(dom.root, (node) => node.tagName === "BUTTON" && node.textContent === "지우기")[0];
  fire(clear, "click");
  assert.equal(byId(dom, "session-sort").value, "cost");
  assert.deepEqual(rowOrder(dom), ["s-c", "s-a", "s-d", "s-e", "s-b"]);
});

test("a refresh keeps the chosen sort, the search text and the focused input", (t) => {
  const dom = mount(t);
  const input = byId(dom, "session-search");
  const select = byId(dom, "session-sort");
  input.focus();
  search(dom, "dashboard");
  choose(dom, "messages");

  const newer = Object.freeze({ ...loaded[4], id: "s-f", title: "Dashboard 새 세션", messages: 12, cost_usd: 9 });
  render(dom.root, { sessions: [newer, ...loaded] }, { now: NOW });

  assert.equal(byId(dom, "session-search"), input);
  assert.equal(byId(dom, "session-sort"), select);
  assert.equal(input.detachCount, 0);
  assert.equal(select.detachCount, 0);
  assert.equal(dom.document.activeElement, input);
  assert.equal(input.value, "dashboard");
  assert.equal(select.value, "messages");
  assert.deepEqual(rowOrder(dom), ["s-b", "s-f", "s-d", "s-a"]);
});

test("a refresh keeps focus on the sort control and on a row link that moved", (t) => {
  const dom = mount(t);
  const select = byId(dom, "session-sort");
  select.focus();
  choose(dom, "cost");
  render(dom.root, { sessions: loaded }, { now: NOW });
  assert.equal(dom.document.activeElement, select);

  byId(dom, "session-link-s-d").focus();
  choose(dom, "messages");
  assert.equal(dom.document.activeElement.id, "session-link-s-d");
  assert.equal(dom.document.activeElement.isConnected, true);
});

test("the empty and no-match notices stay the same under a sort", (t) => {
  const dom = mount(t);
  choose(dom, "cost");
  search(dom, "없는 검색어");
  assert.deepEqual(rowOrder(dom), []);
  assert.match(dom.root.textContent, /검색 결과가 없습니다/);
  assert.match(dom.root.textContent, /검색 결과 0개 \/ 불러온 세션 5개/);

  search(dom, "");
  render(dom.root, { sessions: [] }, { now: NOW });
  assert.match(dom.root.textContent, /아직 저장된 세션이 없습니다/);
  assert.equal(byId(dom, "session-sort").value, "cost");
});

test("sorting and searching never touch the URL, storage or the network", (t) => {
  const dom = mount(t);
  choose(dom, "cost");
  search(dom, "dashboard");
  choose(dom, "messages");
  render(dom.root, { sessions: loaded }, { now: NOW });
  choose(dom, "recent");
  assert.deepEqual(dom.touched, { localStorage: 0, sessionStorage: 0, location: 0, history: 0, fetch: 0 });
});

test("a new mount of the screen starts from the default sort", (t) => {
  const dom = mount(t);
  choose(dom, "cost");
  // Another screen took over the container, as the router does on leaving.
  dom.root.replaceChildren(dom.document.createElement("p"));
  render(dom.root, { sessions: loaded }, { now: NOW });
  assert.equal(byId(dom, "session-sort").value, "recent");
  assert.deepEqual(rowOrder(dom), ids(loaded));
});
