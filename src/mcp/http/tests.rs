//! The Streamable HTTP transport against a fake MCP server on loopback
//! that speaks either era and answers with JSON or event streams.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::mcp::config::ServerConfig;
use crate::mcp::{EnvLookup, Mcp, Status, Timeouts};
use crate::tools::ToolOutput;

const TOKEN: &str = "tok-s3cret-value";

/// One HTTP request the fake server read.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn rpc(&self) -> &str {
        self.body["method"].as_str().unwrap_or_default()
    }
}

enum Body {
    Full(Vec<u8>),
    /// Written piece by piece; with `hold` the connection then stays open
    /// until the client closes it.
    Stream {
        parts: Vec<Vec<u8>>,
        hold: bool,
    },
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Body,
}

impl Reply {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Body::Full(Vec::new()),
        }
    }

    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Body::Full(value.to_string().into_bytes()),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// What the fake server does.
#[derive(Default)]
struct Knobs {
    legacy: bool,
    sse: bool,
    /// Modern: answer `UnsupportedProtocolVersion`.
    unsupported: bool,
    /// Answer every POST with this status and no body.
    status: Option<u16>,
    /// Answer every POST with a redirect here.
    redirect: Option<String>,
    /// The next request in the current session gets 404.
    expire: bool,
    sessions: u32,
    current: Option<String>,
}

#[derive(Clone)]
struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Request ids whose stream the client closed while it was held.
    closed: Arc<Mutex<Vec<Value>>>,
    knobs: Arc<Mutex<Knobs>>,
}

