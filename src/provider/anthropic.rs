//! Anthropic Messages API client.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, ToolSpec};
use crate::message::{ContentBlock, Message, StopReason, Usage};

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
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError> {
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
