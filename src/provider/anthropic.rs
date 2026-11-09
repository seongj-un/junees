//! Anthropic Messages API client.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::{CompletionRequest, CompletionResponse, Provider, ToolSpec};
use crate::message::{ContentBlock, Message, StopReason, Usage};

const API_VERSION: &str = "2023-06-01";
const MAX_ATTEMPTS: u32 = 4;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_RETRY_AFTER_SECS: u64 = 60;

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Result<Self> {
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
}

#[derive(Serialize)]
struct ApiRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "str::is_empty")]
    system: &'a str,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tools: &'a [ToolSpec],
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
    async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse> {
        let body = ApiRequest {
            model: &self.model,
            max_tokens: request.max_tokens,
            system: request.system,
            messages: request.messages,
            tools: request.tools,
        };
        let url = self.messages_url();

        let mut attempt = 1;
        loop {
            let result = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body)
                .send()
                .await;

            match result {
                Ok(response) if response.status().is_success() => {
                    let parsed: ApiResponse = response
                        .json()
                        .await
                        .context("failed to parse Anthropic API response")?;
                    return Ok(parsed.into());
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.trim().parse::<u64>().ok());
                    let text = response.text().await.unwrap_or_default();
                    if is_retryable(status) && attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(backoff(attempt, retry_after)).await;
                        attempt += 1;
                        continue;
                    }
                    bail!("{}", describe_error(status, &text));
                }
                Err(err) if (err.is_timeout() || err.is_connect()) && attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(backoff(attempt, None)).await;
                    attempt += 1;
                }
                Err(err) => return Err(err).context("request to Anthropic API failed"),
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

fn describe_error(status: StatusCode, body: &str) -> String {
    match serde_json::from_str::<ApiErrorBody>(body) {
        Ok(parsed) => format!(
            "Anthropic API error {status} ({}): {}",
            parsed.error.kind, parsed.error.message
        ),
        Err(_) if body.trim().is_empty() => format!("Anthropic API error {status}"),
        Err(_) => format!("Anthropic API error {status}: {}", body.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_omits_empty_system_and_tools() {
        let messages = [Message::user_text("hi")];
        let body = ApiRequest {
            model: "m",
            max_tokens: 10,
            system: "",
            messages: &messages,
            tools: &[],
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            json!({
                "model": "m",
                "max_tokens": 10,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
            })
        );
    }

    #[test]
    fn request_includes_system_and_tools() {
        let tools = [ToolSpec {
            name: "bash".into(),
            description: "run".into(),
            input_schema: json!({"type": "object"}),
        }];
        let body = ApiRequest {
            model: "m",
            max_tokens: 10,
            system: "be nice",
            messages: &[],
            tools: &tools,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["system"], "be nice");
        assert_eq!(value["tools"][0]["name"], "bash");
        assert_eq!(value["tools"][0]["input_schema"]["type"], "object");
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
    fn describe_error_uses_api_message() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            describe_error(StatusCode::UNAUTHORIZED, body),
            "Anthropic API error 401 Unauthorized (authentication_error): invalid x-api-key"
        );
        assert_eq!(
            describe_error(StatusCode::BAD_GATEWAY, "upstream down"),
            "Anthropic API error 502 Bad Gateway: upstream down"
        );
        assert_eq!(
            describe_error(StatusCode::BAD_GATEWAY, ""),
            "Anthropic API error 502 Bad Gateway"
        );
    }

    #[test]
    fn messages_url_handles_trailing_slash() {
        let provider =
            AnthropicProvider::new("k".into(), "https://example.com/".into(), "m".into()).unwrap();
        assert_eq!(provider.messages_url(), "https://example.com/v1/messages");
    }
}
