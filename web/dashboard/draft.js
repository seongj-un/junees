// The 글 생성 card's logic without the DOM (stage 23): what the `draft_level`
// and `draft_done` events add up to, and the words for them. `node --test`
// loads it.

/** The server cuts a section's text at 32,768 characters (`draft_level`). */
export const SECTION_CHARS = 32768;

const KIND_LABEL = {
  concept: "개념",
  gist: "요지",
  outline: "문단 개요",
  body: "본문",
  paragraph: "문단",
  polish: "이음",
};

/** The Korean name of a level's `kind`, or the raw value for a kind this page does not know. */
export function kindLabel(kind) {
  return KIND_LABEL[kind] ?? String(kind ?? "단계");
}

/** The kinds of a draft's levels in order: 2 levels, 3 (default) or 4, then the optional seam level. */
export function levelKinds(levels, polish) {
  const base = levels === 2 ? ["concept", "body"] : levels === 4 ? ["concept", "gist", "outline", "paragraph"] : ["concept", "gist", "body"];
  return polish ? [...base, "polish"] : base;
}

/**
 * What the model asked for, from the `draft` call's input text. The events
 * do not carry the level count, so it is read from the input when that is
 * still whole JSON (the server cuts it at 2,000 characters) and otherwise
 * taken as the default 3. Levels that arrive correct it, see {@link applyLevel}.
 */
export function requestedOptions(inputPreview) {
  let input = null;
  try {
    input = JSON.parse(String(inputPreview ?? ""));
  } catch {
    input = null;
  }
  const levels = [2, 3, 4].includes(input?.levels) ? input.levels : 3;
  return { levels, polish: input?.polish === true };
}

/** The request the model gave `draft`, for showing; the raw input text when it is not whole JSON. */
export function requestText(inputPreview) {
  try {
    const input = JSON.parse(String(inputPreview ?? ""));
    if (typeof input?.request === "string") return input.request;
  } catch {
    // Cut by the server: show what there is.
  }
  return String(inputPreview ?? "");
}

/** A fresh card state for a `draft` call whose input text is `inputPreview`. */
export function newDraft(inputPreview) {
  const { levels, polish } = requestedOptions(inputPreview);
  return { levels, polish, title: "", level: 0, kind: null, sections: [], done: null, ended: false, lost: false };
}

/**
 * Takes one `draft_level` event into `state`. Returns false for an event
 * that is malformed or already seen (a replay after reconnecting delivers
 * the same level again), true when the state changed.
 */
export function applyLevel(state, event) {
  const level = event?.level;
  if (!Number.isSafeInteger(level) || level < 1 || level <= state.level) return false;
  if (!Array.isArray(event.sections)) return false;
  const kind = typeof event.kind === "string" ? event.kind : null;
  // The kind of a level says how many levels the draft has: only a draft of
  // 4 has outlines and paragraphs, only a draft of 2 has a body at level 2.
  if (kind === "outline" || kind === "paragraph") state.levels = 4;
  else if (kind === "gist" && state.levels === 2) state.levels = 3;
  else if (kind === "body" && (level === 2 || level === 3)) state.levels = level;
  else if (kind === "polish") {
    state.polish = true;
    if (level - 1 >= 2 && level - 1 <= 4) state.levels = level - 1;
  }
  state.level = level;
  state.kind = kind;
  if (typeof event.title === "string" && event.title) state.title = event.title;
  for (const raw of event.sections) {
    if (typeof raw?.id !== "string") continue;
    const text = typeof raw.text === "string" ? raw.text : "";
    let section = state.sections.find((s) => s.id === raw.id);
    if (!section) {
      section = { id: raw.id, concept: "", text: "", kind, level, truncated: false };
      state.sections.push(section);
    }
    // The first level's text is the section's concept line, its heading from then on.
    if (level === 1) section.concept = text.split("\n")[0].trim();
    section.text = text;
    section.kind = kind;
    section.level = level;
    section.truncated = raw.truncated === true;
  }
  return true;
}

function count(value) {
  return Number.isFinite(value) && value >= 0 ? value : 0;
}

/**
 * Takes the `draft_done` event into `state`; false when it is not an
 * object. Unknown `stop` values are kept as they are.
 */
