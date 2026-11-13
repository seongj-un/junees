// File checkpoints of a chat (stage 20): what the server's checkpoint list
// and undo answers mean, in words, with no DOM so `node --test` covers it.
//
// A checkpoint number (`turn` in GET .../checkpoints and POST .../undo) is
// not the chat's turn number: the session counts only the turns that
// changed files, from 1, across reopenings. A live turn that left one
// says so in `turn_end.checkpoint`.

import { ApiError } from "./api.js";

/** How a turn changed a file, from `Change::word` in src/checkpoints/mod.rs. */
export const CHANGE_LABEL = { created: "새 파일", modified: "수정", deleted: "삭제" };

export function changeLabel(change) {
  return CHANGE_LABEL[change] ?? String(change ?? "알 수 없음");
}

/** `path` under the chat's working folder, relative to it; any other path whole. */
export function relPath(path, cwd) {
  const text = String(path ?? "");
  if (typeof cwd !== "string" || !cwd) return text;
  const base = cwd.replace(/\/+$/, "");
  return base && text.startsWith(`${base}/`) ? text.slice(base.length + 1) : text;
}

/**
 * The checkpoint list as the server sent it, checked and oldest first:
 * `[{ turn, createdAt, files: [{ path, change, restorable }], undid }]`.
 * Items without a usable number are dropped. `undid` is the list of turns
 * an undo undid, when the server says so; this server version does not.
 */
export function parseCheckpoints(body) {
  if (!Array.isArray(body)) return [];
  return body
    .filter((item) => item && Number.isSafeInteger(item.turn) && item.turn > 0)
    .map((item) => ({
      turn: item.turn,
      createdAt: typeof item.created_at === "string" ? item.created_at : null,
      files: (Array.isArray(item.files) ? item.files : [])
        .filter((file) => file && typeof file.path === "string")
        .map((file) => ({ path: file.path, change: file.change, restorable: file.restorable === true })),
      undid: Array.isArray(item.undid) ? item.undid.filter(Number.isSafeInteger) : [],
    }))
    .sort((a, b) => a.turn - b.turn);
}

/**
 * What undoing from checkpoint `from` touches: that checkpoint and every
 * later one, newest first (the order the server undoes them in), and how
 * many of their files come back as they were and how many stay as they are.
 */
export function undoPlan(list, from) {
  const turns = list.filter((item) => item.turn >= from).sort((a, b) => b.turn - a.turn);
  const files = turns.flatMap((item) => item.files);
  const back = files.filter((file) => file.restorable).length;
  return { turns, back, kept: files.length - back };
}

/** A message the person wrote (not a turn of tool results). */
function personMessage(message) {
  const blocks = Array.isArray(message?.blocks) ? message.blocks : [];
  return message?.role === "user" && blocks.some((b) => b.type === "text") && !blocks.some((b) => b.type === "tool_result");
}

/**
 * Ties the checkpoints to the turns of a session history: a checkpoint
 * belongs to the turn whose span (from the person's message to the last
 * message before the next one) holds its `created_at`. An undo is done
 * between turns, after the last message of one, so it falls in none.
 * Returns `Map(index of the turn's last message -> [checkpoint numbers])`;
 * checkpoints that fit no shown turn (older, or a message with no time)
 * are left out and appear only in the full list.
 */
export function linkHistory(messages, list) {
  const spans = [];
  const shown = Array.isArray(messages) ? messages : [];
  for (const message of shown) {
    const at = Date.parse(message?.at ?? "");
    if (personMessage(message)) spans.push({ start: at, end: at, last: message });
    else if (spans.length) Object.assign(spans[spans.length - 1], { end: at, last: message });
  }
  const links = new Map();
  for (const item of list) {
    if (item.undid.length) continue;
    const at = Date.parse(item.createdAt ?? "");
    if (!Number.isFinite(at)) continue;
    const span = spans.find((s) => Number.isFinite(s.start) && Number.isFinite(s.end) && s.start <= at && at <= s.end);
    if (!span) continue;
    const key = span.last.index;
    links.set(key, [...(links.get(key) ?? []), item.turn]);
  }
  return links;
}

/**
 * The undo answer in words: `{ headline, restored: [{ path, partial }],
 * skipped: [{ path, reason }] }`. `partial` says how far back a file went
 * when an older change to it had to stay.
 */
