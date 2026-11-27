import assert from "node:assert/strict";
import { test } from "node:test";
import { renderKeepingUi, SCROLL_KEY } from "../../keep-ui.js";
import { DOM_GLOBALS, installDom, saveGlobals } from "./fake-dom.mjs";

/** A screen like the usage one: a <details> with a focusable summary and a keyed scroll area. */
function draw(view, { key = "table-30", open = false } = {}) {
  const details = document.createElement("details");
  details.setAttribute("id", "day-table");
  if (open) details.open = true;
  const summary = document.createElement("summary");
  summary.setAttribute("id", "day-table-toggle");
  const area = document.createElement("div");
  area.setAttribute(SCROLL_KEY, key);
  const plain = document.createElement("div");
  plain.setAttribute("id", "plain-scroll");
  details.append(summary, area, plain);
  view.replaceChildren(details);
  return { details, summary, area, plain };
}

function setup(t) {
  t.after(saveGlobals(DOM_GLOBALS));
  const doc = installDom();
  const view = doc.createElement("main");
  doc.body.append(view);
  return { doc, view };
}

test("a refresh puts back the vertical and horizontal scroll of a keyed area", (t) => {
  const { view } = setup(t);
  const before = draw(view, { open: true });
  before.area.scrollTop = 180;
  before.area.scrollLeft = 24;
  renderKeepingUi(view, () => draw(view));
  const after = view.querySelector(`[${SCROLL_KEY}]`);
  assert.notEqual(after, before.area, "the area is a new node, as in a real redraw");
  assert.equal(after.scrollTop, 180);
  assert.equal(after.scrollLeft, 24);
});

test("the details is open again before the scroll is set", (t) => {
  const { view } = setup(t);
  const before = draw(view, { open: true });
  before.area.scrollTop = 50;
  const order = [];
  renderKeepingUi(view, () => {
    const fresh = draw(view);
    let top = 0;
    Object.defineProperty(fresh.area, "scrollTop", {
      get: () => top,
      set: (value) => {
        order.push(["scroll", fresh.details.open]);
        top = value;
      },
    });
  });
  assert.deepEqual(order, [["scroll", true]]);
});

test("a different key starts at the top, as when the period changes", (t) => {
  const { view } = setup(t);
  const before = draw(view, { key: "table-30", open: true });
  before.area.scrollTop = 180;
  before.area.scrollLeft = 24;
  renderKeepingUi(view, () => draw(view, { key: "table-90" }));
  const after = view.querySelector(`[${SCROLL_KEY}]`);
  assert.ok(!after.scrollTop);
  assert.ok(!after.scrollLeft);
});

test("scroll areas without a key are left alone", (t) => {
  const { view } = setup(t);
  const before = draw(view);
  before.plain.scrollTop = 90;
  renderKeepingUi(view, () => draw(view));
  assert.ok(!view.querySelector("#plain-scroll").scrollTop);
});

test("a refresh keeps the open details and the focused summary", (t) => {
  const { doc, view } = setup(t);
  const before = draw(view, { open: true });
  before.summary.focus();
  assert.equal(doc.activeElement, before.summary);
  renderKeepingUi(view, () => draw(view));
  assert.equal(view.querySelector("#day-table").open, true);
  assert.equal(doc.activeElement, view.querySelector("#day-table-toggle"));
  assert.deepEqual(doc.activeElement.focusCalls, [{ preventScroll: true }]);
});

test("a screen that no longer has the keyed area redraws without error", (t) => {
  const { view } = setup(t);
  const before = draw(view, { open: true });
  before.area.scrollTop = 10;
  renderKeepingUi(view, () => view.replaceChildren());
  assert.equal(view.querySelectorAll(`[${SCROLL_KEY}]`).length, 0);
});
