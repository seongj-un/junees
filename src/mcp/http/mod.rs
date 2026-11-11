//! The Streamable HTTP transport: every JSON-RPC message is its own POST
//! to the server's one endpoint, and a request's answer comes back as a
//! JSON object or an SSE stream scoped to that request.
//!
//! Both eras of the binding are spoken (MCP 2026-07-28, "Streamable HTTP",
//! "Backward Compatibility"):
//!
//! - Modern (2026-07-28): no session; each POST carries
//!   `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` and the
//!   `Mcp-Param-*` headers of [`meta`]. Closing the response stream
//!   cancels the request.
//! - Legacy (2025-03-26 to 2025-11-25): `initialize` may start a session
//!   (`Mcp-Session-Id`, sent on every later request and ended with
//!   DELETE); a 404 to a request carrying the session means it ended, and
//!   a new session is opened. The negotiated version goes in
//!   `MCP-Protocol-Version`. Cancelling sends `notifications/cancelled`,
//!   since a closed stream is not a cancellation there. Server requests
//!   on a response stream get `ping` answered and the rest refused.
//!
//! Not done: the standalone GET stream of server messages (legacy),
//! resuming a broken stream with `Last-Event-ID`, `subscriptions/listen`,
//! the deprecated HTTP+SSE transport of 2024-11-05, and OAuth (tokens go
//! in headers from the environment, [`target`]).
//!
//! Redirects are not followed, a response is read up to a size limit, and
//! error texts never carry the URL (its path or query may hold a token),
//! header values or the session id.

pub mod meta;
pub mod sse;
pub mod target;

pub use target::HttpConfig;
