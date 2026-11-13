// The chat screen's logic without the DOM: the event stream parser and the
// words for its events. `node --test` loads it.

/** The server closes a question after this long without an answer. */
export const APPROVAL_TIMEOUT_MS = 5 * 60 * 1000;

/** The server sends `: keepalive` every 15 s; this long without a byte means the stream is dead. */
export const STREAM_SILENCE_MS = 45 * 1000;

/** The longest message the server takes. */
export const MAX_TEXT = 32000;

/**
 * Parses `text/event-stream` text pushed in pieces of any size. Calls
 * `onEvent({ id, data })` for each event that has a `data:` field; comment
 * lines (`: keepalive`) and unknown fields are skipped.
 */
export function createSseParser(onEvent) {
  let buffer = "";
  let id = null;
  let data = [];

  const dispatch = () => {
    if (data.length) onEvent({ id, data: data.join("\n") });
    id = null;
    data = [];
  };

  const line = (text) => {
    if (text === "") return dispatch();
    if (text.startsWith(":")) return;
    const colon = text.indexOf(":");
    const field = colon < 0 ? text : text.slice(0, colon);
    let value = colon < 0 ? "" : text.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "data") data.push(value);
    else if (field === "id") id = value;
  };

  return {
    push(chunk) {
      buffer += chunk;
      let at;
      // A line ends with \n, \r\n or a lone \r; keep a trailing \r until
      // the next piece shows whether \n follows.
      while ((at = buffer.search(/\r\n|\n|\r(?=[^\n])/)) >= 0) {
        const end = buffer[at] === "\r" && buffer[at + 1] === "\n" ? 2 : 1;
        line(buffer.slice(0, at));
        buffer = buffer.slice(at + end);
      }
    },
  };
}

/**
 * One stream event as `{ seq, type, ... }`, or null when its data is not a
 * JSON object with a string `type`. `seq` comes from the `id:` line.
 */
export function parseEvent({ id, data }) {
  let body;
  try {
    body = JSON.parse(data);
  } catch {
    return null;
  }
  if (!body || typeof body !== "object" || typeof body.type !== "string") return null;
  const seq = Number(id);
  return { ...body, seq: Number.isSafeInteger(seq) && id !== "" && id !== null ? seq : null };
}

/** Waits before the `attempt`-th reconnection (0 first): 1, 2, 4, 8, then 10 s. */
export function reconnectDelay(attempt) {
  return Math.min(1000 * 2 ** Math.max(0, attempt), 10000);
}

/** Why a message cannot be sent, or null when it can. */
export function messageProblem(text) {
  const trimmed = String(text ?? "").trim();
  if (!trimmed) return "보낼 내용을 입력해 주세요.";
  if (text.length > MAX_TEXT) return `메시지는 ${MAX_TEXT.toLocaleString("ko-KR")}자까지 보낼 수 있습니다. 지금 ${text.length.toLocaleString("ko-KR")}자입니다.`;
  return null;
}

/** When a question closes: the server's `expires_at` if it sent one, else 5 minutes after it arrived. */
export function approvalDeadline(event, receivedAt) {
  const sent = Date.parse(event?.expires_at ?? "");
  return Number.isFinite(sent) ? sent : receivedAt + APPROVAL_TIMEOUT_MS;
}

/** "4분 12초 남음" for the time left until `deadline`. */
export function formatRemaining(deadline, now) {
  const left = Math.max(0, Math.ceil((deadline - now) / 1000));
  if (left === 0) return "곧 거절됩니다";
  const minutes = Math.floor(left / 60);
  const seconds = left % 60;
  if (minutes === 0) return `${seconds}초 남음`;
  return seconds ? `${minutes}분 ${seconds}초 남음` : `${minutes}분 남음`;
}

export const ANSWER_LABEL = { y: "허용", a: "항상 허용", n: "거절" };

