//! Drives the real OpenAI-compatible client against a fake Chat
//! Completions server on loopback: plain and streamed answers, tool calls
//! (parallel ones, their results and images going back), usage, error
//! mapping and retries, and the binary set to `provider = openai`.

use std::process::Stdio;
use std::time::Duration;

use junees::agent::{Agent, AgentOptions};
use junees::message::{ContentBlock, Message, StopReason};
use junees::provider::openai::{OpenAiProvider, Settings};
use junees::provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, StreamEvent,
};
use junees::tools::{ReadFile, ToolRegistry};
use serde_json::{Value, json};

mod common;
use common::{Served, body_of, fake_api, fake_stream_api};

fn settings(url: &str, key: Option<&str>) -> Settings {
    Settings {
        base_url: format!("{url}/v1"),
        api_key: key.map(str::to_string),
    }
}

fn provider(url: &str) -> OpenAiProvider {
    OpenAiProvider::new(settings(url, Some("sk-fake-openai")), "test-model".into()).unwrap()
}

fn request<'a>(messages: &'a [Message]) -> CompletionRequest<'a> {
    CompletionRequest {
        system: "sys",
        messages,
        tools: &[],
        max_tokens: 64,
        cache: true,
        thinking: true,
        web_search: true,
    }
}

fn answer(text: &str) -> Value {
    json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "test-model",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": text, "refusal": null}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}
    })
}

/// A Chat Completions event stream of these chunks, then `[DONE]`.
fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn delta(delta: Value) -> Value {
    json!({"object": "chat.completion.chunk", "model": "test-model",
           "choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
}

fn finish(reason: &str) -> Value {
    json!({"object": "chat.completion.chunk",
           "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage(prompt: u64, cached: u64, completion: u64) -> Value {
    json!({"object": "chat.completion.chunk", "choices": [],
           "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
                     "total_tokens": prompt + completion,
                     "prompt_tokens_details": {"cached_tokens": cached}}})
}

async fn served<T>(server: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .expect("the fake server got fewer requests")
        .unwrap()
}

#[tokio::test]
async fn sends_a_chat_completions_request_and_reads_the_answer() {
    let (url, server) = fake_api(vec![(200, answer("hello"))]).await;
    let messages = [Message::user_text("hi")];
    let response = provider(&url).complete(request(&messages)).await.unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("hello")]);
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        (response.usage.input_tokens, response.usage.output_tokens),
        (12, 3)
    );

    let requests = served(server).await;
    let head = requests[0].to_lowercase();
    assert!(
        head.starts_with("post /v1/chat/completions http/1.1"),
        "{head}"
    );
    assert!(
        head.contains("authorization: bearer sk-fake-openai"),
        "{head}"
    );
    assert!(!head.contains("x-api-key"), "{head}");
    let body = body_of(&requests[0]);
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"}
            ],
            "max_tokens": 64
        })
    );
}

#[tokio::test]
async fn a_server_on_loopback_is_asked_without_a_key() {
    let (url, server) = fake_api(vec![(200, answer("local"))]).await;
    let local = OpenAiProvider::new(settings(&url, None), "llama".into()).unwrap();
    let messages = [Message::user_text("hi")];
    local.complete(request(&messages)).await.unwrap();
    let requests = served(server).await;
    assert!(
        !requests[0].to_lowercase().contains("authorization:"),
        "{}",
        requests[0]
    );
}

#[tokio::test]
async fn errors_map_to_the_provider_errors_and_only_some_are_retried() {
    let error = |kind: &str, code: &str, message: &str| json!({"error": {"message": message, "type": kind, "param": null, "code": code}});
    let messages = [Message::user_text("hi")];
    // A rate limit is retried (retry-after: 0 from the fake).
    let (url, server) = fake_api(vec![
        (
            429,
            error("requests", "rate_limit_exceeded", "Rate limit reached"),
        ),
        (
            503,
            error("service_unavailable_error", "server_is_overloaded", "busy"),
        ),
        (200, answer("ok")),
    ])
    .await;
    let response = provider(&url).complete(request(&messages)).await.unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("ok")]);
    assert_eq!(served(server).await.len(), 3);

    // An account out of credit, a refused key and an overflow are not.
    for (status, body, check) in [
        (
            429,
            error(
                "insufficient_quota",
                "insufficient_quota",
                "You exceeded your current quota",
            ),
            (|err: &ProviderError| err.to_string().contains("exceeded your current quota"))
                as fn(&ProviderError) -> bool,
        ),
        (
            401,
            error(
                "invalid_request_error",
                "invalid_api_key",
                "Incorrect API key provided",
            ),
            |err| err.is_auth(),
        ),
        (
            400,
            error(
                "invalid_request_error",
                "context_length_exceeded",
                "This model's maximum context length is 8192 tokens.",
            ),
            |err| err.is_context_overflow(),
        ),
    ] {
        let (url, server) = fake_api(vec![(status, body)]).await;
        let err = provider(&url)
            .complete(request(&messages))
            .await
            .unwrap_err();
        assert!(check(&err), "{status}: {err}");
        assert_eq!(served(server).await.len(), 1, "{status}");
    }
}

