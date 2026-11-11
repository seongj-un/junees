//! Helpers shared by the integration tests: the junees binary without the
//! test's own settings and credentials, and a fake Anthropic API server.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Variables that change what the binary does or which account it uses:
/// all of Junees's own settings, the API's, Claude Code's and the GitHub
/// tokens. A developer's `JUNEES_BACKEND=claude-code` would otherwise make
/// a test call the real `claude`.
pub const SCRUBBED_PREFIXES: &[&str] = &["JUNEES_", "ANTHROPIC_", "CLAUDE_CODE_"];
pub const SCRUBBED_NAMES: &[&str] = &["GH_TOKEN", "GITHUB_TOKEN"];

/// Whether a binary test must not pass on the variable `name`.
pub fn scrubbed(name: &str) -> bool {
    SCRUBBED_NAMES.contains(&name)
        || SCRUBBED_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Removes from `command` each of `names` that [`scrubbed`] matches.
pub fn scrub_env<I, S>(command: &mut tokio::process::Command, names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    for name in names {
        let name = name.as_ref();
        if scrubbed(&name.to_string_lossy()) {
            command.env_remove(name);
        }
    }
}

/// The junees binary run in `cwd` with `home` as its `HOME`, without any
/// variable of the test's environment that [`scrubbed`] matches. Every
/// binary test starts here and sets what else it needs afterwards.
///
/// Both folders must lie outside the crate (see [`outside_the_crate`]), so
/// a binary test's bash never runs in the crate's repository (where a bare
/// `git push` reaches its upstream) or in the real home that holds it.
pub fn junees_binary(cwd: &Path, home: &Path) -> tokio::process::Command {
    for (what, path) in [("cwd", cwd), ("HOME", home)] {
        assert!(
            outside_the_crate(path),
            "a binary test's {what} must be a temporary folder, not {}",
            path.display()
        );
    }
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    scrub_env(&mut command, std::env::vars_os().map(|(name, _)| name));
    command.current_dir(cwd).env("HOME", home);
    command
}

/// Whether `path` is an existing folder that is neither the crate's
/// folder, inside it, nor above it (the real home, `/`). A relative path
/// counts from the test's own cwd, the crate, so it never passes.
pub fn outside_the_crate(path: &Path) -> bool {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    path.is_dir() && !path.starts_with(&crate_dir) && !crate_dir.starts_with(&path)
}

/// Serves the given (status, body) pairs to consecutive connections and
/// returns every raw request it received. Error responses carry
/// `retry-after: 0` so retries do not slow the tests down. Status 0 closes
/// the connection without answering.
pub async fn fake_api(responses: Vec<(u16, Value)>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            if status == 0 {
                drop(socket);
                continue;
            }
            let body = body.to_string();
            let retry_after = if status >= 400 {
                "retry-after: 0\r\n"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n{retry_after}\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
        requests
    });
    (url, handle)
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            if buf.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(buf).unwrap()
}

pub fn body_of(request: &str) -> Value {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

pub fn text_reply(text: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

/// One reply of [`fake_stream_api`].
pub enum Served {
    /// A whole response, as [`fake_api`] serves it (status 0 closes the
    /// connection without answering).
    Json(u16, Value),
    /// A 200 `text/event-stream` with this body, written in pieces of
    /// `chunk` bytes, so events and lines (and UTF-8 characters) span
    /// pieces. The connection then closes.
    Stream { body: String, chunk: usize },
    /// Like `Stream`, but the connection then stays open and idle until the
    /// client closes it (up to 10 s). If it did, the request is recorded
    /// with [`CLOSED_BY_CLIENT`] after it.
    Stall { body: String },
}

/// Appended to a stalled request once the client closed the connection.
pub const CLOSED_BY_CLIENT: &str = "\n<closed by the client>";

/// Serves the given replies to consecutive connections and returns every
/// raw request it received.
pub async fn fake_stream_api(replies: Vec<Served>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = read_request(&mut socket).await;
            match reply {
                Served::Json(0, _) => drop(socket),
                Served::Json(status, body) => {
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n\
                         retry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.ok();
                }
                Served::Stream { body, chunk } => {
                    write_stream(&mut socket, &body, chunk).await;
                    socket.shutdown().await.ok();
                }
                Served::Stall { body } => {
                    write_stream(&mut socket, &body, body.len().max(1)).await;
                    let mut rest = [0u8; 64];
                    let closed = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            match socket.read(&mut rest).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    })
                    .await;
                    if closed.is_ok() {
                        request.push_str(CLOSED_BY_CLIENT);
                    }
                }
            }
            requests.push(request);
        }
        requests
    });
    (url, handle)
}

