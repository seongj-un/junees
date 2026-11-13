//! The HTTP side of the dashboard: one request per connection, `GET`
//! only, answered with `connection: close`.
//!
//! Every request is checked in this order, and the first failure answers:
//!
//! 1. the head reads, within [`MAX_HEAD`] bytes and [`READ_TIME`] (400,
//!    431, or the connection is closed);
//! 2. `Host` names this server: `127.0.0.1`, `localhost`, `[::1]` or the
//!    address it listens on, with its port (421), so a page of another
//!    site whose name was pointed at 127.0.0.1 (DNS rebinding) is refused;
//! 3. `Origin`, when sent, is this server (403): no other site's script
//!    reads the answers, and no CORS headers are sent;
//! 4. the method is `GET`, or `POST` on a write path
//!    ([`super::actions`]) only (405); a body of at most [`MAX_BODY`]
//!    bytes (413), read for a `POST` only;
//! 5. a write carries an `Origin` (403), a JSON `Content-Type` if any
//!    (415), and no body but `{}` (400);
//! 6. under `/api/`, `Authorization: Bearer <token>` holds this run's
//!    token, compared in constant time (401).
//!
//! Every answer carries [`SECURITY_HEADERS`].

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use super::{actions, api, assets};
use crate::webhook::http::{self, Head};

/// The request line and the headers together.
pub const MAX_HEAD: usize = 8 * 1024;
/// The body a request may announce; it is not read.
pub const MAX_BODY: u64 = 1024;
/// How long a client has to send the head.
pub const READ_TIME: Duration = Duration::from_secs(10);
/// How long a connection may last, answer included.
pub const CONNECTION_TIME: Duration = Duration::from_secs(60);
/// How long to wait after a failed accept.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);
/// Connections served at once; more wait to be accepted.
const MAX_CONNECTIONS: usize = 16;

/// Sent with every answer.
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cache-control", "no-store"),
];

/// How the settings read the environment; tests give a fake one.
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What every request is served with.
pub struct Context {
    /// The token of this run (base64url).
    pub token: String,
    /// The address and port it listens on, which `Host` must name.
    pub ip: IpAddr,
    pub port: u16,
    pub env: Env,
    pub started: Instant,
    /// What `/api/sessions` counted in each journal.
    pub(crate) sessions: std::sync::Mutex<api::SessionCache>,
    /// One `/api/doctor` at a time.
    pub(crate) doctor: Arc<Semaphore>,
}

impl Context {
    pub fn new(token: String, ip: IpAddr, port: u16, env: Env) -> Self {
        Self {
            token,
            ip,
            port,
            env,
            started: Instant::now(),
            sessions: Default::default(),
            doctor: Arc::new(Semaphore::new(1)),
        }
    }
}

/// An answer: status, content type, body and headers beyond the usual.
pub struct Answer {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, &'static str)>,
}

impl Answer {
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(value).unwrap_or_default(),
            headers: Vec::new(),
        }
    }

    /// The error shape of the API: `{"error": code, "message": text}`.
    pub fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(status, &json!({ "error": code, "message": message }))
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    /// The whole answer as sent.
    pub fn render(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
            self.status,
            http::reason(self.status),
            self.content_type,
            self.body.len()
        );
        for (name, value) in SECURITY_HEADERS.iter().chain(&self.headers) {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Serves `listener`; a failed accept is reported and the next one tried.
pub async fn serve(listener: TcpListener, context: Arc<Context>) -> std::io::Result<()> {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(slot) = slots.clone().acquire_owned().await else {
            return Ok(());
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // Out of file descriptors, a connection reset before it was
            // taken: the next one may do.
            Err(err) => {
                eprintln!("dashboard: cannot accept a connection: {err}");
                tokio::time::sleep(ACCEPT_PAUSE).await;
                continue;
            }
        };
        let context = context.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _ = tokio::time::timeout(CONNECTION_TIME, connection(stream, &context)).await;
        });
    }
}

async fn connection(mut stream: TcpStream, context: &Arc<Context>) {
    let mut buf = Vec::new();
    let read = tokio::time::timeout(
        READ_TIME,
        http::read_head_within(&mut stream, &mut buf, MAX_HEAD),
    )
    .await;
    let answer = match read {
        // Too slow: closed without an answer.
        Err(_) => return,
        Ok(Err(err)) => {
            let code = if err.status == 431 {
                "head_too_large"
            } else {
                "bad_request"
            };
            Answer::error(err.status, code, err.message)
        }
        Ok(Ok(head)) => match read_body(&mut stream, &head, buf).await {
            Ok(body) => answer(&head, &body, context).await,
            Err(answer) => answer,
        },
    };
    send(&mut stream, &answer).await;
}