impl Fake {
    async fn start(knobs: Knobs) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Self {
            addr: listener.local_addr().unwrap(),
            seen: Arc::default(),
            closed: Arc::default(),
            knobs: Arc::new(Mutex::new(knobs)),
        };
        let server = fake.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move { server.serve(stream).await });
            }
        });
        fake
    }

    fn url(&self) -> String {
        format!("http://{}/mcp/s3cret-path?key=q-s3cret", self.addr)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn rpcs(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|seen| match seen.method.as_str() {
                "POST" => seen.rpc().to_string(),
                other => other.to_string(),
            })
            .collect()
    }

    async fn serve(&self, mut stream: TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        let head_end = loop {
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break end;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let method = lines.next().unwrap().split(' ').next().unwrap().to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let length: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .map_or(0, |(_, value)| value.parse().unwrap());
        let mut body = buffer[head_end + 4..].to_vec();
        while body.len() < length {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        let seen = Seen {
            method,
            headers,
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        };
        self.seen.lock().unwrap().push(seen.clone());
        let reply = self.reply(&seen);
        let mut out = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status);
        for (name, value) in &reply.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        match reply.body {
            Body::Full(bytes) => {
                out.push_str(&format!("Content-Length: {}\r\n\r\n", bytes.len()));
                let mut all = out.into_bytes();
                all.extend_from_slice(&bytes);
                let _ = stream.write_all(&all).await;
            }
            Body::Stream { parts, hold } => {
                out.push_str("\r\n");
                let _ = stream.write_all(out.as_bytes()).await;
                for part in parts {
                    let _ = stream.write_all(&part).await;
                    let _ = stream.flush().await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if hold {
                    let read =
                        tokio::time::timeout(Duration::from_secs(30), stream.read(&mut chunk))
                            .await;
                    if matches!(read, Ok(Ok(0) | Err(_))) {
                        self.closed.lock().unwrap().push(seen.body["id"].clone());
                    }
                }
            }
        }
        let _ = stream.shutdown().await;
    }

    fn reply(&self, seen: &Seen) -> Reply {
        let mut knobs = self.knobs.lock().unwrap();
        if let Some(target) = &knobs.redirect {
            return Reply::empty(307).header("Location", target);
        }
        if let Some(status) = knobs.status {
            return Reply::empty(status);
        }
        let id = seen.body.get("id").cloned();
        let method = seen.rpc().to_string();
        let params = seen.body["params"].clone();
        if knobs.legacy {
            if seen.method == "DELETE" {
                knobs.current = None;
                return Reply::empty(200);
            }
            if method == "initialize" {
                knobs.sessions += 1;
                let session = format!("sess-{}", knobs.sessions);
                knobs.current = Some(session.clone());
                let result = json!({"protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}});
                return answer(&knobs, id, result, false).header("Mcp-Session-Id", &session);
            }
            let session = seen.header("mcp-session-id");
            if session.is_none() {
                return Reply::json(
                    400,
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000,
                        "message": "Bad Request: No valid session ID provided"}}),
                );
            }
            if knobs.expire || session != knobs.current.as_deref() {
                knobs.expire = false;
                knobs.current = None;
                return Reply::empty(404);
            }
            if seen.header("mcp-protocol-version") != Some("2025-06-18") {
                return Reply::empty(400);
            }
            if id.is_none() || seen.body.get("method").is_none() {
                // A notification or a reply to the server.
                return Reply::empty(202);
            }
        } else {
            let version = params["_meta"]["io.modelcontextprotocol/protocolVersion"].clone();
            if knobs.unsupported {
                return error(400, id, -32022, "Unsupported protocol version")
                    .data(json!({"supported": ["2099-01-01"], "requested": version}));
            }
            let name_ok = match method.as_str() {
                "tools/call" => seen.header("mcp-name") == params["name"].as_str(),
                _ => seen.header("mcp-name").is_none(),
            };
            if seen.header("mcp-protocol-version") != version.as_str()
                || version != "2026-07-28"
                || seen.header("mcp-method") != Some(method.as_str())
                || !name_ok
            {
                return error(400, id, -32020, "Header mismatch").reply();
            }
            if method == "server/discover" {
                let result = json!({"resultType": "complete", "supportedVersions": ["2026-07-28"],
                    "capabilities": {"tools": {}}});
                return answer(&knobs, id, result, false);
            }
        }
        let result = match method.as_str() {
            "tools/list" => json!({"tools": [
                {"name": "echo", "inputSchema": {"type": "object"}},
                {"name": "sleep", "inputSchema": {"type": "object"}},
                {"name": "big", "inputSchema": {"type": "object"}},
                {"name": "sql", "inputSchema": {"type": "object", "properties": {
                    "region": {"type": "string", "x-mcp-header": "Region"},
                    "limit": {"type": "integer", "x-mcp-header": "Limit"}}}},
                {"name": "bad", "inputSchema": {"type": "object", "properties": {
                    "p": {"type": "string", "x-mcp-header": "a b"}}}},
            ]}),
            "tools/call" => {
                let arguments = &params["arguments"];
                let text = match params["name"].as_str().unwrap_or_default() {
                    "echo" => format!("echo: {}", arguments["text"].as_str().unwrap_or("")),
                    "sleep" => {
                        let content = if knobs.sse {
                            "text/event-stream"
                        } else {
                            "application/json"
                        };
                        return Reply {
                            status: 200,
                            headers: vec![("Content-Type".into(), content.into())],
                            body: Body::Stream {
                                parts: if knobs.sse {
                                    vec![b"id: 1\ndata:\n\n".to_vec()]
                                } else {
                                    Vec::new()
                                },
                                hold: true,
                            },
                        };
                    }
                    "big" => "가".repeat(arguments["chars"].as_u64().unwrap_or(10) as usize),
                    "sql" => format!(
                        "region={:?} limit={:?}",
                        seen.header("mcp-param-region"),
                        seen.header("mcp-param-limit")
                    ),
                    _ => return error(200, id, -32602, "Unknown tool").reply(),
                };
                json!({"content": [{"type": "text", "text": text}]})
            }
            _ => return error(200, id, -32601, "Method not found").reply(),
        };
        let legacy = knobs.legacy;
        answer(&knobs, id, result, legacy)
    }
}

struct RpcErrorReply {
    status: u16,
    message: Value,
}

impl RpcErrorReply {
    fn reply(self) -> Reply {
        Reply::json(self.status, self.message)
    }

    fn data(mut self, data: Value) -> Reply {
        self.message["error"]["data"] = data;
        self.reply()
    }
}

fn error(status: u16, id: Option<Value>, code: i64, message: &str) -> RpcErrorReply {
    RpcErrorReply {
        status,
        message: json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    }
}

