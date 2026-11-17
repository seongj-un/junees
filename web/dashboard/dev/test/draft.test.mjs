import assert from "node:assert/strict";
import test from "node:test";

import {
  applyDone,
  applyLevel,
  draftResultKeys,
  kindLabel,
  levelKinds,
  newDraft,
  parseArticle,
  parseDraftResult,
  progressText,
  requestText,
  requestedOptions,
  retryText,
  sectionFailed,
  stepText,
  steps,
  stopNote,
  summaryLine,
  switchHeadline,
} from "../../draft.js";

const level = (n, kind, texts, extra = {}) => ({
  type: "draft_level",
  level: n,
  kind,
  title: "배포 가이드",
  sections: texts.map((text, i) => ({ id: `s${i + 1}`, text, truncated: false })),
  ...extra,
});

const done = (extra = {}) => ({
  type: "draft_done",
  stop: "done",
  error: null,
  levels: 3,
  levels_done: 3,
  polished: false,
  failed_sections: [],
  calls: 9,
  input_tokens: 11207,
  output_tokens: 3936,
  cache_read_tokens: 0,
  elapsed_ms: 7000,
  sequential_ms: 25200,
  by_level: [
    { level: 1, kind: "concept", calls: 1, failed_calls: 0, input_tokens: 274, output_tokens: 96 },
    { level: 2, kind: "gist", calls: 4, failed_calls: 0, input_tokens: 2850, output_tokens: 640 },
    { level: 3, kind: "body", calls: 4, failed_calls: 0, input_tokens: 8083, output_tokens: 3200 },
  ],
  ...extra,
});

test("the requested levels come from whole JSON input and default to 3", () => {
  assert.deepEqual(requestedOptions('{"request":"보고서","levels":4,"polish":true}'), { levels: 4, polish: true });
  assert.deepEqual(requestedOptions('{"request":"보고서"}'), { levels: 3, polish: false });
  // Cut at 2,000 characters by the server: not JSON, so the default.
  assert.deepEqual(requestedOptions('{"request":"긴 요청...'), { levels: 3, polish: false });
  assert.deepEqual(requestedOptions('{"request":"x","levels":7,"polish":"yes"}'), { levels: 3, polish: false });
  assert.deepEqual(requestedOptions(null), { levels: 3, polish: false });
});

test("each level replaces the text of its section cells", () => {
  const state = newDraft('{"request":"x"}');
  assert.equal(applyLevel(state, level(1, "concept", ["준비물", "설치", "배포"])), true);
  assert.deepEqual(state.sections.map((s) => [s.id, s.concept, s.text, s.kind]), [
    ["s1", "준비물", "준비물", "concept"],
    ["s2", "설치", "설치", "concept"],
    ["s3", "배포", "배포", "concept"],
  ]);
  assert.equal(state.title, "배포 가이드");
  assert.equal(applyLevel(state, level(2, "gist", ["요지 하나", "요지 둘", "요지 셋"])), true);
  assert.equal(applyLevel(state, level(3, "body", ["본문 하나", "본문 둘", "본문 셋"])), true);
  // The concept stays the heading; the text is the newest level's.
  assert.deepEqual(state.sections.map((s) => [s.concept, s.text, s.kind, s.level]), [
    ["준비물", "본문 하나", "body", 3],
    ["설치", "본문 둘", "body", 3],
    ["배포", "본문 셋", "body", 3],
  ]);
});

test("a level seen twice or older than the last is ignored", () => {
  const state = newDraft("{}");
  applyLevel(state, level(1, "concept", ["가"]));
  applyLevel(state, level(2, "gist", ["나"]));
  assert.equal(applyLevel(state, level(2, "gist", ["다"])), false);
  assert.equal(applyLevel(state, level(1, "concept", ["라"])), false);
  assert.equal(state.sections[0].text, "나");
  assert.equal(applyLevel(state, { type: "draft_level", level: 3 }), false);
  assert.equal(applyLevel(state, { type: "draft_level", level: "3", sections: [] }), false);
  assert.equal(applyLevel(state, null), false);
});