#[tokio::test]
async fn an_error_in_a_200_body_maps_to_its_status_and_is_retried_when_it_may_help() {
    let messages = [Message::user_text("hi")];
    // OpenRouter: an error after the model started comes with status 200.
    let (url, server) = fake_api(vec![
        (
            200,
            json!({"error": {"code": 502, "message": "Provider returned error"}}),
        ),
        (200, answer("ok")),
    ])
    .await;
    let response = provider(&url).complete(request(&messages)).await.unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("ok")]);
    assert_eq!(served(server).await.len(), 2);

    let (url, server) = fake_api(vec![(
        200,
        json!({"error": {"code": 400, "message": "Provider rejected the request"}}),
    )])
    .await;
    let err = provider(&url)
        .complete(request(&messages))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ProviderError::Api { status: 400, message, .. }
            if message == "Provider rejected the request"),
        "{err}"
    );
    assert_eq!(served(server).await.len(), 1);
}

fn streaming(url: &str) -> OpenAiProvider {
    provider(url).with_streaming(true)
}

async fn stream_call(
    provider: &OpenAiProvider,
    messages: &[Message],
) -> (Result<CompletionResponse, ProviderError>, Vec<StreamEvent>) {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let result = provider
        .complete_streaming(request(messages), &sender)
        .await;
    drop(sender);
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        events.push(event);
    }
    (result, events)
}

#[tokio::test]
async fn a_stream_is_put_together_and_asks_for_usage() {
    let body = sse(&[
        delta(json!({"role": "assistant", "content": ""})),
        delta(json!({"content": "안녕"})),
        delta(json!({"content": "하세요"})),
        finish("stop"),
        usage(100, 40, 9),
    ]);
    // Pieces of 7 bytes split events, lines and Hangul characters.
    let (url, server) = fake_stream_api(vec![Served::Stream { body, chunk: 7 }]).await;
    let messages = [Message::user_text("hi")];
    let (response, events) = stream_call(&streaming(&url), &messages).await;
    let response = response.unwrap();
    assert_eq!(response.content, vec![ContentBlock::text("안녕하세요")]);
    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta("안녕".into()),
            StreamEvent::TextDelta("하세요".into()),
            StreamEvent::TextEnd
        ]
    );
    assert_eq!(
        (
            response.usage.input_tokens,
            response.usage.cache_read_input_tokens,
            response.usage.output_tokens
        ),
        (60, 40, 9)
    );
    let requests = served(server).await;
    let body = body_of(&requests[0]);
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn a_stream_cut_off_is_retried_and_a_whole_answer_is_taken() {
    let cut = sse(&[delta(json!({"content": "Hal"}))]).replace("data: [DONE]\n\n", "");
    let (url, server) = fake_stream_api(vec![
        Served::Stream {
            body: cut,
            chunk: 64,
        },
        // A server that answers a stream request whole.
        Served::Json(200, answer("Hello")),
    ])
    .await;
    let messages = [Message::user_text("hi")];
    let (response, events) = stream_call(&streaming(&url), &messages).await;
    assert_eq!(response.unwrap().content, vec![ContentBlock::text("Hello")]);
    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta("Hal".into()),
            StreamEvent::Retrying,
            StreamEvent::TextDelta("Hello".into()),
            StreamEvent::TextEnd
        ]
    );
    assert_eq!(served(server).await.len(), 2);

    // An error chunk with a status that is not retried ends the call.
    let refused = sse(&[json!({"error": {"code": 400, "message": "bad request"}})]);
    let (url, server) = fake_stream_api(vec![Served::Stream {
        body: refused,
        chunk: 64,
    }])
    .await;
    let (response, _) = stream_call(&streaming(&url), &messages).await;
    assert!(
        matches!(response, Err(ProviderError::Api { status: 400, .. })),
        "{response:?}"
    );
    assert_eq!(served(server).await.len(), 1);
}

