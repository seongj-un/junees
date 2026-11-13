// Talking to the `junees dashboard` server: the token from the address,
// and GET requests that always end (answer, error or time limit).

const TOKEN_KEY = "junees.dashboard.token";
const DEFAULT_TIMEOUT_MS = 10000;

/** Why a request failed, for the screen to pick its message. */
export class ApiError extends Error {
  /**
   * @param {"unauthorized"|"network"|"timeout"|"http"|"parse"} kind
   * @param {string} message
   * @param {{status?: number, code?: string|null}} [extra]
   */
  constructor(kind, message, extra = {}) {
    super(message);
    this.name = "ApiError";
    this.kind = kind;
    this.status = extra.status ?? null;
    this.code = extra.code ?? null;
  }
}

/**
 * Reads `#token=...` from the address, keeps it for this tab (replacing an
 * older one) and takes it out of the address so it is not left in history
 * or on screen. Called on load and on every hash change, because a
 * restarted server keeps the same origin and the user may paste its new
 * address into the open tab. Returns whether a token was taken.
 */
export function takeTokenFromAddress(win = window) {
  const hash = win.location.hash.replace(/^#/, "");
  if (!hash.includes("token=")) return false;
  const token = new URLSearchParams(hash).get("token");
  if (token) win.sessionStorage.setItem(TOKEN_KEY, token);
  win.history.replaceState(null, "", win.location.pathname + win.location.search);
  return Boolean(token);
}

export function getToken(win = window) {
  return win.sessionStorage.getItem(TOKEN_KEY);
}

export function forgetToken(win = window) {
  win.sessionStorage.removeItem(TOKEN_KEY);
}

/**
 * GETs `path` and returns the parsed JSON body.
 * Throws {@link ApiError}; a 401 or a missing token is `unauthorized`.
 */
export async function getJson(path, { signal, timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
  const token = getToken();
  if (!token) throw new ApiError("unauthorized", "no token");

  const controller = new AbortController();
  const onAbort = () => controller.abort(signal.reason);
  if (signal) {
    if (signal.aborted) controller.abort(signal.reason);
    else signal.addEventListener("abort", onAbort, { once: true });
  }
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);

  try {
    let response;
    try {
      response = await fetch(path, {
        method: "GET",
        headers: { Authorization: `Bearer ${token}`, Accept: "application/json" },
        cache: "no-store",
        credentials: "omit",
        signal: controller.signal,
      });
    } catch (err) {
      if (timedOut) throw new ApiError("timeout", "timed out");
      if (signal?.aborted) throw err;
      throw new ApiError("network", "network error");
    }

    if (response.status === 401) {
      throw new ApiError("unauthorized", "unauthorized", { status: 401 });
    }

    let body;
    try {
      body = await response.json();
    } catch {
      if (timedOut) throw new ApiError("timeout", "timed out");
      if (signal?.aborted) throw signal.reason;
      throw new ApiError("parse", "not json", { status: response.status });
    }

    if (!response.ok) {
      const code = typeof body?.error === "string" ? body.error : null;
      const message = typeof body?.message === "string" ? body.message : "";
      throw new ApiError("http", message, { status: response.status, code });
    }
    return body;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", onAbort);
  }
}

/** A Korean sentence for a failed request (not used for `unauthorized`). */
export function describeError(err) {
  if (!(err instanceof ApiError)) return "알 수 없는 오류로 불러오지 못했습니다.";
  switch (err.kind) {
    case "network":
      return "대시보드 서버에 연결할 수 없습니다. 터미널에서 junees dashboard가 아직 실행 중인지 확인해 주세요.";
    case "timeout":
      return "서버가 제때 응답하지 않았습니다. 잠시 뒤 다시 시도해 주세요.";
    case "parse":
      return "서버 응답을 읽지 못했습니다.";
    case "http":
      return `서버가 요청을 처리하지 못했습니다 (HTTP ${err.status}).`;
    default:
      return "불러오지 못했습니다.";
  }
}