test("the kind of a level corrects the level count read from a cut input", () => {
  const four = newDraft("{cut");
  applyLevel(four, level(1, "concept", ["a"]));
  applyLevel(four, level(2, "gist", ["a"]));
  assert.equal(four.levels, 3);
  applyLevel(four, level(3, "outline", ["1. a"]));
  assert.equal(four.levels, 4);
  assert.deepEqual(steps(four).map((s) => s.kind), ["concept", "gist", "outline", "paragraph"]);

  const two = newDraft("{cut");
  applyLevel(two, level(1, "concept", ["a"]));
  applyLevel(two, level(2, "body", ["b"]));
  assert.equal(two.levels, 2);

  const polished = newDraft("{cut");
  applyLevel(polished, level(1, "concept", ["a"]));
  applyLevel(polished, level(2, "gist", ["a"]));
  applyLevel(polished, level(3, "body", ["a"]));
  assert.equal(polished.polish, false);
  applyLevel(polished, level(4, "polish", ["a"]));
  assert.equal(polished.polish, true);
  assert.equal(polished.levels, 3);
});

test("steps mark what is done, what is being made and what waits", () => {
  const state = newDraft('{"polish":true}');
  assert.deepEqual(steps(state).map((s) => [stepText(s), s.state]), [
    ["1/3 개념", "current"],
    ["2/3 요지", "todo"],
    ["3/3 본문", "todo"],
    ["이음", "todo"],
  ]);
  applyLevel(state, level(1, "concept", ["a", "b"]));
  applyLevel(state, level(2, "gist", ["a", "b"]));
  assert.deepEqual(steps(state).map((s) => s.state), ["done", "done", "current", "todo"]);
  // A cancelled draft leaves the steps it did not reach as skipped, not as "being made".
  applyDone(state, done({ stop: "cancelled", levels_done: 2 }));
  assert.deepEqual(steps(state).map((s) => s.state), ["done", "done", "skipped", "skipped"]);
});

test("the progress line names the finished level and the one that follows", () => {
  const state = newDraft("{}");
  assert.equal(progressText(state), "글의 뼈대를 만드는 중입니다.");
  applyLevel(state, level(1, "concept", ["a", "b", "c", "d"]));
  assert.equal(progressText(state), "1/3단계 개념 완료 (절 4개). 다음 단계: 요지");
  applyLevel(state, level(2, "gist", ["a", "b", "c", "d"]));
  assert.equal(progressText(state), "2/3단계 요지 완료 (절 4개). 다음 단계: 본문");
  applyLevel(state, level(3, "body", ["a", "b", "c", "d"]));
  assert.equal(progressText(state), "3/3단계 본문 완료 (절 4개). 마무리하는 중입니다.");
  applyDone(state, done());
  assert.equal(progressText(state), summaryLine(state.done));
});

test("the summary line has calls, tokens, cache reads, time and the sequential estimate", () => {
  const state = newDraft("{}");
  applyDone(state, done());
  assert.equal(summaryLine(state.done), "호출 9번, 입력 11,207 · 출력 3,936 토큰, 걸린 시간 7.0초, 순차였다면 약 25.2초");
  applyDone(state, done({ cache_read_tokens: 4096, elapsed_ms: 75000, sequential_ms: 0 }));
  assert.equal(summaryLine(state.done), "호출 9번, 입력 11,207 (캐시 읽기 4,096) · 출력 3,936 토큰, 걸린 시간 1분 15초");
});

test("done takes only the fields it understands and keeps unknown stops", () => {
  const state = newDraft("{}");
  assert.equal(applyDone(state, null), false);
  assert.equal(state.done, null);
  applyDone(state, { type: "draft_done", stop: "paused", failed_sections: ["s2", 5], by_level: [null, { level: 2, kind: "gist", calls: 4 }] });
  assert.equal(state.done.stop, "paused");
  assert.deepEqual(state.done.failedSections, ["s2"]);
  assert.equal(state.done.calls, 0);
  assert.deepEqual(state.done.byLevel.map((r) => [r.level, r.calls, r.failedCalls]), [[2, 4, 0]]);
  assert.equal(stopNote(state.done), "글 생성이 paused 상태로 끝났습니다.");
});