export function undoOutcome(body) {
  const partial = new Map(
    (Array.isArray(body?.partial) ? body.partial : [])
      .filter((p) => p && typeof p.path === "string")
      .map((p) => [p.path, p]),
  );
  const restored = (Array.isArray(body?.restored) ? body.restored : [])
    .filter((path) => typeof path === "string")
    .map((path) => ({ path, partial: partial.has(path) ? partialText(partial.get(path)) : null }));
  const skipped = (Array.isArray(body?.skipped) ? body.skipped : [])
    .filter((s) => s && typeof s.path === "string")
    .map((s) => ({ path: s.path, reason: typeof s.reason === "string" && s.reason ? s.reason : "이유 없음" }));
  const some = restored.filter((r) => r.partial).length;
  let headline;
  if (restored.length === 0) {
    headline = skipped.length ? `되돌린 파일이 없습니다. 파일 ${skipped.length}개는 그대로 두었습니다.` : "되돌린 파일이 없습니다.";
  } else if (skipped.length === 0) {
    headline = `파일 ${restored.length}개를 되돌렸습니다.`;
  } else {
    headline = `파일 ${restored.length}개를 되돌리고, ${skipped.length}개는 그대로 두었습니다.`;
  }
  if (some && some === restored.length && skipped.length === 0) headline = `파일 ${some}개를 일부만 되돌렸습니다.`;
  else if (some) headline += ` 그중 ${some}개는 일부만 되돌렸습니다.`;
  return { headline, restored, skipped };
}

/** "변경 기록 4의 변경까지만 되돌렸습니다. 변경 기록 3의 변경은 그대로 두었습니다: <이유>" */
export function partialText(part) {
  return `변경 기록 ${part.until_turn}의 변경까지만 되돌렸습니다. 변경 기록 ${part.skipped_turn}의 변경은 그대로 두었습니다: ${part.reason ?? "이유 없음"}`;
}

/**
 * What to say when an undo fails: `{ text, detail, retry, after }`, where
 * `after` is `stay`, `reload` (the list may be out of date), `unknown`
 * (no answer: the files may have been undone) or `off` (the server keeps
 * no checkpoints).
 */
export function describeUndoError(err) {
  if (err instanceof ApiError && err.kind === "http") {
    if (err.status === 409 && err.code === "checkpoints_off") {
      return {
        text: "이 대시보드 서버는 파일 체크포인트를 끈 채(checkpoints = false) 실행 중이라 되돌릴 수 없습니다. 바뀐 파일은 없습니다.",
        detail: null,
        retry: false,
        after: "off",
      };
    }
    if (err.status === 409) {
      return {
        text: "답을 만드는 중인 대화가 있어 되돌리지 않았습니다. 바뀐 파일은 없습니다. 답이 끝난 뒤 다시 시도해 주세요.",
        detail: null,
        retry: false,
        after: "stay",
      };
    }
    if (err.status === 404) {
      return {
        text: "이 기록부터 되돌릴 변경을 서버에서 찾지 못했습니다. 창을 닫으면 파일 변경 기록을 새로 불러옵니다.",
        detail: null,
        retry: false,
        after: "reload",
      };
    }
    if (err.status === 403) {
      return {
        text: "서버가 이 요청의 출처를 확인하지 못해 거절했습니다. 터미널에 나온 주소로 연 탭에서 다시 시도해 주세요.",
        detail: null,
        retry: false,
        after: "stay",
      };
    }
    return {
      text: err.status >= 500 ? `서버가 체크포인트를 읽지 못해 되돌리지 못했습니다 (HTTP ${err.status}).` : `서버가 요청을 받지 않았습니다 (HTTP ${err.status}).`,
      detail: err.message || null,
      retry: err.status >= 500,
      after: "stay",
    };
  }
  if (err instanceof ApiError && (err.kind === "timeout" || err.kind === "network" || err.kind === "parse")) {
    // The server may have undone the files anyway: no retry, and the
    // buttons of these checkpoints wait until the list says what happened.
    return {
      text: "서버의 답을 받지 못했습니다. 파일을 이미 되돌렸을 수 있습니다. 창을 닫으면 파일 변경 기록을 다시 받아 확인하고, 확인될 때까지 이 기록들의 되돌리기 버튼을 막아 둡니다.",
      detail: null,
      retry: false,
      after: "unknown",
    };
  }
  return { text: "되돌리지 못했습니다.", detail: err?.message ?? null, retry: false, after: "stay" };
}

/**
 * The lines `[되돌림] ...` the server puts before the next message after an
 * undo, so the model knows the files changed, and the message itself.
 */
export function splitUndoNotes(text) {
  const lines = String(text ?? "").split("\n");
  const notes = [];
  while (lines.length && lines[0].startsWith("[되돌림] ")) notes.push(lines.shift().slice("[되돌림] ".length));
  if (!notes.length) return { notes, rest: String(text ?? "") };
  while (lines.length && lines[0] === "") lines.shift();
  return { notes, rest: lines.join("\n") };
}

/**
 * Which checkpoints an undo has put back and not been undone itself:
 * `Map(checkpoint -> the undo's checkpoint)`. Newest first, so undoing an
 * undo brings its turns back.
 */
export function undoneBy(list) {
  const by = new Map();
  for (const item of [...list].sort((a, b) => b.turn - a.turn)) {
    if (by.has(item.turn)) continue;
    for (const turn of item.undid) if (turn < item.turn && !by.has(turn)) by.set(turn, item.turn);
  }
  return by;
}
