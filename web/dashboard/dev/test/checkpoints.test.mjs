import assert from "node:assert/strict";
import test from "node:test";

import { ApiError } from "../../api.js";
import {
  changeLabel,
  describeUndoError,
  linkHistory,
  parseCheckpoints,
  partialText,
  relPath,
  splitUndoNotes,
  undoOutcome,
  undoPlan,
} from "../../checkpoints.js";

const LIST = parseCheckpoints([
  {
    turn: 2,
    created_at: "2026-11-13T06:05:10Z",
    files: [{ path: "/w/b.txt", change: "created", restorable: true }],
  },
  {
    turn: 1,
    created_at: "2026-11-13T06:01:05Z",
    files: [
      { path: "/w/a.txt", change: "created", restorable: true },
      { path: "/w/src/main.rs", change: "modified", restorable: false },
    ],
  },
  { turn: 0, files: [] },
  { turn: "3", files: [] },
]);

test("the checkpoint list is checked and put oldest first", () => {
  assert.deepEqual(
    LIST.map((item) => item.turn),
    [1, 2],
  );
  assert.equal(LIST[0].createdAt, "2026-11-13T06:01:05Z");
  assert.deepEqual(LIST[0].undid, []);
  assert.equal(LIST[0].files[1].restorable, false);
  assert.deepEqual(parseCheckpoints({ turns: [] }), []);
  assert.deepEqual(parseCheckpoints([{ turn: 4, files: [{ path: "/x", change: "deleted", restorable: "yes" }] }])[0].files[0].restorable, false);
});

test("changes are named in Korean, unknown ones as sent", () => {
  assert.equal(changeLabel("created"), "새 파일");
  assert.equal(changeLabel("modified"), "수정");
  assert.equal(changeLabel("deleted"), "삭제");
  assert.equal(changeLabel("renamed"), "renamed");
});

test("paths under the working folder are shown from it, others whole", () => {
  assert.equal(relPath("/w/src/main.rs", "/w"), "src/main.rs");
  assert.equal(relPath("/w/src/main.rs", "/w/"), "src/main.rs");
  assert.equal(relPath("/wx/a.txt", "/w"), "/wx/a.txt");
  assert.equal(relPath("/w/a.txt", null), "/w/a.txt");
});

test("an undo plan takes that checkpoint and every later one, newest first", () => {
  const plan = undoPlan(LIST, 1);
  assert.deepEqual(
    plan.turns.map((item) => item.turn),
    [2, 1],
  );
  assert.equal(plan.back, 2);
  assert.equal(plan.kept, 1);
  assert.deepEqual(
    undoPlan(LIST, 2).turns.map((item) => item.turn),
    [2],
  );
});

const HISTORY = [
  { index: 10, role: "user", at: "2026-11-13T06:01:00Z", blocks: [{ type: "text", text: "a.txt 만들어 줘" }] },
  { index: 11, role: "assistant", at: "2026-11-13T06:01:04Z", blocks: [{ type: "tool_use", name: "write_file" }] },
  { index: 12, role: "user", at: "2026-11-13T06:01:05Z", blocks: [{ type: "tool_result", text_preview: "wrote" }] },
  { index: 13, role: "assistant", at: "2026-11-13T06:01:07Z", blocks: [{ type: "text", text: "만들었습니다" }] },
  { index: 14, role: "user", at: "2026-11-13T06:03:00Z", blocks: [{ type: "text", text: "고마워" }] },
  { index: 15, role: "assistant", at: "2026-11-13T06:03:02Z", blocks: [{ type: "text", text: "네" }] },
  { index: 16, role: "user", at: "2026-11-13T06:05:00Z", blocks: [{ type: "text", text: "b.txt 만들어 줘" }] },
  { index: 17, role: "assistant", at: "2026-11-13T06:05:12Z", blocks: [{ type: "text", text: "만들었습니다" }] },
];

test("a checkpoint sits on the turn whose span holds its time", () => {
  const links = linkHistory(HISTORY, LIST);
  assert.deepEqual([...links], [
    [13, [1]],
    [17, [2]],
  ]);
});