export function applyDone(state, event) {
  if (!event || typeof event !== "object") return false;
  const done = {
    stop: typeof event.stop === "string" ? event.stop : "done",
    error: typeof event.error === "string" && event.error ? event.error : null,
    levels: [2, 3, 4].includes(event.levels) ? event.levels : state.levels,
    levelsDone: count(event.levels_done),
    polished: event.polished === true,
    failedSections: Array.isArray(event.failed_sections) ? event.failed_sections.filter((id) => typeof id === "string") : [],
    calls: count(event.calls),
    inputTokens: count(event.input_tokens),
    outputTokens: count(event.output_tokens),
    cacheReadTokens: count(event.cache_read_tokens),
    elapsedMs: count(event.elapsed_ms),
    sequentialMs: count(event.sequential_ms),
    byLevel: (Array.isArray(event.by_level) ? event.by_level : [])
      .filter((row) => row && typeof row === "object")
      .map((row) => ({
        level: count(row.level),
        kind: typeof row.kind === "string" ? row.kind : null,
        calls: count(row.calls),
        failedCalls: count(row.failed_calls),
        inputTokens: count(row.input_tokens),
        outputTokens: count(row.output_tokens),
      })),
  };
  state.levels = done.levels;
  if (done.polished) state.polish = true;
  state.done = done;
  return true;
}

/**
 * The step list under the title: `{ number, kind, label, state }` for each
 * level, then the seam step without a number. `state` is `done`, `current`
 * (being made now), `todo` or, for a draft that stopped before it, `skipped`.
 */
export function steps(state) {
  const total = state.levels;
  // `ended`: the answer ended without a `draft_done`.
  const finished = state.done !== null || state.ended;
  return levelKinds(state.levels, state.polish).map((kind, i) => {
    const number = i + 1;
    let status;
    if (number <= state.level) status = "done";
    else if (finished) status = "skipped";
    else if (number === state.level + 1) status = "current";
    else status = "todo";
    return { number, kind, label: kindLabel(kind), state: status, total, seam: kind === "polish" };
  });
}

/** A step as read aloud and shown: `2/3 요지`, `이음`. */
export function stepText(step) {
  return step.seam ? step.label : `${step.number}/${step.total} ${step.label}`;
}

/** The line that says how far a running draft has come (the card's live region). */
export function progressText(state) {
  if (state.done) return summaryLine(state.done);
  if (state.ended && state.lost) return "단계 이벤트를 받지 못해 요약을 알 수 없습니다. 새로 고치면 기록에서 다시 그립니다.";
  if (state.ended) return "글 생성이 끝나기 전에 이 답이 끝났습니다. 끝난 단계까지의 글만 남았습니다.";
  if (state.level === 0) return "글의 뼈대를 만드는 중입니다.";
  const all = steps(state);
  const last = all.find((s) => s.number === state.level);
  const following = all.find((s) => s.state === "current");
  const head = last?.seam ? "이음까지 끝났습니다" : `${state.level}/${state.levels}단계 ${kindLabel(state.kind)} 완료`;
  const count = state.sections.length;
  return following ? `${head} (절 ${count}개). 다음 단계: ${following.label}` : `${head} (절 ${count}개). 마무리하는 중입니다.`;
}