/** The answers a question offers, in the order shown (`a` only when sent). */
export function answerChoices(options) {
  const given = Array.isArray(options) ? options : ["y", "n"];
  return ["y", "a", "n"].filter((answer) => given.includes(answer));
}

/** What became of a question, from `approval_closed`. */
export function closedText(answer, by) {
  if (by === "timeout") return "정해진 시간 안에 답이 없어 거절했습니다.";
  // The person stopped the turn while it waited; they did not say no.
  if (by === "cancel") return "답하기 전에 취소해서 실행하지 않았습니다.";
  switch (answer) {
    case "y":
      return "허용했습니다.";
    case "a":
      return "허용했습니다. 이 대화에서는 같은 종류를 다시 묻지 않습니다.";
    case "n":
      return "거절했습니다.";
    default:
      return `닫혔습니다 (${answer ?? "답 없음"}).`;
  }
}

const TOOL_LABEL = {
  bash: "명령 실행",
  write_file: "파일 쓰기",
  edit_file: "파일 고치기",
  read_file: "파일 읽기",
};

/** A Korean name for a tool or question kind, with the raw name when it is unknown. */
export function toolLabel(name) {
  return TOOL_LABEL[name] ?? String(name ?? "도구");
}

/** A note for how a turn ended, or null when it simply finished. */
export function stopReasonText(reason) {
  switch (reason) {
    case undefined:
    case null:
    case "end_turn":
    case "stop_sequence":
    // The error event just before it says what went wrong.
    case "error":
      return null;
    case "max_turns":
      return "한 번에 실행할 수 있는 단계 수의 한도에 닿아 여기서 멈췄습니다. 이어서 보내면 계속합니다.";
    case "cancelled":
    case "canceled":
    case "interrupted":
      return "취소해서 이 답은 여기서 멈췄습니다.";
    case "max_tokens":
      return "답이 길어 한도에서 끊겼습니다.";
    case "refusal":
      return "모델이 이 요청에 답하지 않았습니다.";
    case "pause_turn":
      return "모델이 답을 잠시 멈췄습니다. 이어서 보내면 계속합니다.";
    default:
      return `이 답은 ${reason} 상태로 끝났습니다.`;
  }
}

/** Whether the server was started with `--chat` (from `/api/status`). */
export function chatEnabled(status) {
  return status?.chat === true;
}

/**
 * The events of a replayed stream (all it sent on connecting, before the
 * page shows them) that the session history does not hold yet: turns
 * without their `turn_end`. A turn that ended is in the session's journal,
 * which the page shows above the chat, so drawing it again would show it
 * twice. Returns `{ show, running }`; `running` says a turn is still going.
 */
export function unfinishedTurns(events) {
  const show = [];
  let part = [];
  let ended = false;
  const flush = () => {
    if (!ended) show.push(...part);
  };
  for (const event of events) {
    if (event.type === "turn_start") {
      flush();
      part = [event];
      ended = false;
    } else {
      part.push(event);
      if (event.type === "turn_end") ended = true;
    }
  }
  flush();
  return { show, running: show.length > 0 };
}

/**
 * Session messages up to the last thing the person wrote, for when a turn
 * is still running: what the journal holds after that message belongs to
 * the running turn, which the stream draws in full.
 */
export function beforeRunningTurn(messages) {
  const wrote = (m) => m?.role === "user" && Array.isArray(m.blocks) && m.blocks.some((b) => b?.type === "text");
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (wrote(messages[i])) return messages.slice(0, i + 1);
  }
  return messages;
}

/** What to say when the event stream does not open, by status; null for a plain drop. */
export function streamProblem(err) {
  if (err?.status === 429) {
    return "대화 화면이 열린 탭이 너무 많아 연결하지 못했습니다. 다른 탭의 대화 화면을 닫아 주세요.";
  }
  if (err?.status === 503) return "서버가 지금 대화를 받을 수 없습니다.";
  return null;
}
