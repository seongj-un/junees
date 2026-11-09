//! Anthropic Messages API client.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::stream::{Assembler, SseParser};
use super::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, StreamEvent, StreamSender,
    ToolSpec, report_blocks,
};
use crate::message::{ContentBlock, Role, StopReason, Usage};

const API_VERSION: &str = "2023-06-01";
const MAX_ATTEMPTS: u32 = 4;
/// Bounds one whole model call that is not streamed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a streamed call may wait for its response to start and then
/// for each next event. A stream has no limit on its whole length: the
/// docs advise streaming for long requests, and a long answer goes on as
/// long as events keep coming. The API sends tool input one whole value
/// at a time, so a large file to write can mean a long pause; at the old
/// whole-call limit, no call that used to finish can time out here.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RETRY_AFTER_SECS: u64 = 60;

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    /// Extended thinking budget in tokens; 0 leaves thinking off.
    thinking_budget: u32,
    /// Stream [`Provider::complete_streaming`] calls (`stream: true`).
    stream: bool,
    /// See [`STREAM_IDLE_TIMEOUT`].
    idle_timeout: Duration,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> anyhow::Result<Self> {
        // Timeouts are set per request: a stream has no overall one.
        let client = reqwest::Client::builder()
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            api_key,
            base_url,
            model,
            thinking_budget: 0,
            stream: false,
            idle_timeout: STREAM_IDLE_TIMEOUT,
        })
    }

    /// Streams the calls made with [`Provider::complete_streaming`], so
    /// text shows as it is written; off, they are plain requests whose
    /// response is reported at the end. Other calls are never streamed.
    pub fn with_streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Changes how long a stream may go without an event
    /// ([`STREAM_IDLE_TIMEOUT`] by default).
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle_timeout = idle;
        self
    }

    /// Turns on extended thinking with `budget` tokens for the requests
    /// that allow it; 0 leaves it off. The budget must be at least 1024
    /// and below the `max_tokens` of those requests (the API's rules,
    /// checked by [`crate::config::Config`]).
    pub fn with_thinking(mut self, budget: u32) -> Self {
        self.thinking_budget = budget;
        self
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    fn post(&self, url: &str, body: &ApiRequest<'_>) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body)
    }

    async fn send_once(
        &self,
        url: &str,
        body: &ApiRequest<'_>,
    ) -> Result<CompletionResponse, Failure> {
        let response = self
            .post(url, body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(send_failure)?;

        let status = response.status();
        let retry_after = retry_after(&response);
        let bytes = response.bytes().await.map_err(body_failure)?;

        if !status.is_success() {
            return Err(Failure {
                retryable: is_retryable(status),
                retry_after,
                error: api_error(status, &bytes),
            });
        }
        let parsed: ApiResponse = serde_json::from_slice(&bytes).map_err(|err| Failure {
            retryable: false,
            retry_after: None,
            error: ProviderError::Decode(err.to_string()),
        })?;
        Ok(parsed.into())
    }

    /// One streamed attempt. Sets `reported` once it reported anything,
    /// so a retry can say that what was shown is void.
    async fn stream_once(
        &self,
        url: &str,
        body: &ApiRequest<'_>,
        events: &StreamSender,
        reported: &mut bool,
    ) -> Result<CompletionResponse, Failure> {
        let idle = self.idle_timeout;
        let mut response = tokio::time::timeout(idle, self.post(url, body).send())
            .await
            .map_err(|_| idle_failure(idle))?
            .map_err(send_failure)?;

        let status = response.status();
        let is_stream = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim_start().starts_with("text/event-stream"));
        if !status.is_success() || !is_stream {
            // Errors before the stream starts come as plain responses, and
            // a server that does not stream answers with the whole message.
            let retry_after = retry_after(&response);
            let mut bytes = Vec::new();
            while let Some(chunk) = tokio::time::timeout(idle, response.chunk())
                .await
                .map_err(|_| idle_failure(idle))?
                .map_err(body_failure)?
            {
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                return Err(Failure {
                    retryable: is_retryable(status),
                    retry_after,
                    error: api_error(status, &bytes),
                });
            }
            let parsed: ApiResponse = serde_json::from_slice(&bytes).map_err(|err| Failure {
                retryable: false,
                retry_after: None,
                error: ProviderError::Decode(err.to_string()),
            })?;
            let response = CompletionResponse::from(parsed);
            report_blocks(&response.content, events);
            return Ok(response);
        }

        let mut parser = SseParser::default();
        let mut assembler = Assembler::default();
        let mut report = |event: StreamEvent| {
            *reported = true;
            // A receiver that is gone has nothing left to show.
            let _ = events.send(event);
        };
        // Waiting is bounded per event, not per chunk of bytes.
        let mut deadline = tokio::time::Instant::now() + idle;
        loop {
            let chunk = tokio::time::timeout_at(deadline, response.chunk())
                .await
                .map_err(|_| idle_failure(idle))?
                .map_err(body_failure)?
                .ok_or_else(|| Failure {
                    retryable: true,
                    retry_after: None,
                    error: ProviderError::Transport(
                        "the response stream ended before message_stop".into(),
                    ),
                })?;
            let parsed = parser.push(&chunk).map_err(|err| Failure {
                retryable: false,
                retry_after: None,
                error: ProviderError::Decode(err),
            })?;
            for event in parsed {
                deadline = tokio::time::Instant::now() + idle;
                let done = assembler
                    .apply(&event.data, &mut report)
                    .map_err(|error| Failure {
                        retryable: is_retryable_error(&error),
                        retry_after: None,
                        error,
                    })?;
                if done {
                    return assembler.finish().map_err(|error| Failure {
                        retryable: false,
                        retry_after: None,
                        error,
                    });
                }
            }
        }
    }
}

