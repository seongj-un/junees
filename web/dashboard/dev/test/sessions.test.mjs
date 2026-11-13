// Run with: node --test web/dashboard/dev/test/
import assert from "node:assert/strict";
import { test } from "node:test";

// view-sessions.js imports dom.js, which only touches `document` when called.
import { previewAfterTitle } from "../../view-sessions.js";

test("previewAfterTitle drops the title the preview starts with", () => {
  assert.equal(previewAfterTitle("Fix the login bug", "Fix the login bug\nmy key is [hidden]"), "my key is [hidden]");
  assert.equal(previewAfterTitle("README 확인", "README 확인"), "");
  assert.equal(previewAfterTitle("다른 제목", "미리보기 글"), "미리보기 글");
  assert.equal(previewAfterTitle(null, "미리보기 글"), "미리보기 글");
  assert.equal(previewAfterTitle("제목", null), "");
});