test("each stop has its own note with the level it reached", () => {
  const state = newDraft("{}");
  applyDone(state, done());
  assert.equal(stopNote(state.done), null);
  applyDone(state, done({ stop: "cancelled", levels_done: 2 }));
  assert.match(stopNote(state.done), /^취소해서 2\/3단계까지만 만들었습니다\./);
  applyDone(state, done({ stop: "cancelled", levels_done: 0 }));
  assert.equal(stopNote(state.done), "취소해서 글을 만들기 전에 멈췄습니다.");
  applyDone(state, done({ stop: "limit", levels_done: 2, error: "token limit reached" }));
  assert.match(stopNote(state.done), /^이 실행의 토큰 상한에 닿아 2\/3단계에서 멈췄습니다\./);
  assert.equal(state.done.error, "token limit reached");
  applyDone(state, done({ stop: "failed", levels_done: 0, error: "the plan was not valid" }));
  assert.match(stopNote(state.done), /^글을 만들지 못했습니다\./);
});

test("failed sections are the ones the server names", () => {
  const state = newDraft("{}");
  applyLevel(state, level(1, "concept", ["a", "b"]));
  assert.equal(sectionFailed(state, "s2"), false);
  applyDone(state, done({ failed_sections: ["s2"] }));
  assert.equal(sectionFailed(state, "s2"), true);
  assert.equal(sectionFailed(state, "s1"), false);
});

test("a section text is kept as text and flagged when the server cut it", () => {
  const state = newDraft("{}");
  applyLevel(state, {
    type: "draft_level",
    level: 1,
    kind: "concept",
    title: "<img src=x onerror=alert(1)>",
    sections: [{ id: "s1", text: "<b>굵게</b>", truncated: true }],
  });
  assert.equal(state.title, "<img src=x onerror=alert(1)>");
  assert.equal(state.sections[0].text, "<b>굵게</b>");
  assert.equal(state.sections[0].truncated, true);
});

test("kind labels cover every kind and keep an unknown one", () => {
  assert.deepEqual(["concept", "gist", "outline", "body", "paragraph", "polish"].map(kindLabel), ["개념", "요지", "문단 개요", "본문", "문단", "이음"]);
  assert.equal(kindLabel("sketch"), "sketch");
  assert.deepEqual(levelKinds(2, false), ["concept", "body"]);
});

const STATUS =
  "[draft] levels 3/3, 3 sections, failed sections: none, 9 calls, 11207 input and 3936 output tokens, 7.0s (about 25.2s one call at a time)";

test("a finished tool result splits into the text and the status line", () => {
  const text = `# 배포 가이드\n\n## 준비물\n\n필요한 것.\n\n## 설치\n\n설치 방법.\n---\n${STATUS}`;
  const parsed = parseDraftResult(text);
  assert.equal(parsed.stop, "done");
  assert.equal(parsed.error, null);
  assert.ok(parsed.article.startsWith("# 배포 가이드"));
  assert.ok(!parsed.article.includes("[draft]"));
  assert.deepEqual(parsed.status, {
    levelsDone: 3,
    levels: 3,
    polished: false,
    failedSections: [],
    calls: 9,
    inputTokens: 11207,
    outputTokens: 3936,
    cacheReadTokens: 0,
    elapsedMs: 7000,
    sequentialMs: 25200,
  });
});

test("failed sections and polish are read from the status line", () => {
  const line =
    "[draft] levels 3/3 + polish, 6 sections, failed sections: s2, s5 (an earlier level's text stands in, marked in the text), 20 calls, 100 input and 50 output tokens, 1.5s (about 4.0s one call at a time)";
  const parsed = parseDraftResult(`# T\n\n## a\n\nx\n---\n${line}`);
  assert.deepEqual(parsed.status.failedSections, ["s2", "s5"]);
  assert.equal(parsed.status.polished, true);
});

test("cancelled, limit and failed results are told apart by their first line", () => {
  const cancelled = parseDraftResult(`cancelled by the user; the levels finished before the cancel:\n\n# T\n\n## a\n\nx\n---\n${STATUS}`);
  assert.equal(cancelled.stop, "cancelled");
  assert.ok(cancelled.article.startsWith("# T"));
  const limit = parseDraftResult(`draft stopped: token limit reached (cron_max_tokens); the levels finished before:\n\n# T\n\n## a\n\nx\n---\n${STATUS}`);
  assert.equal(limit.stop, "limit");
  assert.equal(limit.error, "token limit reached (cron_max_tokens)");
  const failed = parseDraftResult(`draft failed: the plan was not valid JSON\n${STATUS}`);
  assert.equal(failed.stop, "failed");
  assert.equal(failed.error, "the plan was not valid JSON");
  assert.equal(failed.article, "");
  assert.equal(failed.status.levels, 3);
});