/// One failed attempt and whether another one may help.
struct Failure {
    error: ProviderError,
    retryable: bool,
    retry_after: Option<u64>,
}

fn send_failure(err: reqwest::Error) -> Failure {
    Failure {
        // is_request covers connections closed before a response.
        retryable: err.is_timeout() || err.is_connect() || err.is_request(),
        retry_after: None,
        error: ProviderError::Transport(Box::new(err)),
    }
}

/// A body cut off midway is a transport problem worth retrying.
fn body_failure(err: reqwest::Error) -> Failure {
    Failure {
        retryable: true,
        retry_after: None,
        error: ProviderError::Transport(Box::new(err)),
    }
}

fn idle_failure(idle: Duration) -> Failure {
    Failure {
        retryable: true,
        retry_after: None,
        error: ProviderError::Transport(
            format!("no response from the API for {} s", idle.as_secs_f32()).into(),
        ),
    }
}

fn retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

/// A request body. Prompt-cache breakpoints go on the last tool, the system
/// prompt and the last block of the last message, so each call reads the
/// prefix the previous one wrote. Blocks Junees keeps raw (thinking and the
/// like) cannot carry a breakpoint, so it goes on the last block before
/// them. They are added here, while serializing,
/// so the conversation types stay plain, and only when the request asks
/// for caching: a cache write costs more than plain input.
#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    system: Vec<Cached<SystemText<'a>>>,
    messages: Vec<ApiMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: Vec<Cached<&'a ToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl<'a> ApiRequest<'a> {
    fn new(model: &'a str, request: CompletionRequest<'a>, thinking_budget: u32) -> Self {
        let system = if request.system.is_empty() {
            Vec::new()
        } else {
            vec![Cached::new(
                SystemText {
                    kind: "text",
                    text: request.system,
                },
                request.cache,
            )]
        };
        let last_tool = request.tools.len().saturating_sub(1);
        let tools = request
            .tools
            .iter()
            .enumerate()
            .map(|(i, tool)| Cached::new(tool, request.cache && i == last_tool))
            .collect();
        let last_message = request.messages.len().saturating_sub(1);
        let messages = request
            .messages
            .iter()
            .enumerate()
            .map(|(i, message)| {
                let last_block = message
                    .content
                    .iter()
                    .rposition(|block| !matches!(block, ContentBlock::Other(_)));
                ApiMessage {
                    role: message.role,
                    content: message
                        .content
                        .iter()
                        .enumerate()
                        .map(|(j, block)| {
                            Cached::new(
                                block,
                                request.cache && i == last_message && last_block == Some(j),
                            )
                        })
                        .collect(),
                }
            })
            .collect();
        Self {
            model,
            max_tokens: request.max_tokens,
            system,
            messages,
            tools,
            thinking: (request.thinking && thinking_budget > 0).then_some(Thinking {
                kind: "enabled",
                budget_tokens: thinking_budget,
            }),
            stream: false,
        }
    }
}

