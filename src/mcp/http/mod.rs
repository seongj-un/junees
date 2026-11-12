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

#[cfg(test)]
mod tests;

use std::error::Error as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};

use super::conn::{CallError, RpcError};
use super::server::{self, Era, MODERN_VERSION};
pub use target::HttpConfig;

/// How long connecting to the server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a notification, a reply to the server or the closing DELETE
/// may take.
const SHORT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bytes of an error response read to find a JSON-RPC error in it.
const ERROR_BODY_BYTES: usize = 64 * 1024;
/// Modern errors other than `UnsupportedProtocolVersion` (-32022): a
/// server answering with one speaks 2026-07-28 and must not be asked to
/// fall back to `initialize`.
const HEADER_MISMATCH: i64 = -32020;
const MISSING_CLIENT_CAPABILITY: i64 = -32021;
const UNSUPPORTED_VERSION: i64 = -32022;

/// Why an HTTP exchange gave no result.
#[derive(Debug)]
enum HttpError {
    /// A JSON-RPC error, in a success response (`status` none) or in the
    /// body of an error response.
    Rpc {
        error: RpcError,
        status: Option<StatusCode>,
    },
    /// An error status without a JSON-RPC error in its body.
    Status(StatusCode),
    /// Anything else, described without the URL or header values.
    Other(String),
}

impl HttpError {
    fn into_call_error(self) -> CallError {
        match self {
            HttpError::Rpc { error, .. } => CallError::Rpc(error),
            HttpError::Status(status) => CallError::Failed(status_text(status)),
            HttpError::Other(reason) => CallError::Failed(reason),
        }
    }

    /// The HTTP error status of the response, whatever its body held.
    fn status(&self) -> Option<StatusCode> {
        match self {
            HttpError::Rpc { status, .. } => *status,
            HttpError::Status(status) => Some(*status),
            HttpError::Other(_) => None,
        }
    }
}

fn status_text(status: StatusCode) -> String {
    format!("it answered HTTP {}", status.as_u16())
}

/// What a request is sent as.
#[derive(Clone, Default)]
struct Session {
    /// `None` before the version handshake.
    era: Option<Era>,
    /// The legacy session id, if the server gave one.
    id: Option<String>,
    /// Counts new sessions, so concurrent requests that all got 404 open
    /// only one.
    generation: u64,
}

/// A server reached over Streamable HTTP. Cheap to share: the requests
/// in flight hold its parts.
pub struct HttpConnection {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    url: Url,
    /// The configured headers, values from the environment.
    headers: HeaderMap,
    max_bytes: usize,
    next_id: AtomicU64,
    session: Mutex<Session>,
    /// Held while a new legacy session is opened.
    reopening: tokio::sync::Mutex<()>,
    closed: Mutex<Option<String>>,
}

