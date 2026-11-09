//! Anthropic Messages API client.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, ToolSpec};
use crate::message::{ContentBlock, Role, StopReason, Usage};

const API_VERSION: &str = "2023-06-01";
const MAX_ATTEMPTS: u32 = 4;
/// Requests are not streamed, so this bounds one whole model call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RETRY_AFTER_SECS: u64 = 60;

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            api_key,
            base_url,
            model,
        })
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    async fn send_once(
        &self,
        url: &str,
        body: &ApiRequest<'_>,
    ) -> Result<CompletionResponse, Failure> {
        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body)
            .send()
            .await
            .map_err(|err| Failure {
                // is_request covers connections closed before a response.
                retryable: err.is_timeout() || err.is_connect() || err.is_request(),
                retry_after: None,
                error: ProviderError::Transport(Box::new(err)),
            })?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());
        // A body cut off midway is a transport problem worth retrying.
        let bytes = response.bytes().await.map_err(|err| Failure {
            retryable: true,
            retry_after: None,
            error: ProviderError::Transport(Box::new(err)),
        })?;

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
}

/// One failed attempt and whether another one may help.
struct Failure {
    error: ProviderError,
    retryable: bool,
    retry_after: Option<u64>,
}

/// A request body. Prompt-cache breakpoints go on the last tool, the system
/// prompt and the last block of the last message, so each call reads the
/// prefix the previous one wrote. They are added here, while serializing,
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
}

impl<'a> ApiRequest<'a> {
    fn new(model: &'a str, request: CompletionRequest<'a>) -> Self {
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
                let last_block = message.content.len().saturating_sub(1);
                ApiMessage {
                    role: message.role,
                    content: message
                        .content
                        .iter()
                        .enumerate()
                        .map(|(j, block)| {
                            Cached::new(
                                block,
                                request.cache && i == last_message && j == last_block,
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
        }
    }
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
            content: response
                .content
                .into_iter()
                .filter(|block| !matches!(block, ContentBlock::Unsupported))
                .collect(),
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
        let body = ApiRequest::new(&self.model, request);
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
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 || status.is_server_error()
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
            },
        );
        serde_json::to_value(&body).unwrap()
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
    fn response_drops_unsupported_blocks() {
        let api: ApiResponse = serde_json::from_value(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "thinking", "thinking": "...", "signature": "s"},
                {"type": "text", "text": "done"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 3, "cache_read_input_tokens": 0}
        }))
        .unwrap();
        let response = CompletionResponse::from(api);
        assert_eq!(
            response.content,
            vec![ContentBlock::Text {
                text: "done".into()
            }]
        );
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 12);
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