/// The body of a `POST`, which `Content-Length` announces and
/// [`MAX_BODY`] limits; `rest` is what was read past the head. Other
/// methods' bodies are not read. A body that is too long, chunked, or
/// does not arrive in [`READ_TIME`] is answered at once.
async fn read_body(
    stream: &mut TcpStream,
    head: &Head,
    mut rest: Vec<u8>,
) -> Result<Vec<u8>, Answer> {
    if head.method != "POST" {
        return Ok(Vec::new());
    }
    // Refused in order by `answer`, after the Host check.
    if body_refused(head).is_some() {
        return Ok(Vec::new());
    }
    let length = head
        .header("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    if rest.len() > length {
        rest.truncate(length);
    }
    let missing = length - rest.len();
    if missing > 0 {
        let mut more = vec![0u8; missing];
        let read = tokio::time::timeout(READ_TIME, stream.read_exact(&mut more)).await;
        if !matches!(read, Ok(Ok(_))) {
            return Err(Answer::error(
                400,
                "bad_request",
                "the request body ended early",
            ));
        }
        rest.extend_from_slice(&more);
    }
    Ok(rest)
}

async fn send<W: AsyncWrite + Unpin>(io: &mut W, answer: &Answer) {
    let _ = io.write_all(&answer.render()).await;
    let _ = io.shutdown().await;
}

/// The answer to a request whose head (and, for a `POST`, body) was read.
pub async fn answer(head: &Head, body: &[u8], context: &Arc<Context>) -> Answer {
    let Some(host) = head
        .header("host")
        .filter(|host| own_host(host, context.ip, context.port))
    else {
        return Answer::error(
            421,
            "misdirected_request",
            "the Host header does not name this dashboard",
        );
    };
    if let Some(origin) = head.header("origin")
        && !same_origin(origin, host)
    {
        return Answer::error(
            403,
            "forbidden_origin",
            "requests from other sites are refused",
        );
    }
    let write = actions::is_write_path(&head.path);
    match (head.method.as_str(), write) {
        ("GET", false) | ("POST", true) => {}
        (_, false) => {
            return Answer::error(405, "method_not_allowed", "only GET is served here")
                .with_header("allow", "GET");
        }
        (_, true) => {
            return Answer::error(405, "method_not_allowed", "only POST is served here")
                .with_header("allow", "POST");
        }
    }
    if let Some(answer) = body_refused(head) {
        return answer;
    }
    if write && let Some(answer) = write_refused(head, body) {
        return answer;
    }
    if head.path == "/api" || head.path.starts_with("/api/") {
        if !authorized(head.header("authorization"), &context.token) {
            return Answer::error(
                401,
                "unauthorized",
                "open the address `junees dashboard` printed",
            )
            .with_header("www-authenticate", "Bearer");
        }
        if write {
            return actions::write(&head.path, context).await;
        }
        return api::answer(&head.path, &head.query, context).await;
    }
    match assets::find(&head.path) {
        Some(asset) => Answer {
            status: 200,
            content_type: asset.content_type,
            body: asset.bytes.to_vec(),
            headers: Vec::new(),
        },
        None => Answer::error(404, "not_found", "there is nothing at this path"),
    }
}

/// Why a write is refused before its token is checked: it must carry an
/// `Origin` (the same origin was checked already), so only this page's
/// script can send it; a `Content-Type`, when given, must be JSON; the
/// body must be empty or `{}`.
fn write_refused(head: &Head, body: &[u8]) -> Option<Answer> {
    if head.header("origin").is_none() {
        return Some(Answer::error(
            403,
            "forbidden_origin",
            "a write needs an Origin header from this page",
        ));
    }
    if let Some(kind) = head.header("content-type") {
        let essence = kind.split(';').next().unwrap_or_default().trim();
        if !essence.eq_ignore_ascii_case("application/json") {
            return Some(Answer::error(
                415,
                "unsupported_media_type",
                "a write's body is JSON (application/json)",
            ));
        }
    }
    let empty = match std::str::from_utf8(body).map(str::trim) {
        Ok("") => true,
        Ok(text) => serde_json::from_str::<serde_json::Map<String, Value>>(text)
            .is_ok_and(|object| object.is_empty()),
        Err(_) => false,
    };
    if !empty {
        return Some(Answer::error(
            400,
            "bad_body",
            "a write takes no body, or `{}`",
        ));
    }
    None
}

/// Why a request with a body is refused: one announced in pieces, or
/// longer than [`MAX_BODY`].
fn body_refused(head: &Head) -> Option<Answer> {
    if head.header("transfer-encoding").is_some() {
        return Some(Answer::error(
            400,
            "bad_request",
            "a body sent in chunks is not accepted",
        ));
    }
    let length = head.header("content-length")?;
    match length.parse::<u64>() {
        Ok(length) if length <= MAX_BODY => None,
        Ok(_) => Some(Answer::error(
            413,
            "body_too_large",
            "a request body may hold 1 KiB at most",
        )),
        Err(_) => Some(Answer::error(
            400,
            "bad_request",
            "the Content-Length header is not a number",
        )),
    }
}

