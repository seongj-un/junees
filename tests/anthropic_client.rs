//! Drives the real Anthropic client against a local fake API server.

use std::time::{Duration, Instant};

use junees::agent::{Agent, AgentOptions};
use junees::message::{ContentBlock, Message};
use junees::provider::anthropic::AnthropicProvider;
use junees::provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use junees::tools::{ToolRegistry, ToolSettings};
use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, text_reply};

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
            cache: true,
            thinking: false,
            web_search: false,
        })
        .await
        .unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("hello")]);

    let requests = server.await.unwrap();
    let head = requests[0].to_lowercase();
    assert!(head.starts_with("post /v1/messages http/1.1"));
    assert!(head.contains("x-api-key: sk-test"));
    assert!(head.contains("anthropic-version: 2023-06-01"));
    let body = body_of(&requests[0]);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}])
    );
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
            cache: true,
            thinking: false,
            web_search: false,
        })
        .await
        .unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("ok")]);
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
            cache: true,
            thinking: false,
            web_search: false,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("authentication_error"), "{err}");
    assert!(err.is_auth());
    assert_eq!(server.await.unwrap().len(), 1);
}

async fn complete_hi(url: String) -> Result<CompletionResponse, ProviderError> {
    let messages = [Message::user_text("hi")];
    provider(url)
        .complete(CompletionRequest {
            system: "",
            messages: &messages,
            tools: &[],
            max_tokens: 64,
            cache: true,
            thinking: false,
            web_search: false,
        })
        .await
}

#[tokio::test]
async fn gives_up_after_four_attempts_and_honors_retry_after() {
    let overloaded =
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    let (url, server) = fake_api(vec![(529, overloaded); 4]).await;
    let started = Instant::now();
    let err = complete_hi(url).await.unwrap_err();
    // retry-after: 0 replaces the 0.5 + 1 + 2 s default backoff.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        err.to_string(),
        "API error 529 (overloaded_error): Overloaded"
    );
    assert_eq!(server.await.unwrap().len(), 4);
}

#[tokio::test]
async fn context_overflow_is_typed_and_not_retried() {
    let too_long = json!({"type": "error", "error": {
        "type": "invalid_request_error",
        "message": "prompt is too long: 215000 tokens > 200000 maximum"
    }});
    let (url, server) = fake_api(vec![(400, too_long)]).await;
    let err = complete_hi(url).await.unwrap_err();
    assert!(err.is_context_overflow(), "{err}");
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn retries_a_connection_closed_before_the_response() {
    let (url, server) = fake_api(vec![(0, Value::Null), (200, text_reply("ok"))]).await;
    let response = complete_hi(url).await.unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("ok")]);
    assert_eq!(server.await.unwrap().len(), 2);
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
        ToolRegistry::with_defaults(&ToolSettings::default()),
        AgentOptions {
            system: "sys".into(),
            max_tokens: 64,
            max_turns: 5,
            context_window: 200_000,
            compact_threshold: 0.8,
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
    assert_eq!(
        tool_names,
        [
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "grep",
            "glob",
            "list_dir"
        ]
    );

    let second = body_of(&requests[1]);
    let result = &second["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "toolu_1");
    assert_eq!(result["content"], "junees-e2e\n[exit code 0]");
}

#[tokio::test]
async fn thinking_blocks_go_back_with_the_tool_results_unchanged() {
    let thinking = json!({
        "type": "thinking",
        "thinking": "The user wants an echo. 먼저 bash로 확인한다.",
        "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds+/==",
    });
    let redacted = json!({"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/LafPsn4a"});
    let tool_call = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            thinking,
            redacted,
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "echo junees-think"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 50}
    });
    let (url, server) = fake_api(vec![
        (200, tool_call.clone()),
        (200, text_reply("It printed junees-think.")),
    ])
    .await;

    let mut agent = Agent::new(
        Box::new(provider(url).with_thinking(1024)),
        ToolRegistry::with_defaults(&ToolSettings::default()),
        AgentOptions {
            system: "sys".into(),
            max_tokens: 2048,
            max_turns: 5,
            context_window: 200_000,
            compact_threshold: 0.8,
        },
    );
    let mut thoughts = 0;
    let answer = agent
        .run("run echo", |event| {
            thoughts += matches!(event, junees::agent::Event::Thinking) as usize;
        })
        .await
        .unwrap();
    assert_eq!(answer, "It printed junees-think.");
    assert_eq!(thoughts, 2);

    let requests = server.await.unwrap();
    for request in &requests {
        assert_eq!(
            body_of(request)["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
    }
    // The assistant turn goes back exactly as it came, signature and all,
    // and the cache mark sits on the tool result, not on a thinking block.
    let second = body_of(&requests[1]);
    assert_eq!(
        second["messages"][1],
        json!({"role": "assistant", "content": tool_call["content"]})
    );
    let result = &second["messages"][2]["content"][0];
    assert_eq!(result["tool_use_id"], "toolu_1");
    assert_eq!(result["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(second.to_string().matches("cache_control").count(), 3);
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_the_key_stays() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    // Where the redirect points: it must never get a connection.
    let elsewhere = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}/v1/messages", elsewhere.local_addr().unwrap());
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut requests = 0;
        // One answer per attempt the client makes.
        while let Ok(Ok((mut socket, _))) =
            tokio::time::timeout(Duration::from_secs(2), proxy.accept()).await
        {
            let mut buf = vec![0u8; 65536];
            let _ = socket.read(&mut buf).await;
            requests += 1;
            let reply = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {target}\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
        requests
    });
    let err = complete_hi(url).await.unwrap_err();
    assert!(err.to_string().contains("307"), "{err}");
    let followed = tokio::time::timeout(Duration::from_millis(500), elsewhere.accept()).await;
    assert!(followed.is_err(), "the client followed the redirect");
    assert_eq!(server.await.unwrap(), 1, "a 3xx is not retried");
}