impl HttpConnection {
    /// A connection to the server of `config`, header values read through
    /// `env`; responses over `max_bytes` are refused. Nothing is sent yet.
    pub fn new(
        config: &HttpConfig,
        env: &dyn Fn(&str) -> Option<String>,
        max_bytes: usize,
    ) -> Result<Self, String> {
        let headers = config.resolve(env)?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT);
        if config.is_loopback() {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|err| format!("cannot set up the HTTP client: {}", describe(&err)))?;
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                url: config.url.clone(),
                headers,
                max_bytes,
                next_id: AtomicU64::new(1),
                session: Mutex::default(),
                reopening: tokio::sync::Mutex::new(()),
                closed: Mutex::new(None),
            }),
        })
    }

    /// Why the connection is closed (after [`HttpConnection::shutdown`]),
    /// `None` while it is usable.
    pub fn closed(&self) -> Option<String> {
        self.inner.closed.lock().unwrap().clone()
    }

    /// Finds the server's era: a modern `server/discover` first, and the
    /// legacy `initialize` handshake when the answer is an error status or
    /// a JSON-RPC error that is not a modern one (the binding's "Backward
    /// Compatibility"). Returns the era and the server's capabilities.
    pub async fn handshake(&self) -> Result<(Era, Value), String> {
        let modern = Era::Modern(MODERN_VERSION.into());
        let probe = self
            .inner
            .exchange(
                "server/discover",
                json!({"_meta": server::request_meta(MODERN_VERSION)}),
                &Session {
                    era: Some(modern),
                    ..Session::default()
                },
                &[],
            )
            .await;
        match probe {
            Ok(answer) => {
                let (era, capabilities) = server::modern(&answer.result)?;
                self.inner.session.lock().unwrap().era = Some(era.clone());
                return Ok((era, capabilities));
            }
            Err(HttpError::Rpc { error, .. }) if error.code == UNSUPPORTED_VERSION => {
                return Err(server::unsupported(&error));
            }
            Err(HttpError::Rpc { error, .. })
                if matches!(error.code, HEADER_MISMATCH | MISSING_CLIENT_CAPABILITY) =>
            {
                return Err(format!(
                    "it refused `server/discover`: {}",
                    server::describe(CallError::Rpc(error))
                ));
            }
            // Not a modern error: a legacy server.
            Err(HttpError::Rpc { .. }) => {}
            Err(HttpError::Status(status)) if status.is_client_error() => {}
            Err(HttpError::Status(status)) => {
                return Err(format!("`server/discover` failed: {}", status_text(status)));
            }
            Err(HttpError::Other(reason)) => {
                return Err(format!("`server/discover` failed: {reason}"));
            }
        }
        let (session, capabilities) = self.inner.initialize().await?;
        let era = session.era.clone().expect("set by initialize");
        *self.inner.session.lock().unwrap() = Session {
            generation: 1,
            ..session
        };
        Ok((era, capabilities))
    }

    /// Sends a request with the era's headers and `extra` ones, and waits
    /// up to `timeout` for its result. Dropping the future (or the
    /// timeout) cancels the request.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        extra: &[(String, String)],
    ) -> Result<Value, CallError> {
        if let Some(reason) = self.closed() {
            return Err(CallError::Closed(reason));
        }
        match tokio::time::timeout(timeout, self.inner.request(method, params, extra)).await {
            Ok(result) => result.map_err(HttpError::into_call_error),
            Err(_) => Err(CallError::Timeout(timeout)),
        }
    }

    /// Ends a legacy session with DELETE (a server may answer 405: it
    /// keeps sessions itself); later requests fail.
    pub async fn shutdown(&self) {
        let session = self.inner.session.lock().unwrap().clone();
        *self.inner.closed.lock().unwrap() = Some("junees closed the connection".into());
        if session.id.is_none() {
            return;
        }
        let request = self
            .inner
            .client
            .request(Method::DELETE, self.inner.url.clone())
            .headers(self.inner.headers.clone())
            .headers(session_headers(&session));
        let _ = tokio::time::timeout(SHORT_TIMEOUT, request.send()).await;
    }
}

/// A JSON-RPC result with the session id the response carried.
struct Answer {
    result: Value,
    session_id: Option<String>,
}

