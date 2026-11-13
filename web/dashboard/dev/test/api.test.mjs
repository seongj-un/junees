// Run with: node --test web/dashboard/dev/test/
import assert from "node:assert/strict";
import { test } from "node:test";

import { ApiError, exchangeCode, getToken, takeCodeFromAddress, takeTokenFromAddress } from "../../api.js";

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

test("takeCodeFromAddress takes #code= once and clears the address", () => {
  const win = fakeWindow("#code=one-TIME_code");
  assert.equal(takeCodeFromAddress(win), "one-TIME_code");
  assert.equal(win.location.hash, "");
  assert.equal(getToken(win), null, "a code is not a token");
  assert.equal(takeCodeFromAddress(fakeWindow("#/sessions")), null);
  assert.equal(takeCodeFromAddress(fakeWindow("#token=abc")), null);
});

test("exchangeCode keeps the token it gets for the code, without a Bearer", async () => {
  const win = fakeWindow("");
  const sent = [];
  globalThis.fetch = async (path, init) => {
    sent.push({ path, init });
    return new Response(JSON.stringify({ token: "tok" }), { status: 200 });
  };
  assert.equal(await exchangeCode("c0de", win), "tok");
  assert.equal(getToken(win), "tok");
  assert.equal(sent[0].path, "/api/token");
  assert.equal(sent[0].init.method, "POST");
  assert.equal(sent[0].init.body, JSON.stringify({ code: "c0de" }));
  assert.equal(sent[0].init.headers.Authorization, undefined);
  assert.equal(sent[0].init.headers["Content-Type"], "application/json");
});

test("a refused code is a 401 bad_code, not a lock of the tab's token", async () => {
  const win = fakeWindow("");
  globalThis.fetch = async () => new Response(JSON.stringify({ error: "bad_code" }), { status: 401 });
  await assert.rejects(exchangeCode("old", win), (err) => {
    assert.ok(err instanceof ApiError);
    assert.deepEqual([err.kind, err.status, err.code], ["http", 401, "bad_code"]);
    return true;
  });
  assert.equal(getToken(win), null);
});
