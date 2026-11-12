//! Drives the real Anthropic client and binary against a fake API server
//! that streams server-sent events.

use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use junees::agent::{Agent, AgentOptions, Cancelled, Event};
use junees::cancel::Cancel;
use junees::message::{ContentBlock, Message};
use junees::provider::anthropic::AnthropicProvider;
use junees::provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, StreamEvent,
};
use junees::tools::ToolRegistry;
use serde_json::{Value, json};

mod common;
use common::{
    CLOSED_BY_CLIENT, Served, body_of, fake_api, fake_stream_api, sse_event, sse_stream, text_reply,
};

fn provider(url: String) -> AnthropicProvider {
    AnthropicProvider::new("sk-test".into(), url, "test-model".into()).unwrap()
}

fn streaming(url: String) -> AnthropicProvider {
    provider(url).with_streaming(true)
}

/// Calls `provider` with one user message and collects what it reported.
async fn call(
    provider: &AnthropicProvider,
) -> (Result<CompletionResponse, ProviderError>, Vec<StreamEvent>) {
    let messages = [Message::user_text("hi")];
    let request = CompletionRequest {
        system: "sys",
        messages: &messages,
        tools: &[],
        max_tokens: 2048,
        cache: true,
        thinking: false,
        web_search: false,
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let result = provider.complete_streaming(request, &sender).await;
    drop(sender);
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    (result, events)
}

/// A reply with every kind of block: thinking with its signature, redacted
/// thinking, text (some Hangul), a server tool call and its result, and a
/// tool call with nested input.
fn rich_message() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "thinking", "thinking": "Check the files first. 먼저 목록을 본다.",
             "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds+/=="},
            {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/LafPsn4a"},
            {"type": "text", "text": "목록을 확인하겠습니다. Let me look."},
            {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
             "input": {"query": "rust sse"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
             "content": [{"type": "web_search_result", "title": "SSE", "url": "https://x",
                          "encrypted_content": "Ev0DCioIAxgC", "page_age": null}]},
            {"type": "tool_use", "id": "toolu_1", "name": "bash",
             "input": {"command": "ls -la \"한글 폴더\"", "timeout_secs": 30,
                       "env": {"nested": [1, 2.5, true, null]}}}
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 120, "output_tokens": 87,
                  "cache_creation_input_tokens": 30, "cache_read_input_tokens": 900}
    })
}

fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .map(|event| match event {
            StreamEvent::TextDelta(text) => text.as_str(),
            StreamEvent::TextEnd => "|",
            StreamEvent::Thinking => "(thinking)",
            StreamEvent::Retrying => "(retrying)",
            StreamEvent::ServerToolUse { .. } => "(server tool)",
        })
        .collect()
}

#[tokio::test]
async fn a_streamed_reply_equals_the_plain_one() {
    let message = rich_message();
    let (url, server) = fake_stream_api(vec![
        Served::Json(200, message.clone()),
        Served::Stream {
            body: sse_stream(&message, 3),
            chunk: 7,
        },
    ])
    .await;
    let (plain, plain_events) = call(&provider(url.clone())).await;
    let (streamed, streamed_events) = call(&streaming(url)).await;
    let (plain, streamed) = (plain.unwrap(), streamed.unwrap());

    assert_eq!(streamed, plain);
    assert_eq!(streamed.content.len(), 6);
    assert_eq!(streamed.usage.cache_read_input_tokens, 900);
    assert_eq!(streamed.usage.output_tokens, 87);
    // The same blocks are reported, the streamed text in pieces.
    assert_eq!(text_of(&plain_events), text_of(&streamed_events));
    assert_eq!(
        text_of(&streamed_events),
        "(thinking)(thinking)목록을 확인하겠습니다. Let me look.|(server tool)"
    );
    assert!(streamed_events.contains(&StreamEvent::ServerToolUse {
        name: "web_search".into(),
        input: json!({"query": "rust sse"}),
    }));
    let deltas = streamed_events
        .iter()
        .filter(|event| matches!(event, StreamEvent::TextDelta(_)))
        .count();
    assert_eq!(
        deltas,
        "목록을 확인하겠습니다. Let me look."
            .chars()
            .count()
            .div_ceil(3)
    );

    let requests = server.await.unwrap();
    assert!(body_of(&requests[0]).get("stream").is_none());
    let streamed_body = body_of(&requests[1]);
    assert_eq!(streamed_body["stream"], true);
    assert_eq!(streamed_body["model"], "test-model");
}

