import assert from "node:assert/strict";
import test from "node:test";

import {
  answerChoices,
  approvalDeadline,
  chatEnabled,
  closedText,
  createSseParser,
  formatRemaining,
  messageProblem,
  parseEvent,
  reconnectDelay,
  stopReasonText,
  toolLabel,
} from "../../chat-stream.js";
import { chatHash, parseHash } from "../../routes.js";

function collect(pieces) {
  const got = [];
  const parser = createSseParser((event) => got.push(event));
  for (const piece of pieces) parser.push(piece);
  return got;
}

test("the SSE parser reads events split anywhere and skips keepalives", () => {
  const text = 'id: 1\ndata: {"type":"text","delta":"안녕"}\n\n: keepalive\n\nid: 2\r\ndata: {"type":"turn_end"}\r\n\r\n';
  const whole = collect([text]);
  assert.deepEqual(whole, [
    { id: "1", data: '{"type":"text","delta":"안녕"}' },
    { id: "2", data: '{"type":"turn_end"}' },
  ]);
  // One character at a time, including a \r\n split in two.
  assert.deepEqual(collect([...text]), whole);
  // An event is not dispatched until its blank line arrives.
  assert.deepEqual(collect(['id: 3\ndata: {"type":"text"}\n']), []);
});

test("the SSE parser joins multi-line data and ignores other fields", () => {
  assert.deepEqual(collect(["event: x\nretry: 10\ndata: a\ndata: b\n\n"]), [{ id: null, data: "a\nb" }]);
  assert.deepEqual(collect(["data:no-space\n\n"]), [{ id: null, data: "no-space" }]);
});

test("parseEvent keeps the seq and rejects what is not an event", () => {
  assert.deepEqual(parseEvent({ id: "7", data: '{"type":"text","delta":"x"}' }), { type: "text", delta: "x", seq: 7 });
  assert.equal(parseEvent({ id: null, data: '{"type":"usage"}' }).seq, null);
  assert.equal(parseEvent({ id: "1", data: "not json" }), null);
  assert.equal(parseEvent({ id: "1", data: '{"delta":"no type"}' }), null);
  assert.equal(parseEvent({ id: "1", data: "[1]" }), null);
});

test("reconnecting waits 1, 2, 4, 8, then 10 seconds", () => {
  assert.deepEqual([0, 1, 2, 3, 4, 9].map(reconnectDelay), [1000, 2000, 4000, 8000, 10000, 10000]);
});

test("messageProblem refuses blank and too long messages", () => {
  assert.match(messageProblem("  \n"), /입력/);
  assert.equal(messageProblem("빌드해 줘"), null);
  assert.equal(messageProblem("가".repeat(32000)), null);
  assert.match(messageProblem("가".repeat(32001)), /32,000자/);
});

test("a question closes at expires_at, or 5 minutes after it arrived", () => {
  assert.equal(approvalDeadline({ expires_at: "2026-11-13T09:05:00Z" }, 0), Date.parse("2026-11-13T09:05:00Z"));
  assert.equal(approvalDeadline({}, 1000), 1000 + 300000);
  assert.equal(approvalDeadline({ expires_at: "soon" }, 0), 300000);
});

test("formatRemaining counts down in Korean", () => {
  assert.equal(formatRemaining(300000, 0), "5분 남음");
  assert.equal(formatRemaining(252000, 0), "4분 12초 남음");
  assert.equal(formatRemaining(9500, 0), "10초 남음");
  assert.equal(formatRemaining(0, 5), "곧 거절됩니다");
});

test("answers keep the y, a, n order and offer a only when sent", () => {
  assert.deepEqual(answerChoices(["n", "a", "y"]), ["y", "a", "n"]);
  assert.deepEqual(answerChoices(["y", "n"]), ["y", "n"]);
  assert.deepEqual(answerChoices(undefined), ["y", "n"]);
});

test("closed questions, stop reasons and tool names read naturally", () => {
  assert.match(closedText("n", "timeout"), /5분/);
  assert.equal(closedText("y", "browser"), "허용했습니다.");
  assert.match(closedText("a", "browser"), /다시 묻지 않습니다/);
  assert.equal(stopReasonText("end_turn"), null);
  assert.match(stopReasonText("cancelled"), /취소/);
  assert.match(stopReasonText("something_new"), /something_new/);
  assert.equal(toolLabel("bash"), "명령 실행");
  assert.equal(toolLabel("mcp__github__create_issue"), "mcp__github__create_issue");
});

test("chat is on only when status says true", () => {
  assert.equal(chatEnabled({ chat: true }), true);
  assert.equal(chatEnabled({ chat: "yes" }), false);
  assert.equal(chatEnabled({}), false);
});

test("chat addresses: new, going on with a session, and a bad id", () => {
  assert.deepEqual(parseHash("#/chat"), { name: "chat", args: {}, canonical: "#/chat" });
  assert.deepEqual(parseHash("#/chat/20261113-0920-1c4e"), {
    name: "chat",
    args: { id: "20261113-0920-1c4e" },
    canonical: "#/chat/20261113-0920-1c4e",
  });
  assert.deepEqual(parseHash("#/chat/..%2Fx"), { name: "chat", args: {}, canonical: "#/chat" });
  assert.equal(chatHash("a_b"), "#/chat/a_b");
  assert.equal(chatHash(null), "#/chat");
});