/// The response to request `id`, as JSON or as a stream that first sends
/// a priming event, a comment, a progress notification and, with
/// `server_requests` (legacy), a `ping` and a `roots/list` request.
fn answer(knobs: &Knobs, id: Option<Value>, result: Value, server_requests: bool) -> Reply {
    let message = json!({"jsonrpc": "2.0", "id": id, "result": result});
    if !knobs.sse {
        return Reply::json(200, message);
    }
    let mut parts = vec![
        b"id: e1\ndata:\n\n".to_vec(),
        b": keep-alive\r\n\r\n".to_vec(),
        format!(
            "data: {}\n\n",
            json!({"jsonrpc": "2.0", "method": "notifications/progress",
                "params": {"progressToken": 1, "progress": 0.5}})
        )
        .into_bytes(),
    ];
    if server_requests {
        parts.push(
            format!(
                "data: {}\n\n",
                json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})
            )
            .into_bytes(),
        );
        parts.push(
            format!(
                "data: {}\n\n",
                json!({"jsonrpc": "2.0", "id": "srv-2", "method": "roots/list"})
            )
            .into_bytes(),
        );
    }
    // The response split across writes, with CRLF line ends.
    let text = message.to_string();
    let (first, second) = text.split_at(text.len() / 2);
    parts.push(format!("event: message\r\ndata: {first}").into_bytes());
    parts.push(format!("{second}\r\n\r\n").into_bytes());
    Reply {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "text/event-stream".into()),
            ("X-Accel-Buffering".into(), "no".into()),
        ],
        body: Body::Stream { parts, hold: false },
    }
}

fn env() -> EnvLookup {
    Arc::new(|name: &str| (name == "TEST_TOKEN").then(|| TOKEN.to_string()))
}

fn config(url: &str, extra: Value) -> ServerConfig {
    let mut server = json!({"url": url, "headers": {"Authorization": "Bearer env:TEST_TOKEN"}});
    for (key, value) in extra.as_object().unwrap() {
        server[key] = value.clone();
    }
    let list = json!({"servers": {"web": server}});
    crate::mcp::config::parse(&list.to_string())
        .unwrap()
        .remove(0)
}

async fn start(fake: &Fake, extra: Value) -> Mcp {
    start_with(config(&fake.url(), extra), env()).await
}

async fn start_with(config: ServerConfig, env: EnvLookup) -> Mcp {
    let timeouts = Timeouts {
        probe: Duration::from_millis(500),
        start: Duration::from_secs(10),
    };
    Mcp::start_servers_with_env(vec![config], &[], Path::new("/"), timeouts, env).await
}

async fn call(mcp: &Mcp, tool: &str, arguments: Value) -> ToolOutput {
    mcp.servers()[0].call(tool, arguments).await
}

fn failure(mcp: &Mcp) -> String {
    match mcp.servers()[0].status() {
        Status::Failed(reason) => reason,
        other => panic!("not failed: {other:?}"),
    }
}