test("a result the server cut has no status line and still gives its text", () => {
  const parsed = parseDraftResult("# 배포 가이드\n\n## 준비물\n\n필요한 것");
  assert.equal(parsed.status, null);
  assert.equal(parsed.stop, "done");
  assert.ok(parsed.article.startsWith("# 배포 가이드"));
  // A status line in a shape this page does not know is not guessed at.
  assert.equal(parseDraftResult("# T\n---\n[draft] levels ?").status, null);
});

test("the article splits into sections and takes failure notes out of the text", () => {
  const article = "# 배포 가이드\n\n## 준비물\n\n필요한 것.\n\n둘째 문단.\n\n## 설치\n\n설치 방법.\n\n> [s2] 이 절은 3단계 생성에 실패해 앞 단계의 글을 대신 넣었습니다.\n";
  const parsed = parseArticle(article);
  assert.equal(parsed.title, "배포 가이드");
  assert.deepEqual(parsed.sections, [
    { concept: "준비물", text: "필요한 것.\n\n둘째 문단.", failedId: null },
    { concept: "설치", text: "설치 방법.", failedId: "s2" },
  ]);
  assert.deepEqual(parseArticle("그냥 글"), { title: "", sections: [] });
});

test("the retry time is relative to now and never a negative number", () => {
  const now = new Date("2026-11-17T09:00:00Z");
  assert.equal(retryText("2026-11-17T09:15:00Z", now), "15분 뒤");
  assert.equal(retryText("2026-11-17T09:00:20Z", now), "곧");
  assert.equal(retryText("2026-11-17T08:59:00Z", now), "지금부터 가능");
  assert.equal(retryText("2026-11-17T11:30:00Z", now), "2시간 30분 뒤");
  assert.equal(retryText("2026-11-17T11:00:00Z", now), "2시간 뒤");
  assert.equal(retryText(null, now), null);
  assert.equal(retryText("not a time", now), null);
});

test("the switch headline names why and keeps an unknown reason", () => {
  assert.equal(switchHeadline({ reason: "rate_limit" }), "사용 한도로 막혀 예비 모델로 이어 갑니다");
  assert.equal(switchHeadline({ reason: "overloaded" }), "서버가 붐벼 예비 모델로 이어 갑니다");
  assert.equal(switchHeadline({ reason: "unavailable" }), "응답이 없어 예비 모델로 이어 갑니다");
  assert.equal(switchHeadline({ reason: "quota" }), "사유(quota)로 예비 모델로 이어 갑니다");
});

test("an answer that ends without draft_done leaves its steps skipped", () => {
  const state = newDraft("{}");
  applyLevel(state, level(1, "concept", ["a"]));
  state.ended = true;
  assert.deepEqual(steps(state).map((s) => s.state), ["done", "skipped", "skipped"]);
  assert.match(progressText(state), /^글 생성이 끝나기 전에 이 답이 끝났습니다\./);
});

test("draft results are paired with their calls by order, never by their text", () => {
  const use = (name) => ({ type: "tool_use", name, input_preview: "{}" });
  const result = (text) => ({ type: "tool_result", is_error: false, text_preview: text });
  const messages = [
    { index: 0, role: "user", blocks: [{ type: "text", text: "보고서를 써 줘" }] },
    { index: 1, role: "assistant", blocks: [{ type: "text", text: "쓰겠습니다" }, use("read_file"), use("draft")] },
    { index: 2, role: "user", blocks: [result("# not a draft\n---\n[draft] levels 3/3"), result("# T")] },
    { index: 3, role: "assistant", blocks: [use("draft")] },
    { index: 4, role: "user", blocks: [result("# T")] },
    // A call whose results are not in the list (another page) or are fewer: no guess.
    { index: 5, role: "assistant", blocks: [use("draft"), use("bash")] },
    { index: 6, role: "user", blocks: [result("x")] },
    { index: 8, role: "assistant", blocks: [use("draft")] },
  ];
  assert.deepEqual([...draftResultKeys(messages)].sort(), ["2:1", "4:0"]);
  assert.deepEqual([...draftResultKeys(null)], []);
});

test("the request is shown as the model wrote it, not as JSON", () => {
  assert.equal(requestText('{"request":"배포 가이드를 써 줘\\n표 포함","levels":3}'), "배포 가이드를 써 줘\n표 포함");
  assert.equal(requestText('{"request":"cut'), '{"request":"cut');
  assert.equal(requestText(undefined), "");
});