impl Inner {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// A request in the current session; a legacy session the server
    /// ended (404) is opened again once and the request sent again.
    async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        extra: &[(String, String)],
    ) -> Result<Value, HttpError> {
        let session = self.session.lock().unwrap().clone();
        let mut guard = CancelGuard::new(self, &session);
        let mut answer = self
            .exchange_as(&mut guard, method, &params, &session, extra)
            .await;
        guard.disarm();
        // The status counts, not the body: servers send a JSON-RPC error
        // (-32001 "Session not found") with the 404.
        if answer.as_ref().err().and_then(HttpError::status) == Some(StatusCode::NOT_FOUND)
            && session.id.is_some()
        {
            let session = self.reopen(session.generation).await?;
            guard = CancelGuard::new(self, &session);
            answer = self
                .exchange_as(&mut guard, method, &params, &session, extra)
                .await;
            guard.disarm();
        }
        answer.map(|answer| answer.result)
    }

    async fn exchange_as(
        self: &Arc<Self>,
        guard: &mut CancelGuard,
        method: &str,
        params: &Value,
        session: &Session,
        extra: &[(String, String)],
    ) -> Result<Answer, HttpError> {
        let id = self.next_id();
        guard.id = Some(id);
        self.exchange_with_id(id, method, params.clone(), session, extra)
            .await
    }

    async fn exchange(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        session: &Session,
        extra: &[(String, String)],
    ) -> Result<Answer, HttpError> {
        let id = self.next_id();
        self.exchange_with_id(id, method, params, session, extra)
            .await
    }

    /// Opens a new legacy session unless another request already did since
    /// `seen` (the session generation it was sent in).
    async fn reopen(self: &Arc<Self>, seen: u64) -> Result<Session, HttpError> {
        let _reopening = self.reopening.lock().await;
        let current = self.session.lock().unwrap().clone();
        if current.generation != seen {
            return Ok(current);
        }
        let (session, _) = self.initialize().await.map_err(|reason| {
            HttpError::Other(format!(
                "the server ended the session, and opening a new one failed: {reason}"
            ))
        })?;
        let session = Session {
            generation: seen + 1,
            ..session
        };
        *self.session.lock().unwrap() = session.clone();
        Ok(session)
    }

    /// The legacy handshake: `initialize` without a session, then
    /// `notifications/initialized` in the session it opened.
    async fn initialize(self: &Arc<Self>) -> Result<(Session, Value), String> {
        let answer = self
            .exchange(
                "initialize",
                server::initialize_params(),
                &Session::default(),
                &[],
            )
            .await
            .map_err(|err| match err {
                HttpError::Status(status) if status.is_client_error() => format!(
                    "it answered neither `server/discover` nor `initialize` ({}); servers on \
                     the deprecated HTTP+SSE transport of 2024-11-05 are not supported",
                    status_text(status)
                ),
                err => format!(
                    "`initialize` failed: {}",
                    server::describe(err.into_call_error())
                ),
            })?;
        let (era, capabilities) = server::legacy_result(&answer.result)?;
        let session = Session {
            era: Some(era),
            id: answer.session_id,
            generation: 0,
        };
        self.notify(&session, "notifications/initialized", json!({}))
            .await
            .map_err(|err| {
                format!(
                    "`notifications/initialized` was refused: {}",
                    server::describe(err.into_call_error())
                )
            })?;
        Ok((session, capabilities))
    }

    /// POSTs a notification (or a reply to the server) and expects 202.
    async fn post_message(
        self: &Arc<Self>,
        session: &Session,
        message: &Value,
    ) -> Result<(), HttpError> {
        let request = self.post(session, message, None, &[]);
        let response = tokio::time::timeout(SHORT_TIMEOUT, request.send())
            .await
            .map_err(|_| HttpError::Other("no answer within 10 s".into()))?
            .map_err(|err| HttpError::Other(describe(&err)))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(self.error_response(response).await)
    }

    async fn notify(
        self: &Arc<Self>,
        session: &Session,
        method: &str,
        params: Value,
    ) -> Result<(), HttpError> {
        self.post_message(
            session,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    /// A POST of `body` with the headers of `session` (and of the request,
    /// for a modern one).
    fn post(
        &self,
        session: &Session,
        body: &Value,
        request: Option<(&str, &Value)>,
        extra: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        let mut headers = session_headers(session);
        if let (Some(Era::Modern(_)), Some((method, params))) = (&session.era, request) {
            add(&mut headers, "mcp-method", method);
            if let Some(name) = meta::name_header(method, params) {
                add(&mut headers, "mcp-name", &name);
            }
            for (name, value) in extra {
                add(&mut headers, name, value);
            }
        }
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        self.client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .headers(headers)
            .body(body.to_string())
    }

    /// Sends request `id` and reads its answer, from JSON or an SSE
    /// stream.
    async fn exchange_with_id(
        self: &Arc<Self>,
        id: u64,
        method: &str,
        params: Value,
        session: &Session,
        extra: &[(String, String)],
    ) -> Result<Answer, HttpError> {
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self
            .post(session, &body, Some((method, &params)), extra)
            .send()
            .await
            .map_err(|err| HttpError::Other(describe(&err)))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(HttpError::Other(format!(
                "it answered with a redirect (HTTP {}), which junees does not follow; put the \
                 final address in `url`",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(self.error_response(response).await);
        }
        let session_id = match response.headers().get("mcp-session-id") {
            None => None,
            Some(value) => {
                let bytes = value.as_bytes();
                if bytes.is_empty() || !bytes.iter().all(|b| (0x21..=0x7e).contains(b)) {
                    return Err(HttpError::Other(
                        "it sent a session id that is not visible ASCII".into(),
                    ));
                }
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
        };
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let reply = match kind.as_str() {
            "application/json" => self.json_reply(response, id).await?,
            "text/event-stream" => self.stream_reply(response, id, session).await?,
            _ if status == StatusCode::ACCEPTED || kind.is_empty() => {
                return Err(HttpError::Other(format!(
                    "it answered HTTP {} without a result",
                    status.as_u16()
                )));
            }
            _ => {
                return Err(HttpError::Other(format!(
                    "it answered with the content type `{}`, neither JSON nor an event stream",
                    crate::prompt_text::escape_hidden(&kind)
                )));
            }
        };
        reply.map(|result| Answer { result, session_id })
    }

    /// The JSON-RPC error in an error response, or its status.
    async fn error_response(&self, mut response: reqwest::Response) -> HttpError {
        let status = response.status();
        let mut body = Vec::new();
        while body.len() < ERROR_BODY_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                _ => break,
            }
        }
        let error = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|message| rpc_error(&message));
        match error {
            Some(error) => HttpError::Rpc {
                error,
                status: Some(status),
            },
            None => HttpError::Status(status),
        }
    }

    async fn json_reply(
        &self,
        mut response: reqwest::Response,
        id: u64,
    ) -> Result<Result<Value, HttpError>, HttpError> {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| HttpError::Other(describe(&err)))?
        {
            if body.len() + chunk.len() > self.max_bytes {
                return Err(self.too_large());
            }
            body.extend_from_slice(&chunk);
        }
        let message: Value = serde_json::from_slice(&body)
            .map_err(|_| HttpError::Other("its answer is not valid JSON".into()))?;
        // 2025-03-26 allowed batches.
        let items = match message {
            Value::Array(items) => items,
            message => vec![message],
        };
        items
            .iter()
            .find_map(|item| reply_to(item, id))
            .ok_or_else(|| HttpError::Other("its answer is not a response to the request".into()))
    }

    /// Reads the event stream until the response to `id`. Notifications
    /// are skipped; legacy server requests are answered.
    async fn stream_reply(
        self: &Arc<Self>,
        mut response: reqwest::Response,
        id: u64,
        session: &Session,
    ) -> Result<Result<Value, HttpError>, HttpError> {
        let mut parser = sse::Parser::default();
        let mut events = Vec::new();
        let mut read = 0usize;
        loop {
            let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| HttpError::Other(describe(&err)))?
            else {
                return Err(HttpError::Other(
                    "it closed the event stream before answering".into(),
                ));
            };
            read += chunk.len();
            if read > self.max_bytes {
                return Err(self.too_large());
            }
            parser.feed(&chunk, &mut events);
            for data in events.drain(..) {
                let Ok(message) = serde_json::from_slice::<Value>(&data) else {
                    continue;
                };
                let items = match message {
                    Value::Array(items) => items,
                    message => vec![message],
                };
                for item in items {
                    if let Some(reply) = reply_to(&item, id) {
                        return Ok(reply);
                    }
                    self.answer_server_request(&item, session);
                }
            }
        }
    }

    /// Answers a server's request on a legacy stream in the background:
    /// `ping` with an empty result, anything else with -32601.
    fn answer_server_request(self: &Arc<Self>, message: &Value, session: &Session) {
        let (Some(method), Some(id)) = (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) else {
            return;
        };
        if !matches!(session.era, Some(Era::Legacy(_))) {
            return;
        }
        let reply = if method == "ping" {
            json!({"jsonrpc": "2.0", "id": id, "result": {}})
        } else {
            json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601,
                "message": format!("junees does not support `{method}`")
            }})
        };
        let inner = Arc::clone(self);
        let session = session.clone();
        tokio::spawn(async move {
            let _ = inner.post_message(&session, &reply).await;
        });
    }

    fn too_large(&self) -> HttpError {
        const MIB: usize = 1024 * 1024;
        HttpError::Other(if self.max_bytes.is_multiple_of(MIB) {
            format!("its answer is larger than {} MiB", self.max_bytes / MIB)
        } else {
            format!("its answer is larger than {} bytes", self.max_bytes)
        })
    }
}