#[tokio::test]
async fn a_modern_server_gets_metadata_headers_and_json_or_streams() {
    for sse in [false, true] {
        let fake = Fake::start(Knobs {
            sse,
            ..Knobs::default()
        })
        .await;
        let mcp = start(&fake, json!({})).await;
        assert_eq!(
            mcp.servers()[0].status(),
            Status::Running {
                version: "2026-07-28".into(),
                tools: 4
            },
            "sse {sse}"
        );
        // The tool whose header mark breaks the rules is left out.
        assert_eq!(
            mcp.tool_names(),
            [
                "mcp__web__echo",
                "mcp__web__sleep",
                "mcp__web__big",
                "mcp__web__sql"
            ]
        );
        assert!(
            mcp.report()
                .iter()
                .any(|line| line.contains("skipped `bad`: its `x-mcp-header` `a b` is not a valid")),
            "{:?}",
            mcp.report()
        );
        let output = call(&mcp, "echo", json!({"text": "안녕"})).await;
        assert_eq!(
            (output.content.as_str(), output.is_error),
            ("echo: 안녕", false)
        );
        let output = call(&mcp, "sql", json!({"region": "서울", "limit": 7})).await;
        assert_eq!(
            output.content,
            "region=Some(\"=?base64?7ISc7Jq4?=\") limit=Some(\"7\")"
        );
        let output = call(&mcp, "sql", json!({})).await;
        assert_eq!(output.content, "region=None limit=None");

        assert_eq!(
            fake.rpcs(),
            [
                "server/discover",
                "tools/list",
                "tools/call",
                "tools/call",
                "tools/call"
            ]
        );
        for seen in fake.seen() {
            assert_eq!(seen.method, "POST");
            assert_eq!(
                seen.header("accept"),
                Some("application/json, text/event-stream")
            );
            assert_eq!(seen.header("content-type"), Some("application/json"));
            assert_eq!(seen.header("mcp-protocol-version"), Some("2026-07-28"));
            assert_eq!(seen.header("mcp-method"), Some(seen.rpc()));
            assert_eq!(
                seen.header("authorization"),
                Some(format!("Bearer {TOKEN}").as_str())
            );
            assert_eq!(seen.header("mcp-session-id"), None);
            assert_eq!(
                seen.body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
        }
        mcp.shutdown().await;
        // No session to end: nothing more was sent.
        assert_eq!(fake.seen().len(), 5);
    }
}

#[tokio::test]
async fn a_legacy_server_gets_initialize_and_its_session() {
    for sse in [false, true] {
        let fake = Fake::start(Knobs {
            legacy: true,
            sse,
            ..Knobs::default()
        })
        .await;
        let mcp = start(&fake, json!({})).await;
        assert_eq!(
            mcp.servers()[0].status(),
            Status::Running {
                version: "2025-06-18".into(),
                tools: 5
            },
            "sse {sse}"
        );
        let output = call(&mcp, "echo", json!({"text": "hi"})).await;
        assert_eq!(
            (output.content.as_str(), output.is_error),
            ("echo: hi", false)
        );
        // Marked parameters are a modern thing.
        let output = call(&mcp, "sql", json!({"region": "x"})).await;
        assert_eq!(output.content, "region=None limit=None");

        let seen = fake.seen();
        assert_eq!(seen[0].rpc(), "server/discover");
        assert_eq!(seen[1].rpc(), "initialize");
        assert_eq!(seen[1].header("mcp-session-id"), None);
        assert_eq!(seen[1].body["params"]["protocolVersion"], "2025-11-25");
        assert_eq!(seen[2].rpc(), "notifications/initialized");
        for later in &seen[2..] {
            assert_eq!(later.header("mcp-session-id"), Some("sess-1"));
            assert_eq!(later.header("mcp-protocol-version"), Some("2025-06-18"));
            assert_eq!(later.header("mcp-method"), None);
            assert!(later.body["params"].get("_meta").is_none());
        }
        if sse {
            // The server's requests on the streams were answered: ping with
            // a result, roots/list refused.
            let probe = fake.clone();
            let answered = |id: &str, check: &dyn Fn(&Value) -> bool| {
                probe.seen().iter().any(|seen| {
                    seen.body.get("method").is_none() && seen.body["id"] == id && check(&seen.body)
                })
            };
            assert!(eventually(|| answered("srv-1", &|body| body["result"] == json!({}))).await);
            assert!(
                eventually(|| answered("srv-2", &|body| body["error"]["code"] == -32601)).await
            );
        }
        mcp.shutdown().await;
        let seen = fake.seen();
        let delete = seen.iter().find(|seen| seen.method == "DELETE").unwrap();
        assert_eq!(delete.header("mcp-session-id"), Some("sess-1"));
        let output = call(&mcp, "echo", json!({"text": "hi"})).await;
        assert!(
            output.is_error && output.content.contains("stopped"),
            "{}",
            output.content
        );
    }
}

#[tokio::test]
async fn an_ended_session_is_opened_again() {
    let fake = Fake::start(Knobs {
        legacy: true,
        ..Knobs::default()
    })
    .await;
    let mcp = start(&fake, json!({})).await;
    fake.knobs.lock().unwrap().expire = true;
    let output = call(&mcp, "echo", json!({"text": "again"})).await;
    assert_eq!(
        (output.content.as_str(), output.is_error),
        ("echo: again", false)
    );
    let seen = fake.seen();
    let tail: Vec<(String, Option<&str>)> = seen[seen.len() - 4..]
        .iter()
        .map(|seen| (seen.rpc().to_string(), seen.header("mcp-session-id")))
        .collect();
    assert_eq!(
        tail,
        [
            ("tools/call".to_string(), Some("sess-1")),
            ("initialize".to_string(), None),
            ("notifications/initialized".to_string(), Some("sess-2")),
            ("tools/call".to_string(), Some("sess-2")),
        ]
    );
    // A modern server's 404 is an answer, not an ended session.
    let modern = Fake::start(Knobs::default()).await;
    let mcp = start(&modern, json!({})).await;
    modern.knobs.lock().unwrap().status = Some(404);
    let output = call(&mcp, "echo", json!({})).await;
    assert!(output.is_error);
    assert!(
        output.content.contains("failed: it answered HTTP 404"),
        "{}",
        output.content
    );
    assert_eq!(modern.rpcs().last().unwrap(), "tools/call");
}

#[tokio::test]
async fn a_modern_server_with_other_versions_is_not_asked_to_initialize() {
    let fake = Fake::start(Knobs {
        unsupported: true,
        ..Knobs::default()
    })
    .await;
    let mcp = start(&fake, json!({})).await;
    let reason = failure(&mcp);
    assert!(reason.contains("does not speak MCP 2026-07-28"), "{reason}");
    assert!(reason.contains("2099-01-01"), "{reason}");
    assert_eq!(fake.rpcs(), ["server/discover"]);
}

#[tokio::test]
async fn slow_calls_time_out_and_are_cancelled_per_era() {
    // Modern: closing the stream is the cancellation.
    for sse in [false, true] {
        let fake = Fake::start(Knobs {
            sse,
            ..Knobs::default()
        })
        .await;
        let mcp = start(&fake, json!({"timeout_secs": 1})).await;
        let output = call(&mcp, "sleep", json!({})).await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .contains("no answer from the MCP server `web` within 1 s"),
            "{}",
            output.content
        );
        let closed = fake.closed.clone();
        assert!(eventually(|| closed.lock().unwrap().len() == 1).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!fake.rpcs().contains(&"notifications/cancelled".to_string()));
    }
    // Legacy: a closed stream is not a cancellation, so it is sent.
    let fake = Fake::start(Knobs {
        legacy: true,
        sse: true,
        ..Knobs::default()
    })
    .await;
    let mcp = start(&fake, json!({"timeout_secs": 1})).await;
    let output = call(&mcp, "sleep", json!({})).await;
    assert!(output.is_error);
    let probe = fake.clone();
    assert!(
        eventually(|| probe
            .rpcs()
            .contains(&"notifications/cancelled".to_string()))
        .await
    );
    let seen = fake.seen();
    let call_id = seen
        .iter()
        .find(|s| s.body["params"]["name"] == "sleep")
        .unwrap()
        .body["id"]
        .clone();
    let cancel = seen
        .iter()
        .find(|s| s.rpc() == "notifications/cancelled")
        .unwrap();
    assert_eq!(cancel.body["params"]["requestId"], call_id);
    assert_eq!(cancel.header("mcp-session-id"), Some("sess-1"));
}