async fn write_stream(socket: &mut TcpStream, body: &str, chunk: usize) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\n\
                cache-control: no-cache\r\nconnection: close\r\n\r\n";
    socket.write_all(head.as_bytes()).await.unwrap();
    for piece in body.as_bytes().chunks(chunk) {
        // The client may have gone; the test sees that elsewhere.
        if socket.write_all(piece).await.is_err() {
            return;
        }
        socket.flush().await.ok();
        // Let the client read this piece on its own.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// One server-sent event.
pub fn sse_event(data: &Value) -> String {
    format!(
        "event: {}\ndata: {data}\n\n",
        data["type"].as_str().unwrap()
    )
}

/// The event stream the API sends for `message` (a whole Messages API
/// response), as the streaming docs describe it: text, thinking and tool
/// input arrive in deltas of `piece` characters, each citation of a text in
/// a delta of its own, other blocks whole in
/// their start event, input usage in message_start and the cumulative
/// usage in message_delta. A ping comes after the first block starts.
pub fn sse_stream(message: &Value, piece: usize) -> String {
    let mut events = Vec::new();
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(1);
    events.push(json!({"type": "message_start", "message": start}));
    let deltas = |events: &mut Vec<Value>, index: usize, kind: &str, field: &str, text: &str| {
        let chars: Vec<char> = text.chars().collect();
        for part in chars.chunks(piece) {
            let part: String = part.iter().collect();
            events.push(json!({"type": "content_block_delta", "index": index,
                               "delta": {"type": kind, field: part}}));
        }
    };
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let kind = block["type"].as_str().unwrap();
        let mut opened = block.clone();
        match kind {
            "text" => {
                opened["text"] = json!("");
                // Citations come in deltas of their own.
                opened.as_object_mut().unwrap().remove("citations");
            }
            "thinking" => {
                opened["thinking"] = json!("");
                opened["signature"] = json!("");
            }
            "tool_use" | "server_tool_use" => opened["input"] = json!({}),
            _ => {}
        }
        events.push(json!({"type": "content_block_start", "index": index,
                           "content_block": opened}));
        if index == 0 {
            events.push(json!({"type": "ping"}));
        }
        match kind {
            "text" => {
                for citation in block["citations"].as_array().into_iter().flatten() {
                    events.push(json!({"type": "content_block_delta", "index": index,
                        "delta": {"type": "citations_delta", "citation": citation}}));
                }
                deltas(
                    &mut events,
                    index,
                    "text_delta",
                    "text",
                    block["text"].as_str().unwrap(),
                )
            }
            "thinking" => {
                let thinking = block["thinking"].as_str().unwrap();
                deltas(&mut events, index, "thinking_delta", "thinking", thinking);
                events.push(json!({"type": "content_block_delta", "index": index,
                    "delta": {"type": "signature_delta", "signature": block["signature"]}}));
            }
            "tool_use" | "server_tool_use" => {
                let input = block["input"].to_string();
                deltas(
                    &mut events,
                    index,
                    "input_json_delta",
                    "partial_json",
                    &input,
                );
            }
            _ => {}
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta",
        "delta": {"stop_reason": message["stop_reason"], "stop_sequence": null},
        "usage": message["usage"]}));
    events.push(json!({"type": "message_stop"}));
    events.iter().map(sse_event).collect()
}
