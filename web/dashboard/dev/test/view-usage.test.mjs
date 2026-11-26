import assert from "node:assert/strict";
import { test } from "node:test";
import { SCROLL_KEY } from "../../keep-ui.js";
import { render } from "../../view-usage.js";
import { DOM_GLOBALS, saveGlobals } from "./fake-dom.mjs";

// The renderer uses only these DOM operations. Native details interaction and
// layout are left to the browser.
class Element {
  children = [];
  attributes = {};
  style = { setProperty() {} };

  constructor(tag) { this.tag = tag; }
  setAttribute(key, value) { this.attributes[key] = value; }
  addEventListener() {}
  append(child) { this.children.push(child); }
  replaceChildren(...children) { this.children = children; }
  get textContent() { return this.children.map((child) => child.textContent).join(""); }
}

// Each test swaps the globals itself and puts them back in `t.after`, as the
// other test files do; a file-level hook would also run around every other
// file in the one-process CI run and hide the swap from zz-globals.test.mjs.
const installed = new WeakSet();

function installStandIn(t) {
  // One save per test even when a test draws several times: a second save
  // would capture the stand-in the first one left and restore it afterwards.
  if (installed.has(t)) return;
  installed.add(t);
  t.after(saveGlobals(DOM_GLOBALS));
  globalThis.Node = Element;
  globalThis.document = {
    createElement: (tag) => new Element(tag),
    createTextNode: (textContent) => ({ textContent }),
  };
}

function findAll(node, predicate) {
  return [
    ...(predicate(node) ? [node] : []),
    ...(node.children ?? []).flatMap((child) => findAll(child, predicate)),
  ];
}

function draw(t, days, rows) {
  installStandIn(t);
  const root = new Element("main");
  render(root, { days: rows }, { params: { days }, now: new Date(2026, 10, 23) });
  return root;
}

function row(date, input_tokens, output_tokens, cost_usd) {
  return { date, model: "fixture-model", input_tokens, output_tokens, cost_usd, cache_read_tokens: 900, cache_write_tokens: 800 };
}

function dailyRows(root, count) {
  const details = findAll(root, (node) => node.tag === "details");
  assert.deepEqual(details.map((node) => node.attributes.id), ["usage-day-table"]);
  // app.js puts focus back by this id after an auto-refresh.
  const summary = findAll(details[0], (node) => node.tag === "summary");
  assert.deepEqual(summary.map((node) => node.attributes.id), ["usage-day-table-toggle"]);
  const caption = findAll(details[0], (node) => node.tag === "caption");
  assert.deepEqual(caption.map((node) => node.textContent), [`최근 ${count}일 날짜별 토큰과 비용`]);
  const headers = findAll(details[0], (node) => node.tag === "th" && node.attributes.scope === "col");
  assert.deepEqual(headers.map((node) => node.textContent), ["날짜", "입력 토큰", "출력 토큰", "비용"]);
  const body = findAll(details[0], (node) => node.tag === "tbody")[0];
  assert.equal(body.children.length, count);
  for (const tr of body.children) {
    assert.equal(tr.children[0].tag, "th");
    assert.equal(tr.children[0].attributes.scope, "row");
    assert.deepEqual(tr.children.slice(1).map((cell) => cell.tag), ["td", "td", "td"]);
  }
  return body.children.map((tr) => tr.children.map((cell) => cell.textContent));
}

const hasChart = (root) => findAll(root, (node) => node.tag === "figure").length === 1;

const UNPRICED_NOTE = /가격을 알 수 없는 사용이 있어 비용 막대를 그리지 않았습니다/;
const NO_COST_NOTE = /이 기간에 막대로 표시할 비용이 없습니다/;
const chartNotes = (root) => findAll(root, (node) => node.attributes?.class === "chart-empty").map((node) => node.textContent);

test("unpriced usage still shows separate daily input and output counts, newest first", (t) => {
  const root = draw(t, 7, [row("2026-11-22", 20, 2, null), row("2026-11-23", 1200, 30, null)]);
  assert.equal(hasChart(root), false);
  const notes = chartNotes(root);
  assert.equal(notes.length, 1);
  assert.match(notes[0], UNPRICED_NOTE);
  assert.doesNotMatch(notes[0], NO_COST_NOTE);
  assert.deepEqual(dailyRows(root, 7).slice(0, 2), [
    ["11월 23일", "1,200", "30", "가격 없음"],
    ["11월 22일", "20", "2", "가격 없음"],
  ]);
});

test("known zero cost shows daily tokens without drawing cost bars", (t) => {
  const root = draw(t, 30, [row("2026-11-23", 11, 0, 0)]);
  assert.equal(hasChart(root), false);
  const notes = chartNotes(root);
  assert.equal(notes.length, 1);
  assert.match(notes[0], NO_COST_NOTE);
  assert.doesNotMatch(notes[0], UNPRICED_NOTE);
  assert.deepEqual(dailyRows(root, 30)[0], ["11월 23일", "11", "0", "$0.00"]);
});

test("mixed and priced dates keep their cost labels and the cost chart", (t) => {
  const root = draw(t, 90, [
    row("2026-11-23", 40, 4, 1.5),
    row("2026-11-23", 30, 3, null),
    row("2026-11-22", 20, 2, 0.75),
  ]);
  assert.equal(hasChart(root), true);
  assert.deepEqual(dailyRows(root, 90).slice(0, 2), [
    ["11월 23일", "70", "7", "$1.50 (일부 가격 없음)"],
    ["11월 22일", "20", "2", "$0.75"],
  ]);
});

test("the daily table scroll area is keyed by its period so a new period starts at the top", (t) => {
  const keys = [7, 30, 90].map((days) => {
    const root = draw(t, days, [row("2026-11-23", 11, 1, 0.5)]);
    const areas = findAll(root, (node) => node.attributes?.[SCROLL_KEY] !== undefined);
    assert.equal(areas.length, 1);
    assert.equal(areas[0].attributes.class, "table-wrap");
    return areas[0].attributes[SCROLL_KEY];
  });
  assert.equal(new Set(keys).size, 3);
  // the same period in a later refresh keeps the same key
  const again = findAll(draw(t, 30, [row("2026-11-23", 12, 1, 0.5)]), (node) => node.attributes?.[SCROLL_KEY] !== undefined);
  assert.equal(again[0].attributes[SCROLL_KEY], keys[1]);
});

test("an empty period keeps the empty notice without a daily table", (t) => {
  const root = draw(t, 7, []);
  assert.match(root.textContent, /최근 7일 동안 사용 기록이 없습니다/);
  assert.equal(findAll(root, (node) => node.tag === "table").length, 0);
});
