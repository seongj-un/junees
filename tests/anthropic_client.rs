//! Drives the real Anthropic client against a local fake API server.

use junees::agent::{Agent, AgentOptions};
use junees::message::{ContentBlock, Message};
use junees::provider::anthropic::AnthropicProvider;
use junees::provider::{CompletionRequest, Provider};
use junees::tools::ToolRegistry;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Serves the given (status, body) pairs to consecutive connections and
/// returns every raw request it received.
async fn fake_api(responses: Vec<(u16, Value)>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n\
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

fn body_of(request: &str) -> Value {
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    serde_json::from_str(body).unwrap()
}

fn text_reply(text: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

fn provider(url: String) -> AnthropicProvider {
    AnthropicProvider::new("sk-test".into(), url, "test-model".into()).unwrap()
}

#[tokio::test]
async fn sends_headers_and_body() {
    let (url, server) = fake_api(vec![(200, text_reply("hello"))]).await;
    let messages = [Message::user_text("hi")];
    let response = provider(url)
        .complete(CompletionRequest {
            system: "sys",
            messages: &messages,
            tools: &[],
            max_tokens: 64,
        })
        .await
        .unwrap();
    assert_eq!(
        response.content,
        vec![ContentBlock::Text {
            text: "hello".into()
        }]
    );

    let requests = server.await.unwrap();
    let head = requests[0].to_lowercase();
    assert!(head.starts_with("post /v1/messages http/1.1"));
    assert!(head.contains("x-api-key: sk-test"));
    assert!(head.contains("anthropic-version: 2023-06-01"));
    let body = body_of(&requests[0]);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(body["system"], "sys");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
}

#[tokio::test]
async fn retries_when_overloaded() {
    let overloaded =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    let (url, server) = fake_api(vec![(529, overloaded), (200, text_reply("ok"))]).await;
    let messages = [Message::user_text("hi")];
    let response = provider(url)
        .complete(CompletionRequest {
            system: "",
            messages: &messages,
            tools: &[],
            max_tokens: 64,
        })
        .await
        .unwrap();
    assert_eq!(
        response.content,
        vec![ContentBlock::Text { text: "ok".into() }]
    );
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn does_not_retry_auth_errors() {
    let denied = json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}});
    let (url, server) = fake_api(vec![(401, denied)]).await;
    let messages = [Message::user_text("hi")];
    let err = provider(url)
        .complete(CompletionRequest {
            system: "",
            messages: &messages,
            tools: &[],
            max_tokens: 64,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("authentication_error"), "{err}");
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn agent_runs_bash_through_the_real_client() {
    let tool_call = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": "Running it."},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "echo junees-e2e"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let (url, server) = fake_api(vec![
        (200, tool_call),
        (200, text_reply("It printed junees-e2e.")),
    ])
    .await;

    let mut agent = Agent::new(
        Box::new(provider(url)),
        ToolRegistry::with_defaults(),
        AgentOptions {
            system: "sys".into(),
            max_tokens: 64,
            max_turns: 5,
        },
    );
    let answer = agent.run("run echo", |_| {}).await.unwrap();
    assert_eq!(answer, "It printed junees-e2e.");

    let requests = server.await.unwrap();
    let first = body_of(&requests[0]);
    let tool_names: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, ["bash", "read_file", "write_file"]);

    let second = body_of(&requests[1]);
    let result = &second["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_1");
    assert_eq!(result["content"], "junees-e2e\n[exit code 0]");
}