/** `7.0초` from milliseconds; `1분 5초` from a minute on. */
export function formatSeconds(ms) {
  const seconds = Math.max(0, ms) / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)}초`;
  const whole = Math.round(seconds);
  const minutes = Math.floor(whole / 60);
  return whole % 60 ? `${minutes}분 ${whole % 60}초` : `${minutes}분`;
}

const number = new Intl.NumberFormat("ko-KR");

/**
 * The one line under a finished card: calls, tokens, cache reads, time and,
 * for a draft that ended normally and gained from running at once, what one
 * call at a time would have taken.
 */
export function summaryLine(done) {
  const input = `입력 ${number.format(done.inputTokens)}${done.cacheReadTokens > 0 ? ` (캐시 읽기 ${number.format(done.cacheReadTokens)})` : ""}`;
  const parts = [
    `호출 ${number.format(done.calls)}번`,
    `${input} · 출력 ${number.format(done.outputTokens)} 토큰`,
    `걸린 시간 ${formatSeconds(done.elapsedMs)}`,
  ];
  // The estimate adds only the calls that finished, while the time spent includes the
  // level a cancel or a token limit threw away: after such a stop, or when the estimate is
  // not longer than the time spent, the comparison would read backwards.
  if (done.stop === "done" && done.sequentialMs > done.elapsedMs) parts.push(`순차였다면 약 ${formatSeconds(done.sequentialMs)}`);
  return parts.join(", ");
}

/** What a stop other than `done` means, in a sentence; null when it finished. */
export function stopNote(done) {
  const reached = `${done.levelsDone}/${done.levels}단계`;
  switch (done.stop) {
    case "done":
      return null;
    case "cancelled":
      return done.levelsDone > 0
        ? `취소해서 ${reached}까지만 만들었습니다. 끝나지 않은 단계의 글은 버렸고, 끝난 단계까지의 글이 남았습니다.`
        : "취소해서 글을 만들기 전에 멈췄습니다.";
    case "limit":
      return done.levelsDone > 0
        ? `이 실행의 토큰 상한에 닿아 ${reached}에서 멈췄습니다. 남은 호출은 보내지 않았고, 끝난 단계까지의 글이 남았습니다.`
        : "이 실행의 토큰 상한에 닿아 글을 만들지 못했습니다.";
    case "failed":
      return done.levelsDone > 0
        ? `글을 끝까지 만들지 못했습니다. ${reached}까지의 글이 남았습니다.`
        : "글을 만들지 못했습니다. 뼈대조차 만들지 못해 남은 글이 없습니다.";
    default:
      return `글 생성이 ${done.stop} 상태로 끝났습니다.`;
  }
}

/** Where the card shows a failed section: what stands in for it. */
export const FAILED_SECTION_NOTE = "생성에 실패해 앞 단계의 글로 대신했습니다.";

/** Whether `section` of a finished card is one the server named in `failed_sections`. */
export function sectionFailed(state, id) {
  return state.done?.failedSections.includes(id) === true;
}

/** The label of the table row for a level: `2단계 요지`. */
export function levelRowLabel(row) {
  return `${row.level}단계 ${kindLabel(row.kind)}`;
}

// ---- the tool result (after a reload the events do not come again) ----
//
// The session API sends what a `draft` call came to as its own `draft` object on the
// tool result (same fields as `draft_done`, `sequential_ms` and `by_level` null when
// not known), and pairs a call with its result by `tool_use_id`. Nothing about the
// state is read from the text the model wrote.

/** The `draft` object of a tool result block, or null (a result the server could not read a status from). */
export function resultStatus(block) {
  const draft = block?.draft;
  return draft && typeof draft === "object" && !Array.isArray(draft) ? draft : null;
}

/**
 * The text of a result as `{ head, omitted, tail }`. A result the server cut
 * has its first 2,000 characters (`text_preview`) and its last 500
 * (`text_tail`) with `omitted_chars` between them: `omitted` is that number,
 * `null` when the tail or the number is missing (then nothing after `head`
 * is known). With nothing omitted the two parts are one text. The status
 * line Junees puts at the end of a result is cut off when the server read a
 * status from it (`draft` is not null), without reading it.
 */
export function resultText(block) {
  const known = resultStatus(block) !== null;
  const clean = (text) => (known ? withoutStatusLine(text) : text);
  const preview = String(block?.text_preview ?? "");
  if (block?.truncated !== true) return { head: clean(preview), omitted: 0, tail: "" };
  const tail = typeof block.text_tail === "string" ? block.text_tail : null;
  const omitted = Number.isSafeInteger(block.omitted_chars) && block.omitted_chars >= 0 ? block.omitted_chars : null;
  if (tail === null || omitted === null) return { head: preview, omitted: null, tail: "" };
  if (omitted === 0) return { head: clean(preview + tail), omitted: 0, tail: "" };
  return { head: preview, omitted, tail: clean(tail) };
}

/** `text` without its last line when that is Junees's `[draft] ...` status line, and the `---` before it. */
function withoutStatusLine(text) {
  const at = text.lastIndexOf("\n");
  if (at < 0 || !text.slice(at + 1).startsWith("[draft] ")) return text;
  const before = text.slice(0, at);
  return before.endsWith("\n---") ? before.slice(0, -4) : before;
}

/**
 * The Markdown a draft result holds as `{ title, sections: [{ concept, text,
 * failedId }] }`: `# Title`, then `## concept` and text per section, with a
 * `> [s2] ...` line closing a section that had a failure. Text before the
 * first heading is ignored; a result without `# ` gives no sections.
 */
