// Screen addresses: `#/status`, `#/sessions`, `#/sessions/<id>` and so on.
// No DOM here, so `node --test` can load it.

export const SCREENS = ["status", "sessions", "usage", "cron", "webhooks", "doctor"];

/** The session id format the server accepts (`[A-Za-z0-9_-]{1,64}`). */
export const SESSION_ID = /^[A-Za-z0-9_-]{1,64}$/;

/**
 * The screen a hash points at: `{ name, args, canonical }`. `canonical` is
 * the hash the address should show, so old `#sessions` links and unknown
 * hashes are rewritten to `#/sessions` and `#/status`.
 */
export function parseHash(hash) {
  const path = String(hash ?? "")
    .replace(/^#/, "")
    .replace(/^\/+/, "");
  const [first = "", second, ...rest] = path.split("/");
  if (first === "sessions" && second !== undefined && rest.length === 0) {
    let id = second;
    try {
      id = decodeURIComponent(second);
    } catch {
      id = "";
    }
    if (SESSION_ID.test(id)) {
      return { name: "session", args: { id }, canonical: sessionHash(id) };
    }
    return { name: "sessions", args: {}, canonical: "#/sessions" };
  }
  const name = SCREENS.includes(first) && second === undefined ? first : "status";
  return { name, args: {}, canonical: `#/${name}` };
}

/** The address of one session's screen. */
export function sessionHash(id) {
  return `#/sessions/${encodeURIComponent(id)}`;
}