#[tokio::test]
async fn a_whole_json_answer_to_a_stream_request_is_read_as_is() {
    let (url, server) = fake_api(vec![(200, text_reply("not streamed"))]).await;
    let (response, events) = call(&streaming(url)).await;
    assert_eq!(
        response.unwrap().content,
        vec![ContentBlock::text("not streamed")]
    );
    assert_eq!(text_of(&events), "not streamed|");
    assert_eq!(body_of(&server.await.unwrap()[0])["stream"], true);
}

/// The first events of a text reply: message_start and `text` so far.
fn opening(text: &str) -> String {
    let start = json!({"type": "message_start", "message": {"id": "m", "type": "message",
        "role": "assistant", "content": [], "stop_reason": null,
        "usage": {"input_tokens": 5, "output_tokens": 1}}});
    let mut body = sse_event(&start);
    if !text.is_empty() {
        body += &sse_event(&json!({"type": "content_block_start", "index": 0,
                                   "content_block": {"type": "text", "text": ""}}));
        body += &sse_event(&json!({"type": "content_block_delta", "index": 0,
                                   "delta": {"type": "text_delta", "text": text}}));
    }
    body
}

fn error_event(kind: &str, message: &str) -> String {
    sse_event(&json!({"type": "error", "error": {"type": kind, "message": message}}))
}

fn stream(body: String) -> Served {
    Served::Stream { body, chunk: 5 }
}