/// `thinking: {type: "enabled", budget_tokens}` of a request.
#[derive(Serialize)]
struct Thinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

#[derive(Serialize)]
struct SystemText<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: Role,
    content: Vec<Cached<&'a ContentBlock>>,
}

/// An item with an optional prompt-cache breakpoint after it.
#[derive(Serialize)]
struct Cached<T> {
    #[serde(flatten)]
    item: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl<T> Cached<T> {
    fn new(item: T, marked: bool) -> Self {
        Self {
            item,
            cache_control: marked.then_some(CacheControl { kind: "ephemeral" }),
        }
    }
}

#[derive(Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<StopReason>,
    #[serde(default)]
    usage: Usage,
}

impl From<ApiResponse> for CompletionResponse {
    fn from(response: ApiResponse) -> Self {
        Self {
            content: response.content,
            stop_reason: response.stop_reason.unwrap_or(StopReason::Other),
            usage: response.usage,
        }
    }
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError> {
        let body = ApiRequest::new(&self.model, request, self.thinking_budget);
        let url = self.messages_url();

        let mut attempt = 1;
        loop {
            match self.send_once(&url, &body).await {
                Ok(response) => return Ok(response),
                Err(failure) if failure.retryable && attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(backoff(attempt, failure.retry_after)).await;
                    attempt += 1;
                }
                Err(failure) => return Err(failure.error),
            }
        }
    }

    /// Streams the call when streaming is on. A stream that breaks off (a
    /// dropped connection, an `error` event such as `overloaded_error`, no
    /// event for too long) is retried like a failed request, from the
    /// start; if part of it was reported, [`StreamEvent::Retrying`] says
    /// so first. Dropping the future closes the connection at once.
    async fn complete_streaming(
        &self,
        request: CompletionRequest<'_>,
        events: &StreamSender,
    ) -> Result<CompletionResponse, ProviderError> {
        if !self.stream {
            let response = self.complete(request).await?;
            report_blocks(&response.content, events);
            return Ok(response);
        }
        let mut body = ApiRequest::new(&self.model, request, self.thinking_budget);
        body.stream = true;
        let url = self.messages_url();

        let mut attempt = 1;
        loop {
            let mut reported = false;
            match self.stream_once(&url, &body, events, &mut reported).await {
                Ok(response) => return Ok(response),
                Err(failure) if failure.retryable && attempt < MAX_ATTEMPTS => {
                    if reported {
                        let _ = events.send(StreamEvent::Retrying);
                    }
                    tokio::time::sleep(backoff(attempt, failure.retry_after)).await;
                    attempt += 1;
                }
                Err(failure) => return Err(failure.error),
            }
        }
    }

    fn manual_thinking(&self) -> bool {
        self.thinking_budget > 0
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 || status.is_server_error()
}

/// For an `error` event in a stream, which carries the status its type has
/// in a plain response.
fn is_retryable_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Api { status, .. } => StatusCode::from_u16(*status).is_ok_and(is_retryable),
        ProviderError::Transport(_) => true,
        ProviderError::Decode(_) => false,
    }
}

/// Wait before retry number `attempt` (1-based). Honors `retry-after` when the
/// server sends one, otherwise backs off exponentially from 500ms.
fn backoff(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs {
        Some(secs) => Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)),
        None => Duration::from_millis(500 * 2u64.pow(attempt.saturating_sub(1).min(6))),
    }
}