/// Whether `host` (a `Host` header) is this server, listening on `ip`
/// and `port`: `127.0.0.1`, `localhost`, `[::1]` or `ip` itself
/// (`127.0.0.2`), with `:port`, or without a port when it is 80, which
/// browsers leave out.
pub fn own_host(host: &str, ip: IpAddr, port: u16) -> bool {
    let (name, given) = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((inside, after)) => (&host[..inside.len() + 2], after),
            None => return false,
        },
        None => host.split_once(':').map_or((host, ""), |(name, rest)| {
            (name, &host[name.len()..][..rest.len() + 1])
        }),
    };
    let port_ok = given == format!(":{port}") || (given.is_empty() && port == 80);
    let own = match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    port_ok
        && (name == "127.0.0.1"
            || name == "[::1]"
            || name == own
            || name.eq_ignore_ascii_case("localhost"))
}

/// Whether `origin` (an `Origin` header) is the page of `host`.
fn same_origin(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|rest| rest.eq_ignore_ascii_case(host))
}

/// Whether `header` (`Authorization`) is `Bearer <token>`.
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(given) = header.and_then(|header| header.strip_prefix("Bearer ")) else {
        return false;
    };
    constant_time_eq(given.trim().as_bytes(), token.as_bytes())
}

/// `a == b`, taking as long whatever byte differs. The length is not
/// hidden: every token has 43 characters.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_must_name_this_server() {
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let own_host = |host: &str, port: u16| own_host(host, local, port);
        for host in [
            "127.0.0.1:7878",
            "localhost:7878",
            "LocalHost:7878",
            "[::1]:7878",
        ] {
            assert!(own_host(host, 7878), "{host}");
        }
        for host in [
            "127.0.0.1:7879",
            "127.0.0.1",
            "localhost",
            "evil.example:7878",
            "127.0.0.1.evil.example:7878",
            "[::1]",
            "0.0.0.0:7878",
            "127.0.0.2:7878",
            "",
            "[::1",
            "127.0.0.1:7878:1",
        ] {
            assert!(!own_host(host, 7878), "{host}");
        }
        // Port 80: browsers send no port.
        assert!(own_host("127.0.0.1", 80));
        assert!(own_host("localhost:80", 80));
        assert!(!own_host("127.0.0.1", 81));
    }

    #[test]
    fn the_listening_address_names_this_server_too() {
        let other: IpAddr = "127.0.0.2".parse().unwrap();
        assert!(own_host("127.0.0.2:9000", other, 9000));
        assert!(own_host("localhost:9000", other, 9000));
        assert!(!own_host("127.0.0.3:9000", other, 9000));
        let v6: IpAddr = "::1".parse().unwrap();
        assert!(own_host("[::1]:9000", v6, 9000));
        assert!(!own_host("[::2]:9000", v6, 9000));
    }

    #[test]
    fn origins_must_be_this_page() {
        assert!(same_origin("http://127.0.0.1:7878", "127.0.0.1:7878"));
        assert!(same_origin("http://localhost:7878", "localhost:7878"));
        assert!(!same_origin("http://localhost:7878", "127.0.0.1:7878"));
        assert!(!same_origin("https://127.0.0.1:7878", "127.0.0.1:7878"));
        assert!(!same_origin("null", "127.0.0.1:7878"));
        assert!(!same_origin("http://evil.example", "127.0.0.1:7878"));
    }

    #[test]
    fn the_token_is_compared_whole() {
        assert!(authorized(Some("Bearer abc"), "abc"));
        assert!(!authorized(Some("Bearer abd"), "abc"));
        assert!(!authorized(Some("Bearer ab"), "abc"));
        assert!(!authorized(Some("Bearer abcd"), "abc"));
        assert!(!authorized(Some("bearer abc"), "abc"));
        assert!(!authorized(Some("Basic abc"), "abc"));
        assert!(!authorized(Some("Bearer "), "abc"));
        assert!(!authorized(None, "abc"));
    }

    #[test]
    fn every_answer_carries_the_security_headers() {
        let text = String::from_utf8(Answer::error(404, "not_found", "x").render()).unwrap();
        for (name, value) in SECURITY_HEADERS {
            assert!(text.contains(&format!("{name}: {value}\r\n")), "{name}");
        }
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(text.contains("content-type: application/json; charset=utf-8\r\n"));
        assert!(!text.to_ascii_lowercase().contains("access-control"));
    }
}