#[tokio::test]
async fn an_error_event_mid_stream_retries_from_the_start() {
    let reply = text_reply("Hallo, Welt.");
    let (url, server) = fake_stream_api(vec![
        stream(opening("Hal") + &error_event("overloaded_error", "Overloaded")),
        stream(sse_stream(&reply, 4)),
    ])
    .await;
    let (response, events) = call(&streaming(url)).await;
    assert_eq!(
        response.unwrap().content,
        vec![ContentBlock::text("Hallo, Welt.")]
    );
    assert_eq!(text_of(&events), "Hal(retrying)Hallo, Welt.|");
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_dropped_stream_retries_and_says_so_only_after_output() {
    let reply = text_reply("done");
    let (url, server) = fake_stream_api(vec![
        // Cut before anything was shown, then after some text, and a
        // status error before the stream starts.
        stream(opening("")),
        stream(opening("do")),
        Served::Json(
            529,
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
        ),
        stream(sse_stream(&reply, 2)),
    ])
    .await;
    let (response, events) = call(&streaming(url)).await;
    assert_eq!(response.unwrap().content, vec![ContentBlock::text("done")]);
    assert_eq!(text_of(&events), "do(retrying)done|");
    assert_eq!(server.await.unwrap().len(), 4);
}

#[tokio::test]
async fn a_non_retryable_error_event_ends_the_call() {
    let (url, server) = fake_stream_api(vec![stream(
        opening("") + &error_event("invalid_request_error", "prompt is too long: 9 > 8"),
    )])
    .await;
    let (response, events) = call(&streaming(url)).await;
    let err = response.unwrap_err();
    assert!(err.is_context_overflow(), "{err}");
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn retries_give_up_after_four_attempts() {
    let overloaded = || stream(opening("x") + &error_event("overloaded_error", "Overloaded"));
    let (url, server) = fake_stream_api((0..4).map(|_| overloaded()).collect()).await;
    let (response, events) = call(&streaming(url)).await;
    assert_eq!(
        response.unwrap_err().to_string(),
        "API error 529 (overloaded_error): Overloaded"
    );
    // Shown four times, voided three times.
    assert_eq!(text_of(&events), "x(retrying)x(retrying)x(retrying)x");
    assert_eq!(server.await.unwrap().len(), 4);
}

#[tokio::test]
async fn a_stalled_stream_times_out_and_is_retried() {
    let (url, server) = fake_stream_api(vec![
        Served::Stall {
            body: opening("wai") + &sse_event(&json!({"type": "ping"})),
        },
        stream(sse_stream(&text_reply("waited"), 3)),
    ])
    .await;
    let provider = streaming(url).with_idle_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let (response, events) = call(&provider).await;
    assert_eq!(
        response.unwrap().content,
        vec![ContentBlock::text("waited")]
    );
    assert_eq!(text_of(&events), "wai(retrying)waited|");
    // 0.3 s idle and 0.5 s backoff, not the 300 s default.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let requests = server.await.unwrap();
    assert!(requests[0].ends_with(CLOSED_BY_CLIENT));
}

fn options() -> AgentOptions {
    AgentOptions {
        system: "sys".into(),
        max_tokens: 64,
        max_turns: 5,
        context_window: 200_000,
        compact_threshold: 0.8,
    }
}

#[tokio::test]
async fn cancelling_closes_the_stream_at_once() {
    let (url, server) = fake_stream_api(vec![Served::Stall {
        body: opening("Thinking about i"),
    }])
    .await;
    let cancel = Cancel::new();
    let mut agent = Agent::new(Box::new(streaming(url)), ToolRegistry::new(), options())
        .with_cancel(cancel.clone());
    let mut shown = String::new();
    let started = Instant::now();
    let err = agent
        .run("go", |event| {
            if let Event::TextDelta(text) = event {
                shown.push_str(text);
                cancel.cancel();
            }
        })
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<Cancelled>().is_some(), "{err}");
    assert_eq!(shown, "Thinking about i");
    assert_eq!(agent.messages(), [Message::user_text("go")]);
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the connection stayed open")
        .unwrap();
    assert!(requests[0].ends_with(CLOSED_BY_CLIENT));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// The binary in a temporary folder, talking to the fake API, saving no
/// session.
async fn junees(url: &str, extra_env: &[(&str, &str)]) -> Output {
    // The working folder is below HOME, not HOME itself: Junees's state
    // folder (`~/.junees`, made on first use) inside the working folder
    // would add a sandbox line to bash results.
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let mut command = common::junees_binary(&cwd, home.path());
    command
        .args(["--no-session", "-p", "go"])
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .envs(extra_env.iter().copied())
        .stdin(Stdio::null());
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "junees failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn bash_call(text: &str, command: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": text},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": command}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

#[tokio::test]
async fn the_binary_prints_streamed_text_once() {
    let first = bash_call("확인해 볼게요.", "echo streamed-e2e");
    let last = text_reply("It printed streamed-e2e. 끝.");
    let (url, server) = fake_stream_api(vec![
        Served::Stream {
            body: sse_stream(&first, 2),
            chunk: 3,
        },
        Served::Stream {
            body: sse_stream(&last, 2),
            chunk: 3,
        },
    ])
    .await;
    let output = junees(&url, &[]).await;
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "확인해 볼게요.\nIt printed streamed-e2e. 끝.\n"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-> bash"), "{stderr}");
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let second = body_of(&requests[1]);
    assert_eq!(second["stream"], true);
    // The streamed tool call went back as the API sent it, and ran.
    assert_eq!(second["messages"][1]["content"], first["content"]);
    assert_eq!(
        second["messages"][2]["content"][0]["content"],
        "streamed-e2e\n[exit code 0]"
    );
}

#[tokio::test]
async fn the_binary_says_when_it_retries() {
    let (url, server) = fake_stream_api(vec![
        stream(opening("Hal") + &error_event("overloaded_error", "Overloaded")),
        stream(sse_stream(&text_reply("Hallo."), 2)),
    ])
    .await;
    let output = junees(&url, &[]).await;
    // stdout is a pipe: the cut-off attempt is dropped, the answer comes
    // once.
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hallo.\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("  (retrying)\n"));
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_retry_after_a_finished_text_block_prints_that_block_once() {
    let reply = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "First."}, {"type": "text", "text": "Second."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 4}
    });
    // The first block is complete before the stream breaks off.
    let broken = opening("First.")
        + &sse_event(&json!({"type": "content_block_stop", "index": 0}))
        + &sse_event(&json!({"type": "content_block_start", "index": 1,
                             "content_block": {"type": "text", "text": ""}}))
        + &sse_event(&json!({"type": "content_block_delta", "index": 1,
                             "delta": {"type": "text_delta", "text": "Sec"}}))
        + &error_event("overloaded_error", "Overloaded");
    let (url, server) = fake_stream_api(vec![stream(broken), stream(sse_stream(&reply, 3))]).await;
    let output = junees(&url, &[]).await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "First.\nSecond.\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("  (retrying)\n"));
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn with_streaming_off_the_binary_asks_for_whole_replies() {
    let (url, server) = fake_api(vec![(200, text_reply("whole"))]).await;
    let output = junees(&url, &[("JUNEES_STREAM", "0")]).await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "whole\n");
    let requests = server.await.unwrap();
    assert!(body_of(&requests[0]).get("stream").is_none());
}
