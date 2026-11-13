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

/**
 * Reads a one-time `#code=...` from the address (the browser `junees
 * dashboard` opens gets one instead of the token) and clears the address
 * at once. Returns the code, or null. Trade it with {@link exchangeCode}.
 */
export function takeCodeFromAddress(win = window) {
  const hash = win.location.hash.replace(/^#/, "");
  if (!/(^|&)code=/.test(hash)) return null;
  const code = new URLSearchParams(hash).get("code");
  win.history.replaceState(null, "", win.location.pathname + win.location.search);
  return code || null;
}

/**
 * Trades a one-time code for the token (`POST /api/token`, no Bearer) and
 * keeps the token for this tab. A used, wrong or expired code throws an
 * {@link ApiError} with `status` 401 and `code` `bad_code`.
 */
export async function exchangeCode(code, win = window) {
  const answer = await request("POST", "/api/token", { auth: false, payload: { code } });
  if (typeof answer?.token !== "string" || answer.token === "") {
    throw new ApiError("parse", "no token in the answer");
  }
  win.sessionStorage.setItem(TOKEN_KEY, answer.token);
  return answer.token;
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
export function getJson(path, options = {}) {
  return request("GET", path, options);
}

/**
 * POSTs an empty JSON object to `path` (the dashboard's write requests take
 * no body) and returns the parsed answer. The browser adds `Origin`, which
 * the server requires on writes. Throws like {@link getJson}.
 */
export function postJson(path, options = {}) {
  return request("POST", path, options);
}

async function request(method, path, { signal, timeoutMs = DEFAULT_TIMEOUT_MS, auth = true, payload = {} } = {}) {
  const token = auth ? getToken() : null;
  if (auth && !token) throw new ApiError("unauthorized", "no token");

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
        method,
        headers: {
          ...(auth ? { Authorization: `Bearer ${token}` } : {}),
          Accept: "application/json",
          ...(method === "POST" ? { "Content-Type": "application/json" } : {}),
        },
        body: method === "POST" ? JSON.stringify(payload) : undefined,
        cache: "no-store",
        credentials: "omit",
        signal: controller.signal,
      });
    } catch (err) {
      if (timedOut) throw new ApiError("timeout", "timed out");
      if (signal?.aborted) throw err;
      throw new ApiError("network", "network error");
    }

    if (response.status === 401 && auth) {
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
