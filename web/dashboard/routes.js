// Screen addresses: `#/status`, `#/sessions`, `#/sessions/<id>` and so on.
// No DOM here, so `node --test` can load it.

export const SCREENS = ["status", "chat", "sessions", "usage", "cron", "webhooks", "doctor"];

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
  if ((first === "sessions" || first === "chat") && second !== undefined && rest.length === 0) {
    const id = decodeId(second);
    if (first === "chat") {
      return id ? { name: "chat", args: { id }, canonical: chatHash(id) } : { name: "chat", args: {}, canonical: "#/chat" };
    }
    if (id) return { name: "session", args: { id }, canonical: sessionHash(id) };
    return { name: "sessions", args: {}, canonical: "#/sessions" };
  }
  const name = SCREENS.includes(first) && second === undefined ? first : "status";
  return { name, args: {}, canonical: `#/${name}` };
}

/** A session id from an address part, or null when it is not one. */
function decodeId(part) {
  let id;
  try {
    id = decodeURIComponent(part);
  } catch {
    return null;
  }
  return SESSION_ID.test(id) ? id : null;
}

/** The address of one session's screen. */
export function sessionHash(id) {
  return `#/sessions/${encodeURIComponent(id)}`;
}

/** The address of the chat that goes on with session `id` (`#/chat` for a new one). */
export function chatHash(id) {
  return id ? `#/chat/${encodeURIComponent(id)}` : "#/chat";
}