/// `MCP-Protocol-Version` (once the era is known) and `Mcp-Session-Id`.
fn session_headers(session: &Session) -> HeaderMap {
    let mut headers = HeaderMap::new();
    match &session.era {
        Some(Era::Modern(version) | Era::Legacy(version)) => {
            add(&mut headers, "mcp-protocol-version", version);
        }
        None => {}
    }
    if let Some(id) = &session.id {
        add(&mut headers, "mcp-session-id", id);
    }
    headers
}

/// Adds a header; values that are not valid header text were encoded or
/// checked before, so a failure only drops the header.
fn add(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

/// The result or error of `message` if it answers request `id`.
fn reply_to(message: &Value, id: u64) -> Option<Result<Value, HttpError>> {
    if message.get("method").is_some() || message.get("id").and_then(Value::as_u64) != Some(id) {
        return None;
    }
    Some(match rpc_error(message) {
        Some(error) => Err(HttpError::Rpc {
            error,
            status: None,
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    })
}

fn rpc_error(message: &Value) -> Option<RpcError> {
    let error = message.get("error")?;
    Some(RpcError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        data: error.get("data").cloned(),
    })
}

/// A request error and its causes, without the URL.
pub(crate) fn describe(err: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    if err.is_timeout() {
        parts.push("timed out".to_string());
    } else if err.is_connect() {
        parts.push("cannot connect".to_string());
    }
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    if parts.is_empty() {
        parts.push("the request failed".into());
    }
    crate::prompt_text::escape_hidden(&parts.join(": "))
}

/// Sends `notifications/cancelled` for a legacy request whose future was
/// dropped before its answer; a modern request is cancelled by the
/// closed stream alone.
struct CancelGuard {
    inner: Arc<Inner>,
    session: Option<Session>,
    id: Option<u64>,
}

impl CancelGuard {
    fn new(inner: &Arc<Inner>, session: &Session) -> Self {
        Self {
            inner: Arc::clone(inner),
            session: matches!(session.era, Some(Era::Legacy(_))).then(|| session.clone()),
            id: None,
        }
    }

    fn disarm(&mut self) {
        self.id = None;
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let (Some(session), Some(id)) = (self.session.take(), self.id) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        runtime.spawn(async move {
            let _ = inner
                .notify(
                    &session,
                    "notifications/cancelled",
                    json!({"requestId": id, "reason": "junees stopped waiting"}),
                )
                .await;
        });
    }
}
