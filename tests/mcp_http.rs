//! Runs the real binary against a fake API server and a fake MCP server
//! on loopback that speaks Streamable HTTP (MCP 2026-07-28): the model
//! calls the server's tool, the server gets its header from the variable
//! `mcp.json` names, and bash does not see that variable.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod common;
use common::{body_of, fake_api, text_reply};

/// The requests the fake MCP server read: headers (lower-case names) and
/// JSON body.
type Seen = Arc<Mutex<Vec<(Vec<(String, String)>, Value)>>>;

/// A modern MCP server with one tool, `echo`, answering `tools/call` as
/// an event stream and the rest as JSON.
async fn fake_mcp() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move { serve(socket, log).await });
        }
    });
    (url, seen)
}

async fn serve(mut socket: TcpStream, seen: Seen) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let (headers, body) = loop {
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
        let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_string();
        let headers: Vec<(String, String)> = head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .map_or(0, |(_, value)| value.parse::<usize>().unwrap());
        if buffer.len() >= end + 4 + length {
            let body: Value = serde_json::from_slice(&buffer[end + 4..end + 4 + length]).unwrap();
            break (headers, body);
        }
    };
    seen.lock().unwrap().push((headers, body.clone()));
    let id = body["id"].clone();
    let params = &body["params"];
    let (kind, text) = match body["method"].as_str().unwrap_or_default() {
        "server/discover" => (
            "application/json",
            json!({"jsonrpc": "2.0", "id": id, "result": {"resultType": "complete",
                "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}})
            .to_string(),
        ),
        "tools/list" => (
            "application/json",
            json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [
                {"name": "echo", "description": "Echoes", "inputSchema": {"type": "object",
                    "properties": {"text": {"type": "string"}}}}]}})
            .to_string(),
        ),
        "tools/call" => (
            "text/event-stream",
            format!(
                "id: 1\ndata:\n\ndata: {}\n\n",
                json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text",
                    "text": format!("echo: {}", params["arguments"]["text"].as_str().unwrap_or(""))}]}})
            ),
        ),
        _ => (
            "application/json",
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no"}})
                .to_string(),
        ),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\
         \r\n{text}",
        text.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// A state folder whose `mcp.json` lists the server at `url` as `web`.
/// The working folder under `dir`, beside the state folder: a working
/// folder holding the state folder gets a note from the bash sandbox.
fn work(dir: &Path) -> PathBuf {
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    work
}

fn home_with_server(dir: &Path, url: &str) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let list = json!({"servers": {"web": {
        "url": url,
        "headers": {"Authorization": "Bearer env:WEB_MCP_TOKEN"},
    }}});
    let path = home.join("mcp.json");
    std::fs::write(&path, list.to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    home
}

fn tool_calls(calls: &[(&str, &str, Value)]) -> Value {
    let content: Vec<Value> = calls
        .iter()
        .map(
            |(id, name, input)| json!({"type": "tool_use", "id": id, "name": name, "input": input}),
        )
        .collect();
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

#[tokio::test]
async fn the_model_calls_an_http_server_whose_token_bash_cannot_see() {
    let (mcp_url, seen) = fake_mcp().await;
    let (url, api) = fake_api(vec![
        (
            200,
            tool_calls(&[
                ("toolu_1", "mcp__web__echo", json!({"text": "안녕"})),
                (
                    "toolu_2",
                    "bash",
                    json!({"command": "echo \"[${WEB_MCP_TOKEN:-unset}]\""}),
                ),
            ]),
        ),
        (200, text_reply("done")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let home = home_with_server(dir.path(), &mcp_url);
    let output = common::junees_binary(&work(dir.path()), &home)
        .args(["-p", "go", "--no-session"])
        .env("ANTHROPIC_API_KEY", "sk-test")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &home)
        .env("JUNEES_STREAM", "0")
        .env("WEB_MCP_TOKEN", "web-token-value")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("mcp: web started (1 tool, over HTTP)"),
        "{stderr}"
    );
    assert!(!stderr.contains("web-token-value"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");

    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    let body = body_of(&requests[1]);
    let results = body["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    assert_eq!(results[0]["tool_use_id"], "toolu_1");
    assert_eq!(results[0]["content"], "echo: 안녕");
    assert_eq!(results[1]["content"], "[unset]\n[exit code 0]");

    let seen = seen.lock().unwrap().clone();
    let methods: Vec<&str> = seen
        .iter()
        .map(|(_, body)| body["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["server/discover", "tools/list", "tools/call"]);
    for (headers, _) in &seen {
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("authorization"), Some("Bearer web-token-value"));
        assert_eq!(header("mcp-protocol-version"), Some("2026-07-28"));
    }
}

#[tokio::test]
async fn an_unset_header_variable_keeps_the_server_out() {
    let (mcp_url, seen) = fake_mcp().await;
    let (url, api) = fake_api(vec![(200, text_reply("hi"))]).await;
    let dir = tempfile::tempdir().unwrap();
    let home = home_with_server(dir.path(), &mcp_url);
    let output = common::junees_binary(&work(dir.path()), &home)
        .args(["-p", "hello", "--no-session"])
        .env("ANTHROPIC_API_KEY", "sk-test")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &home)
        .env("JUNEES_STREAM", "0")
        .env_remove("WEB_MCP_TOKEN")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "mcp: web failed to start: header `Authorization` takes its value from the \
             variable WEB_MCP_TOKEN, which is not set"
        ),
        "{stderr}"
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    let tools = body_of(&requests[0])["tools"].to_string();
    assert!(!tools.contains("mcp__web"), "{tools}");
    assert!(seen.lock().unwrap().is_empty());
}
