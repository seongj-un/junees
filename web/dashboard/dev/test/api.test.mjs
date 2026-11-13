// Run with: node --test web/dashboard/dev/test/
import assert from "node:assert/strict";
import { test } from "node:test";

import { getToken, takeTokenFromAddress } from "../../api.js";

/** The parts of `window` the token helpers use. */
function fakeWindow(hash) {
  const store = new Map();
  const win = {
    location: { pathname: "/", search: "", hash },
    sessionStorage: {
      getItem: (key) => store.get(key) ?? null,
      setItem: (key, value) => store.set(key, String(value)),
      removeItem: (key) => store.delete(key),
    },
    history: {
      replaceState: (_state, _title, url) => {
        win.location.hash = url.includes("#") ? url.slice(url.indexOf("#")) : "";
      },
    },
  };
  return win;
}

test("takeTokenFromAddress keeps the token and clears it from the address", () => {
  const win = fakeWindow("#token=abc_DEF-123");
  assert.equal(takeTokenFromAddress(win), true);
  assert.equal(getToken(win), "abc_DEF-123");
  assert.equal(win.location.hash, "");
});

test("a new token pasted into an open tab replaces the old one", () => {
  const win = fakeWindow("#token=old");
  takeTokenFromAddress(win);
  win.location.hash = "#token=new";
  assert.equal(takeTokenFromAddress(win), true);
  assert.equal(getToken(win), "new");
});

test("route hashes and an empty token are not taken", () => {
  const win = fakeWindow("#sessions");
  assert.equal(takeTokenFromAddress(win), false);
  assert.equal(win.location.hash, "#sessions");
  assert.equal(getToken(win), null);

  const empty = fakeWindow("#token=");
  assert.equal(takeTokenFromAddress(empty), false);
  assert.equal(getToken(empty), null);
  assert.equal(empty.location.hash, "");
});
