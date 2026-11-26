// How to run: see the 테스트 section of web/dashboard/dev/README.md.
//
// Guards the rule that a test puts back every global it swaps. CI runs all
// test files in one process (`--test-isolation=none`): Node imports every file
// first and only then runs the tests, in file name order. So the snapshot
// below, taken while this file is imported, is the state before any test body
// ran, and the test in this file runs after all the others as long as this
// file sorts last. The second test fails if a file is ever named after it.
import assert from "node:assert/strict";
import { readdirSync } from "node:fs";
import { basename } from "node:path";
import { test } from "node:test";

/** The globals the render and API tests swap. */
const WATCHED = Object.freeze(["document", "Node", "localStorage", "sessionStorage", "location", "history", "fetch"]);

/** How each name is defined on `globalThis`, without reading its value. */
const snapshot = () => WATCHED.map((name) => [name, Object.getOwnPropertyDescriptor(globalThis, name)]);

const before = snapshot();

/** The names whose definition differs between two snapshots. */
function changed(then, now) {
  const keys = ["value", "get", "set", "writable", "enumerable", "configurable"];
  return then
    .filter(([, was], at) => {
      const is = now[at][1];
      if (!was || !is) return was !== is;
      return keys.some((key) => was[key] !== is[key]);
    })
    .map(([name]) => name);
}

test("no test left a swapped global behind", () => {
  assert.deepEqual(
    changed(before, snapshot()),
    [],
    "these globals differ from before the tests ran; restore them with saveGlobals (fake-dom.mjs) in t.after",
  );
});

// The snapshot above is blind to a file-level `before` hook: in one process it
// runs before this file is even imported, so the swapped value becomes the
// "original". These names do not exist in Node at all, so no snapshot is needed
// to know the right answer.
const BROWSER_ONLY = Object.freeze(["document", "Node", "location", "history"]);

test("browser-only globals do not exist once every test has finished", () => {
  assert.deepEqual(
    BROWSER_ONLY.filter((name) => Object.getOwnPropertyDescriptor(globalThis, name)),
    [],
    "these exist only because a test (or a file-level before hook) put them there; use saveGlobals with t.after in each test",
  );
});

test("this guard is the last test file by name", () => {
  const files = readdirSync(import.meta.dirname)
    .filter((name) => name.endsWith(".test.mjs"))
    .sort();
  assert.equal(files.at(-1), basename(import.meta.filename));
});