/// A PNG header Junees takes for a 2x2 picture.
fn png() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xaeB`\x82");
    bytes
}

#[tokio::test]
async fn the_agent_runs_parallel_tool_calls_and_sends_results_and_images_back() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, "first line\n").unwrap();
    let shot = dir.path().join("shot.png");
    std::fs::write(&shot, png()).unwrap();
    let args = |path: &std::path::Path| json!({"path": path.display().to_string()}).to_string();
    let (notes_args, shot_args) = (args(&notes), args(&shot));
    let split = notes_args.len() / 2;
    let calls = sse(&[
        delta(json!({"content": "Reading both."})),
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
            "function": {"name": "read_file", "arguments": &notes_args[..split]}}]}),
        ),
        delta(
            json!({"tool_calls": [{"index": 1, "id": "call_b", "type": "function",
            "function": {"name": "read_file", "arguments": shot_args}}]}),
        ),
        delta(json!({"tool_calls": [{"index": 0,
            "function": {"arguments": &notes_args[split..]}}]})),
        finish("tool_calls"),
        usage(50, 0, 20),
    ]);
    let done = sse(&[
        delta(json!({"content": "A note and a picture."})),
        finish("stop"),
        usage(80, 50, 6),
    ]);
    let (url, server) = fake_stream_api(vec![
        Served::Stream {
            body: calls,
            chunk: 16,
        },
        Served::Stream {
            body: done,
            chunk: 16,
        },
    ])
    .await;

    let mut tools = ToolRegistry::new();
    tools.register(ReadFile::default());
    let mut agent = Agent::new(
        Box::new(streaming(&url)),
        tools,
        AgentOptions {
            system: "sys".into(),
            max_tokens: 64,
            max_turns: 5,
            context_window: 200_000,
            compact_threshold: 0.8,
        },
    );
    let answer = agent.run("read them", |_| {}).await.unwrap();
    assert_eq!(answer, "A note and a picture.");

    let requests = served(server).await;
    let first = body_of(&requests[0]);
    assert_eq!(first["tools"][0]["type"], "function");
    assert_eq!(first["tools"][0]["function"]["name"], "read_file");
    assert!(first["tools"][0]["function"]["parameters"].is_object());

    let second = body_of(&requests[1]);
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages[0], json!({"role": "system", "content": "sys"}));
    assert_eq!(messages[1], json!({"role": "user", "content": "read them"}));
    let assistant = &messages[2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "Reading both.");
    assert_eq!(
        assistant["tool_calls"],
        json!([
            {"id": "call_a", "type": "function",
             "function": {"name": "read_file", "arguments": json!({"path": notes.display().to_string()}).to_string()}},
            {"id": "call_b", "type": "function",
             "function": {"name": "read_file", "arguments": json!({"path": shot.display().to_string()}).to_string()}}
        ])
    );
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_a");
    assert!(
        messages[3]["content"]
            .as_str()
            .unwrap()
            .contains("first line"),
        "{}",
        messages[3]
    );
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "call_b");
    // The picture follows in a user message: tool messages take text only.
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(png());
    assert_eq!(
        messages[5],
        json!({"role": "user", "content": [
            {"type": "text", "text": "The images of tool call call_b:"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{data}")}}
        ]})
    );
    assert_eq!(messages.len(), 6, "{second}");
}

#[tokio::test]
async fn the_binary_uses_an_openai_compatible_server_and_counts_its_usage() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let body = sse(&[
        delta(json!({"content": "Hi from the fake server."})),
        finish("stop"),
        usage(1000, 400, 100),
    ]);
    let (url, server) = fake_stream_api(vec![Served::Stream { body, chunk: 32 }]).await;
    let junees_home = home.path().join("junees");
    let mut command = common::junees_binary(&work, home.path());
    command
        .args(["-p", "hello"])
        .env("JUNEES_HOME", &junees_home)
        .env("JUNEES_PROVIDER", "openai")
        .env("OPENAI_BASE_URL", format!("{url}/v1"))
        .env("OPENAI_API_KEY", "sk-fake-openai")
        .env("JUNEES_MODEL", "test-model")
        .stdin(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout.trim(), "Hi from the fake server.", "{stderr}");

    let requests = served(server).await;
    let head = requests[0].to_lowercase();
    assert!(head.starts_with("post /v1/chat/completions"), "{head}");
    assert!(
        head.contains("authorization: bearer sk-fake-openai"),
        "{head}"
    );
    let body = body_of(&requests[0]);
    assert_eq!(body["model"], "test-model");
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "hello"})
    );
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(!body.to_string().contains("cache_control"), "{body}");

    // The session's usage, priced by the `prices` setting: 600 input,
    // 400 cached and 100 output tokens at $2, $1 and $8 per million.
    let mut usage = common::junees_binary(&work, home.path());
    usage
        .args(["usage", "--by", "model"])
        .env("JUNEES_HOME", &junees_home)
        .env("JUNEES_PRICES", "test-model=2/8/2.5/1")
        .stdin(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(60), usage.output())
        .await
        .expect("junees usage did not exit")
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let row = text
        .lines()
        .find(|line| line.starts_with("test-model"))
        .unwrap_or_else(|| panic!("no test-model row in\n{text}"));
    assert!(row.ends_with("$0.0024"), "{row}");
}
