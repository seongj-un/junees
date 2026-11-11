//! The transport a running server is reached over: its own process on
//! stdio ([`Connection`]) or Streamable HTTP ([`HttpConnection`]).

use std::time::Duration;

use serde_json::Value;

use super::conn::{CallError, Connection};
use super::http::HttpConnection;

pub enum Link {
    Stdio(Connection),
    Http(HttpConnection),
}

impl Link {
    /// `stdio` or `HTTP`, for `/mcp`.
    pub fn kind(&self) -> &'static str {
        match self {
            Link::Stdio(_) => "stdio",
            Link::Http(_) => "HTTP",
        }
    }

    /// Why the server can no longer be used, `None` while it can.
    pub fn closed(&self) -> Option<String> {
        match self {
            Link::Stdio(connection) => connection.closed(),
            Link::Http(connection) => connection.closed(),
        }
    }

    /// The server's process id, for a server Junees runs.
    pub fn pid(&self) -> Option<u32> {
        match self {
            Link::Stdio(connection) => Some(connection.pid()),
            Link::Http(_) => None,
        }
    }

    /// The last line a server process wrote on stderr.
    pub fn stderr_line(&self) -> Option<String> {
        match self {
            Link::Stdio(connection) => connection.stderr_line(),
            Link::Http(_) => None,
        }
    }

    /// Sends a request and waits up to `timeout` for its result; dropping
    /// the future cancels it. `headers` go only to a modern HTTP server
    /// (`Mcp-Param-*`).
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        headers: &[(String, String)],
    ) -> Result<Value, CallError> {
        match self {
            Link::Stdio(connection) => connection.request(method, params, timeout).await,
            Link::Http(connection) => connection.request(method, params, timeout, headers).await,
        }
    }

    pub async fn shutdown(&self) {
        match self {
            Link::Stdio(connection) => connection.shutdown().await,
            Link::Http(connection) => connection.shutdown().await,
        }
    }
}
