import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { render } from "../../view-usage.js";

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

before(() => {
  globalThis.Node = Element;
  globalThis.document = {
    createElement: (tag) => new Element(tag),
    createTextNode: (textContent) => ({ textContent }),
  };
});
after(() => {
  delete globalThis.document;
  delete globalThis.Node;
});

function findAll(node, predicate) {
  return [
    ...(predicate(node) ? [node] : []),
    ...(node.children ?? []).flatMap((child) => findAll(child, predicate)),
  ];
}

function draw(days, rows) {
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
  const headers = findAll(details[0], (node) => node.tag === "th" && node.attributes.scope === "col");
  assert.deepEqual(headers.map((node) => node.textContent), ["날짜", "입력 토큰", "출력 토큰", "비용"]);
  const body = findAll(details[0], (node) => node.tag === "tbody")[0];
  assert.equal(body.children.length, count);
  return body.children.map((tr) => tr.children.map((cell) => cell.textContent));
}

const hasChart = (root) => findAll(root, (node) => node.tag === "figure").length === 1;

test("unpriced usage still shows separate daily input and output counts, newest first", () => {
  const root = draw(7, [row("2026-11-22", 20, 2, null), row("2026-11-23", 1200, 30, null)]);
  assert.equal(hasChart(root), false);
  assert.deepEqual(dailyRows(root, 7).slice(0, 2), [
    ["11월 23일", "1,200", "30", "가격 없음"],
    ["11월 22일", "20", "2", "가격 없음"],
  ]);
});

test("known zero cost shows daily tokens without drawing cost bars", () => {
  const root = draw(30, [row("2026-11-23", 11, 0, 0)]);
  assert.equal(hasChart(root), false);
  assert.deepEqual(dailyRows(root, 30)[0], ["11월 23일", "11", "0", "$0.00"]);
});

test("mixed and priced dates keep their cost labels and the cost chart", () => {
  const root = draw(90, [
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

test("an empty period keeps the empty notice without a daily table", () => {
  const root = draw(7, []);
  assert.match(root.textContent, /최근 7일 동안 사용 기록이 없습니다/);
  assert.equal(findAll(root, (node) => node.tag === "table").length, 0);
});