export function parseArticle(article) {
  const lines = String(article ?? "").split("\n");
  const first = lines.findIndex((line) => line.startsWith("# "));
  if (first < 0) return { title: "", sections: [] };
  const title = lines[first].slice(2).trim();
  const sections = [];
  let current = null;
  for (const line of lines.slice(first + 1)) {
    if (line.startsWith("## ")) {
      current = { concept: line.slice(3).trim(), lines: [], failedId: null };
      sections.push(current);
    } else if (current) {
      const note = /^> \[([^\]]+)\] /.exec(line);
      if (note) current.failedId = note[1];
      else current.lines.push(line);
    }
  }
  return {
    title,
    sections: sections.map((s) => ({ concept: s.concept, text: s.lines.join("\n").trim(), failedId: s.failedId })),
  };
}

// ---- the model switch line (stage 22) ----

const SWITCH_REASON = {
  // The words of `src/provider/fallback/switch.rs`, which the terminal and the gateway use.
  // `unavailable` is a 5xx answer or no answer at all.
  rate_limit: "사용 한도로 막혀",
  overloaded: "과부하로 막혀",
  unavailable: "서버 오류나 연결 실패로 막혀",
};

/** Why the model changed, as the start of the notice; a raw reason for a code this page does not know. */
export function switchReason(reason) {
  return SWITCH_REASON[reason] ?? `사유(${String(reason ?? "알 수 없음")})로`;
}

/** The notice's headline: why, and that the chat goes on with the fallback model. */
export function switchHeadline(event) {
  return `${switchReason(event?.reason)} 예비 모델로 이어 갑니다`;
}

/**
 * When the main model may be tried again, from `retry_at`, relative to
 * `now` (a Date), to follow the label `다시 시도: `; null when the server
 * sent none. The first call of the first turn that starts after it goes to
 * the main model.
 */
export function retryText(retryAt, now) {
  const at = typeof retryAt === "string" ? new Date(retryAt) : null;
  if (!at || Number.isNaN(at.getTime())) return null;
  const seconds = Math.round((at.getTime() - now.getTime()) / 1000);
  if (seconds <= 0) return "지금부터 가능";
  if (seconds < 45) return "곧";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}분 뒤`;
  const hours = Math.floor(minutes / 60);
  return minutes % 60 ? `${hours}시간 ${minutes % 60}분 뒤` : `${hours}시간 뒤`;
}

/**
 * Which tool results of a session's messages are a `draft` call's, as
 * `"<message index>:<block number>"`: the results whose `tool_use_id` is the
 * id of a `draft` `tool_use` in the list, and the results that carry the
 * server's `draft` field (it is there only on the result of a `draft` call,
 * `null` when no status could be read). The second rule is for a call that
 * is not in the list: the chat screen gets only the last 20 messages, so the
 * first one can be a result whose call lies before the window. A call or
 * result without an id (a session answer from before the ids) is left out, and
 * so is a result whose call is not in the list and that has no `draft` field;
 * its block stays a plain tool block.
 */
export function draftResultKeys(messages) {
  const keys = new Set();
  const list = Array.isArray(messages) ? messages : [];
  const ids = new Set();
  for (const message of list) {
    for (const block of Array.isArray(message?.blocks) ? message.blocks : []) {
      if (block?.type === "tool_use" && block.name === "draft" && typeof block.tool_use_id === "string" && block.tool_use_id) {
        ids.add(block.tool_use_id);
      }
    }
  }
  for (const message of list) {
    (Array.isArray(message?.blocks) ? message.blocks : []).forEach((block, n) => {
      if (block?.type !== "tool_result") return;
      const paired = typeof block.tool_use_id === "string" && ids.has(block.tool_use_id);
      if (paired || "draft" in block) keys.add(`${message.index}:${n}`);
    });
  }
  return keys;
}