/// Waits up to 5 s for the condition.
async fn eventually(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

#[tokio::test]
async fn responses_over_the_limit_are_refused() {
    for sse in [false, true] {
        let fake = Fake::start(Knobs {
            sse,
            ..Knobs::default()
        })
        .await;
        let config = config(&fake.url(), json!({}));
        let env = env();
        let connection = HttpConnection::new(config.http.as_ref().unwrap(), &*env, 4000).unwrap();
        let (era, _) = connection.handshake().await.unwrap();
        let params = era.params(json!({"name": "big", "arguments": {"chars": 500}}));
        let result = connection
            .request("tools/call", params.clone(), Duration::from_secs(5), &[])
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"].as_str().unwrap().len(), 1500);
        let params = era.params(json!({"name": "big", "arguments": {"chars": 2000}}));
        let err = connection
            .request("tools/call", params, Duration::from_secs(5), &[])
            .await
            .unwrap_err();
        assert_eq!(
            err,
            CallError::Failed("its answer is larger than 4000 bytes".into()),
            "sse {sse}"
        );
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let target = Fake::start(Knobs::default()).await;
    let fake = Fake::start(Knobs {
        redirect: Some(target.url()),
        ..Knobs::default()
    })
    .await;
    let mcp = start(&fake, json!({})).await;
    let reason = failure(&mcp);
    assert!(
        reason.contains("it answered with a redirect (HTTP 307), which junees does not follow"),
        "{reason}"
    );
    assert!(target.seen().is_empty());
    assert_eq!(fake.seen().len(), 1);
}

#[tokio::test]
async fn failures_name_neither_the_url_nor_header_values() {
    // Nothing listens on a port that was just freed.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/mcp/s3cret-path?key=q-s3cret");
    let mcp = start_with(config(&url, json!({})), env()).await;
    let reason = failure(&mcp);
    assert!(
        reason.starts_with("`server/discover` failed: cannot connect"),
        "{reason}"
    );

    let fake = Fake::start(Knobs {
        status: Some(500),
        ..Knobs::default()
    })
    .await;
    let mcp = start(&fake, json!({})).await;
    let reason = failure(&mcp);
    assert_eq!(reason, "`server/discover` failed: it answered HTTP 500");

    // A legacy-looking 405 to both requests.
    let fake = Fake::start(Knobs {
        status: Some(405),
        ..Knobs::default()
    })
    .await;
    let mcp = start(&fake, json!({})).await;
    let reason = failure(&mcp);
    assert!(
        reason.contains("neither `server/discover` nor `initialize`"),
        "{reason}"
    );
    assert!(reason.contains("HTTP+SSE transport"), "{reason}");
    assert_eq!(fake.rpcs(), ["server/discover", "initialize"]);

    // An unset variable: nothing is sent.
    let fake = Fake::start(Knobs::default()).await;
    let mcp = start_with(config(&fake.url(), json!({})), Arc::new(|_: &str| None)).await;
    assert_eq!(
        failure(&mcp),
        "header `Authorization` takes its value from the variable TEST_TOKEN, which is not set"
    );
    assert!(fake.seen().is_empty());

    for mcp in [&mcp, &start(&fake, json!({})).await] {
        let text = format!("{}{}", mcp.report().join("\n"), mcp.overview());
        for secret in ["s3cret", TOKEN, "/mcp"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
}

#[tokio::test]
async fn the_overview_names_the_transport_and_headers() {
    let fake = Fake::start(Knobs::default()).await;
    let mcp = start(&fake, json!({"trust": true})).await;
    assert_eq!(
        mcp.overview(),
        format!(
            "  web: running, MCP 2026-07-28, 4 tools\n     Streamable HTTP `http://{}`, headers \
             Authorization (from env TEST_TOKEN); 4 tools offered to the model, trusted (no \
             approval questions)\n",
            fake.addr
        )
    );
    assert_eq!(mcp.report()[0], "mcp: web started (4 tools, over HTTP)");
    let external = mcp.external_tools();
    assert_eq!(external.len(), 4);
    assert!(
        external
            .iter()
            .all(|(_, tool)| tool.trusted && tool.server == "web")
    );
}

#[test]
fn json_rpc_answers_are_matched_by_id() {
    let answer = json!({"jsonrpc": "2.0", "id": 3, "result": {"ok": true}});
    assert!(reply_to(&answer, 4).is_none());
    assert_eq!(reply_to(&answer, 3).unwrap().unwrap(), json!({"ok": true}));
    let request = json!({"jsonrpc": "2.0", "id": 3, "method": "ping"});
    assert!(reply_to(&request, 3).is_none());
    let error = json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -1, "message": "no"}});
    assert!(matches!(
        reply_to(&error, 3),
        Some(Err(HttpError::Rpc(RpcError { code: -1, .. })))
    ));
}