fn api_error(status: StatusCode, body: &[u8]) -> ProviderError {
    match serde_json::from_slice::<ApiErrorBody>(body) {
        Ok(parsed) => ProviderError::Api {
            status: status.as_u16(),
            kind: Some(parsed.error.kind),
            message: parsed.error.message,
        },
        Err(_) => ProviderError::Api {
            status: status.as_u16(),
            kind: None,
            message: String::from_utf8_lossy(body).trim().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;
    use serde_json::{Value, json};

    fn request(system: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
        request_with(system, messages, tools, true)
    }

    fn request_with(system: &str, messages: &[Message], tools: &[ToolSpec], cache: bool) -> Value {
        let body = ApiRequest::new(
            "m",
            CompletionRequest {
                system,
                messages,
                tools,
                max_tokens: 10,
                cache,
                thinking: true,
            },
            0,
        );
        serde_json::to_value(&body).unwrap()
    }

    #[test]
    fn thinking_is_asked_for_only_with_a_budget_and_when_allowed() {
        let messages = [Message::user_text("hi")];
        let body = |thinking, budget| {
            let request = CompletionRequest {
                system: "",
                messages: &messages,
                tools: &[],
                max_tokens: 4096,
                cache: false,
                thinking,
            };
            serde_json::to_value(ApiRequest::new("m", request, budget)).unwrap()
        };
        assert_eq!(
            body(true, 2048)["thinking"],
            json!({"type": "enabled", "budget_tokens": 2048})
        );
        for (thinking, budget) in [(false, 2048), (true, 0), (false, 0)] {
            let value = body(thinking, budget);
            assert!(value.get("thinking").is_none(), "{value}");
        }
        let provider = |budget| {
            AnthropicProvider::new("k".into(), "http://localhost".into(), "m".into())
                .unwrap()
                .with_thinking(budget)
        };
        assert!(provider(2048).manual_thinking());
        assert!(!provider(0).manual_thinking());
    }

    #[test]
    fn a_request_without_caching_has_no_cache_marks() {
        let tools = [ToolSpec {
            name: "bash".into(),
            description: "runs".into(),
            input_schema: json!({"type": "object"}),
        }];
        let messages = [Message::user_text("summarize this")];
        let value = request_with("be brief", &messages, &tools, false);
        assert!(!value.to_string().contains("cache_control"), "{value}");
        assert_eq!(
            value["system"],
            json!([{"type": "text", "text": "be brief"}])
        );
        assert_eq!(
            value["messages"][0]["content"],
            json!([{"type": "text", "text": "summarize this"}])
        );
    }

    #[test]
    fn request_omits_empty_system_and_tools() {
        let messages = [Message::user_text("hi")];
        assert_eq!(
            request("", &messages, &[]),
            json!({
                "model": "m",
                "max_tokens": 10,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
                ]}]
            })
        );
        // No messages at all still serializes.
        assert_eq!(request("", &[], &[])["messages"], json!([]));
    }

    #[test]
    fn request_marks_tools_system_and_the_last_block_for_caching() {
        let spec = |name: &str| ToolSpec {
            name: name.into(),
            description: "run".into(),
            input_schema: json!({"type": "object"}),
        };
        let tools = [spec("bash"), spec("read_file")];
        let messages = [
            Message::user_text("list files"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "sure".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        content: "a\nb".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "and then?".into(),
                    },
                ],
            },
        ];
        let ephemeral = json!({"type": "ephemeral"});
        let value = request("be nice", &messages, &tools);

        assert_eq!(
            value["system"],
            json!([{"type": "text", "text": "be nice", "cache_control": ephemeral}])
        );
        assert_eq!(
            value["tools"],
            json!([
                {"name": "bash", "description": "run", "input_schema": {"type": "object"}},
                {"name": "read_file", "description": "run", "input_schema": {"type": "object"},
                 "cache_control": ephemeral}
            ])
        );
        assert_eq!(
            value["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "list files"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "sure"},
                    {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb"},
                    {"type": "text", "text": "and then?", "cache_control": ephemeral}
                ]}
            ])
        );
        let marks = value.to_string().matches("cache_control").count();
        assert_eq!(marks, 3);
    }

    #[test]
    fn response_reads_cache_usage() {
        let api: ApiResponse = serde_json::from_value(json!({
            "content": [],
            "usage": {"input_tokens": 4, "output_tokens": 2,
                      "cache_creation_input_tokens": 100, "cache_read_input_tokens": 900}
        }))
        .unwrap();
        let usage = CompletionResponse::from(api).usage;
        assert_eq!(usage.cache_creation_input_tokens, 100);
        assert_eq!(usage.cache_read_input_tokens, 900);
    }

    #[test]
    fn response_keeps_thinking_blocks_as_they_came() {
        let thinking = json!({"type": "thinking", "thinking": "...", "signature": "s"});
        let api: ApiResponse = serde_json::from_value(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [thinking, {"type": "text", "text": "done"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 3, "cache_read_input_tokens": 0}
        }))
        .unwrap();
        let response = CompletionResponse::from(api);
        assert_eq!(
            response.content,
            vec![
                ContentBlock::Other(thinking),
                ContentBlock::Text {
                    text: "done".into()
                }
            ]
        );
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 12);
    }

    #[test]
    fn raw_blocks_go_back_unchanged_and_without_cache_marks() {
        let thinking = json!({"type": "thinking", "thinking": "plan", "signature": "sig=="});
        let redacted = json!({"type": "redacted_thinking", "data": "opaque"});
        let messages = [
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Other(thinking.clone()),
                    ContentBlock::Other(redacted.clone()),
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "bash".into(),
                        input: json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
        ];
        let value = request("", &messages, &[]);
        assert_eq!(value["messages"][1]["content"][0], thinking);
        assert_eq!(value["messages"][1]["content"][1], redacted);
        assert_eq!(
            value["messages"][2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        // A last message that ends in raw blocks is marked before them, and
        // one with nothing else is not marked at all.
        let mut ends_raw = messages[1].clone();
        ends_raw.content.rotate_left(2);
        let value = request("", &[ends_raw], &[]);
        let content = &value["messages"][0]["content"];
        assert_eq!(content[0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(content[1], thinking);
        assert_eq!(content[2], redacted);
        let only_raw = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Other(thinking.clone())],
        };
        let value = request("", &[only_raw], &[]);
        assert!(!value.to_string().contains("cache_control"), "{value}");
    }

    #[test]
    fn missing_stop_reason_maps_to_other() {
        let api: ApiResponse = serde_json::from_value(json!({"content": []})).unwrap();
        assert_eq!(CompletionResponse::from(api).stop_reason, StopReason::Other);
    }

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::from_u16(529).unwrap()));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn backoff_grows_and_respects_retry_after() {
        assert_eq!(backoff(1, None), Duration::from_millis(500));
        assert_eq!(backoff(2, None), Duration::from_millis(1000));
        assert_eq!(backoff(3, None), Duration::from_millis(2000));
        assert_eq!(backoff(1, Some(7)), Duration::from_secs(7));
        assert_eq!(
            backoff(1, Some(3600)),
            Duration::from_secs(MAX_RETRY_AFTER_SECS)
        );
    }

    #[test]
    fn api_error_uses_the_error_body() {
        let body = br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            api_error(StatusCode::UNAUTHORIZED, body).to_string(),
            "API error 401 (authentication_error): invalid x-api-key"
        );
        assert_eq!(
            api_error(StatusCode::BAD_GATEWAY, b"upstream down").to_string(),
            "API error 502: upstream down"
        );
        assert_eq!(
            api_error(StatusCode::BAD_GATEWAY, b"").to_string(),
            "API error 502"
        );
    }

    #[test]
    fn messages_url_handles_trailing_slash() {
        let provider =
            AnthropicProvider::new("k".into(), "https://example.com/".into(), "m".into()).unwrap();
        assert_eq!(provider.messages_url(), "https://example.com/v1/messages");
    }
}
