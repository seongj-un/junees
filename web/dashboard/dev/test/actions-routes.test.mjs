// Run with: node --test web/dashboard/dev/test/
import assert from "node:assert/strict";
import { test } from "node:test";

import { actionFor, describeActionError } from "../../actions.js";
import { ApiError } from "../../api.js";
import { parseHash, sessionHash } from "../../routes.js";
import { firstLine, mergePages, roleLabel } from "../../view-session.js";

test("parseHash reads screens, old links and session addresses", () => {
  assert.deepEqual(parseHash(""), { name: "status", args: {}, canonical: "#/status" });
  assert.deepEqual(parseHash("#cron"), { name: "cron", args: {}, canonical: "#/cron" });
  assert.deepEqual(parseHash("#/webhooks"), { name: "webhooks", args: {}, canonical: "#/webhooks" });
  assert.deepEqual(parseHash("#/sessions/20261113-0841-3f2a"), {
    name: "session",
    args: { id: "20261113-0841-3f2a" },
    canonical: "#/sessions/20261113-0841-3f2a",
  });
  assert.equal(parseHash("#/nowhere").canonical, "#/status");
  assert.equal(parseHash("#/status/extra").canonical, "#/status");
});

test("parseHash never turns a bad id into a session address", () => {
  for (const hash of ["#/sessions/..%2Fetc", "#/sessions/a%2Fb", "#/sessions/%E0%A4%A", "#/sessions/", `#/sessions/${"a".repeat(65)}`, "#/sessions/a/b"]) {
    assert.notEqual(parseHash(hash).name, "session", hash);
  }
  assert.equal(sessionHash("abc_DEF-1"), "#/sessions/abc_DEF-1");
});

test("actionFor offers no resume for an item paused for the home folder", () => {
  assert.equal(actionFor({ paused: false }), "pause");
  assert.equal(actionFor({ paused: true }), "resume");
  assert.equal(actionFor({ paused: true, paused_for_home: true }), null);
});

test("describeActionError says what each refusal means", () => {
  const conflict = describeActionError(new ApiError("http", "run `junees cron allow-home c3`", { status: 409 }));
  assert.equal(conflict.detail, "run `junees cron allow-home c3`");
  assert.deepEqual([conflict.outcome, conflict.retry], ["stay", false]);

  const forbidden = describeActionError(new ApiError("http", "no origin", { status: 403 }));
  assert.match(forbidden.text, /출처/);
  assert.equal(forbidden.detail, null);
  assert.equal(forbidden.retry, false);

  assert.equal(describeActionError(new ApiError("unauthorized", "x", { status: 401 })).outcome, "lock");
  assert.equal(describeActionError(new ApiError("http", "", { status: 404 })).outcome, "reload");

  const flaky = describeActionError(new ApiError("timeout", "timed out"));
  assert.equal(flaky.retry, true);
  assert.match(flaky.text, /응답하지 않았습니다/);
});

test("mergePages keeps order and drops repeats", () => {
  const first = [{ index: 0 }, { index: 1 }];
  const later = [{ index: 1 }, { index: 3 }, { index: 2 }];
  assert.deepEqual(
    mergePages(first, later).map((m) => m.index),
    [0, 1, 2, 3],
  );
});

test("roleLabel names a user turn of tool results as one", () => {
  assert.equal(roleLabel({ role: "user", blocks: [{ type: "text" }] }), "사용자");
  assert.equal(roleLabel({ role: "user", blocks: [{ type: "tool_result" }, { type: "tool_result" }] }), "도구 결과");
  assert.equal(roleLabel({ role: "assistant", blocks: [] }), "어시스턴트");
  assert.equal(roleLabel({ role: "robot" }), "robot");
});

test("firstLine skips blank lines", () => {
  assert.equal(firstLine("\n\n  cargo test -q\nmore"), "cargo test -q");
  assert.equal(firstLine(null), "");
});