test("a checkpoint made between turns (an undo) or older than the history is not tied", () => {
  const list = parseCheckpoints([
    { turn: 1, created_at: "2026-11-13T05:00:00Z", files: [] },
    { turn: 3, created_at: "2026-11-13T06:02:00Z", files: [] },
    { turn: 4, created_at: "2026-11-13T06:05:11Z", files: [], undid: [2] },
  ]);
  assert.equal(linkHistory(HISTORY, list).size, 0);
});

test("a message with no time ties nothing", () => {
  const history = HISTORY.map((m) => ({ ...m, at: null }));
  assert.equal(linkHistory(history, LIST).size, 0);
  assert.equal(linkHistory(null, LIST).size, 0);
});

test("the undo answer is told in words, with how far a file went back", () => {
  const outcome = undoOutcome({
    restored: ["/w/a.txt", "/w/c.txt"],
    skipped: [{ path: "/w/src/main.rs", reason: "바뀐 뒤 수정됨" }],
    partial: [{ path: "/w/c.txt", until_turn: 4, skipped_turn: 3, reason: "턴이 만든 파일이 그 뒤 수정되어 지우지 않음" }],
  });
  assert.equal(outcome.headline, "파일 2개를 되돌리고, 1개는 그대로 두었습니다. 그중 1개는 일부만 되돌렸습니다.");
  assert.equal(outcome.restored[0].partial, null);
  assert.equal(
    outcome.restored[1].partial,
    "변경 기록 4의 변경까지만 되돌렸습니다. 변경 기록 3의 변경은 그대로 두었습니다: 턴이 만든 파일이 그 뒤 수정되어 지우지 않음",
  );
  assert.deepEqual(outcome.skipped, [{ path: "/w/src/main.rs", reason: "바뀐 뒤 수정됨" }]);
  assert.equal(undoOutcome({ restored: ["/a"], skipped: [], partial: [] }).headline, "파일 1개를 되돌렸습니다.");
  assert.equal(undoOutcome({ restored: [], skipped: [{ path: "/a", reason: "" }] }).headline, "되돌린 파일이 없습니다. 파일 1개는 그대로 두었습니다.");
  assert.equal(undoOutcome({ restored: [], skipped: [{ path: "/a", reason: "" }] }).skipped[0].reason, "이유 없음");
  assert.equal(undoOutcome(null).headline, "되돌린 파일이 없습니다.");
  assert.match(partialText({ until_turn: 2, skipped_turn: 1 }), /이유 없음$/);
});

test("undo failures say what happened and what comes next", () => {
  const http = (status, code, message = "") => new ApiError("http", message, { status, code });
  assert.equal(describeUndoError(http(409, "checkpoints_off")).after, "off");
  const running = describeUndoError(http(409, "turn_running"));
  assert.equal(running.after, "stay");
  assert.equal(running.retry, false);
  assert.match(running.text, /바뀐 파일은 없습니다/);
  assert.equal(describeUndoError(http(404, "not_found")).after, "reload");
  const broken = describeUndoError(http(500, "undo_failed", "cannot read the file checkpoints"));
  assert.equal(broken.retry, true);
  assert.equal(broken.detail, "cannot read the file checkpoints");
  assert.equal(describeUndoError(http(400, "bad_body")).retry, false);
  const lost = describeUndoError(new ApiError("timeout", "slow"));
  assert.equal(lost.after, "reload");
  assert.match(lost.text, /확인하지 못했습니다/);
});

test("the undo lines before a message are taken apart from it", () => {
  const text = "[되돌림] 턴 1의 파일 변경을 되돌렸습니다: /w/a.txt. bash로 바꾼 파일은 기록되지 않아 되돌리지 않았습니다.\n[되돌림] 둘째\n\n이어서 해 줘";
  assert.deepEqual(splitUndoNotes(text), {
    notes: ["턴 1의 파일 변경을 되돌렸습니다: /w/a.txt. bash로 바꾼 파일은 기록되지 않아 되돌리지 않았습니다.", "둘째"],
    rest: "이어서 해 줘",
  });
  assert.deepEqual(splitUndoNotes("그냥 [되돌림] 글"), { notes: [], rest: "그냥 [되돌림] 글" });
  assert.deepEqual(splitUndoNotes(undefined), { notes: [], rest: "" });
});
