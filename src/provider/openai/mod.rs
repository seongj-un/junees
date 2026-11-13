//! A client for the OpenAI Chat Completions API (`POST
//! {base}/chat/completions`) and the servers that speak it: OpenAI,
//! OpenRouter, Ollama (`/v1`), vLLM, LM Studio and the like.
//!
//! The conversation keeps its Messages API shape ([`crate::message`]);
//! [`convert`] turns it into Chat Completions messages and the answer back
//! into blocks, [`stream`] puts a streamed answer together. Anthropic's
//! own features (prompt-cache marks, extended thinking, the server web
//! search tool) have no counterpart here and are left out.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::StatusCode;

use super::http::{
    Failure, MAX_ATTEMPTS, REQUEST_TIMEOUT, STREAM_IDLE_TIMEOUT, backoff, body_failure,
    idle_failure, is_retryable, retry_after, send_failure,
};
use super::stream::SseParser;
use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, StreamSender};
use crate::mcp::http::target::is_loopback;
use convert::{ApiError, ChatRequest, ChatResponse, Options, stream_status};
use stream::Chunks;

mod convert;
mod stream;

/// The API's address when `openai_base_url` is not set.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// The setting for the API's address (`openai_base_url`).
pub const BASE_URL_ENV: &str = "OPENAI_BASE_URL";
/// The setting for the key (`openai_api_key`).
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";
/// The setting for the field that carries the output limit
/// (`openai_max_tokens_field`).
pub const MAX_TOKENS_FIELD_ENV: &str = "JUNEES_OPENAI_MAX_TOKENS_FIELD";
/// The setting for `reasoning_effort` (`openai_reasoning_effort`).
pub const REASONING_EFFORT_ENV: &str = "JUNEES_OPENAI_REASONING_EFFORT";

/// The provider's settings. Deliberately not `Debug`: it holds the key.
#[derive(Clone, PartialEq)]
pub struct Settings {
    /// Where the API is, without a trailing `/`: `https://api.openai.com/v1`,
    /// `http://localhost:11434/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    /// Which field carries the output limit.
    pub limit_field: LimitField,
    /// `reasoning_effort` for every call, sent only when set: `low`,
    /// `medium`, `high` and so on.
    pub reasoning_effort: Option<String>,
}

/// The request field for the output limit (`openai_max_tokens_field`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LimitField {
    /// `max_completion_tokens` for OpenAI's own API and Azure OpenAI,
    /// `max_tokens` elsewhere ([`Settings::uses_max_completion_tokens`]).
    #[default]
    Auto,
    MaxTokens,
    MaxCompletionTokens,
}

impl Settings {
    /// Settings for `base_url` and `api_key`, the rest left at their
    /// defaults. Nothing is checked.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            limit_field: LimitField::Auto,
            reasoning_effort: None,
        }
    }

    /// Reads `OPENAI_BASE_URL`, `OPENAI_API_KEY`,
    /// `JUNEES_OPENAI_MAX_TOKENS_FIELD` and `JUNEES_OPENAI_REASONING_EFFORT`
    /// from `get` (which trims and drops empty values) and checks the
    /// address: https, or http on a loopback address only, since the key
    /// and the whole conversation go there.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let base_url = get(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        check_base_url(&base_url)?;
        let limit_field = match get(MAX_TOKENS_FIELD_ENV).as_deref() {
            None | Some("auto") => LimitField::Auto,
            Some("max_tokens") => LimitField::MaxTokens,
            Some("max_completion_tokens") => LimitField::MaxCompletionTokens,
            Some(other) => bail!(
                "{MAX_TOKENS_FIELD_ENV} (`openai_max_tokens_field`) must be auto, max_tokens \
                 or max_completion_tokens, got `{other}`"
            ),
        };
        let reasoning_effort = get(REASONING_EFFORT_ENV).map(|effort| effort.to_ascii_lowercase());
        if let Some(effort) = &reasoning_effort {
            // The values differ by model and grow (none ... xhigh, max), so
            // only the form is checked; the server refuses one it lacks.
            let word = effort.len() <= 16 && effort.bytes().all(|byte| byte.is_ascii_lowercase());
            if !word {
                bail!(
                    "{REASONING_EFFORT_ENV} (`openai_reasoning_effort`) must be a reasoning \
                     effort such as low, medium or high, got `{effort}`"
                );
            }
        }
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: get(API_KEY_ENV),
            limit_field,
            reasoning_effort,
        })
    }

    /// Whether the server runs on this machine (a loopback address), where
    /// a server without a key (Ollama, LM Studio) is fine.
    pub fn is_local(&self) -> bool {
        reqwest::Url::parse(&self.base_url).is_ok_and(|url| is_loopback(&url))
    }

    /// The address's host name, lowercase.
    fn host(&self) -> Option<String> {
        let url = reqwest::Url::parse(&self.base_url).ok()?;
        url.host_str().map(str::to_ascii_lowercase)
    }

    /// Whether the address is OpenAI's own API: `api.openai.com`, or a
    /// regional domain such as `eu.api.openai.com` (data residency,
    /// developers.openai.com/api/docs/guides/your-data).
    pub fn is_official(&self) -> bool {
        self.host()
            .is_some_and(|host| host == "api.openai.com" || host.ends_with(".api.openai.com"))
    }

    /// Whether the address is Azure OpenAI's v1 API:
    /// `https://<resource>.openai.azure.com/openai/v1` or
    /// `https://<resource>.services.ai.azure.com/openai/v1` (Azure
    /// OpenAI's API version lifecycle page), or
    /// `https://<resource>.cognitiveservices.azure.com/openai/v1`, the
    /// endpoint the Azure portal shows for newer resources.
    pub fn is_azure(&self) -> bool {
        self.host().is_some_and(|host| {
            [
                ".openai.azure.com",
                ".services.ai.azure.com",
                ".cognitiveservices.azure.com",
            ]
            .iter()
            .any(|suffix| host.ends_with(suffix))
        })
    }

    /// Whether the output limit goes as `max_completion_tokens`. OpenAI
    /// deprecated `max_tokens` and its reasoning models refuse it (the
    /// Chat Completions reference); Azure OpenAI's reasoning models take
    /// only `max_completion_tokens` (its reasoning models page). Other
    /// servers (Ollama, vLLM, LM Studio, older proxies) know `max_tokens`.
    pub fn uses_max_completion_tokens(&self) -> bool {
        match self.limit_field {
            LimitField::Auto => self.is_official() || self.is_azure(),
            LimitField::MaxTokens => false,
            LimitField::MaxCompletionTokens => true,
        }
    }

    /// The key a request needs: an error when there is none and the
    /// server is not on this machine.
    pub fn require_key(&self) -> Result<()> {
        if self.api_key.is_none() && !self.is_local() {
            bail!(
                "no API key for {}: set openai_api_key or {API_KEY_ENV}, in the same place \
                 as the address (config.toml's openai_api_key goes with openai_base_url, \
                 {API_KEY_ENV} with {BASE_URL_ENV}; a server on this machine, such as \
                 Ollama on http://localhost:11434/v1, needs none)",
                self.base_url
            );
        }
        Ok(())
    }
}

/// Refuses an address that is not https, or http on loopback.
fn check_base_url(raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw.trim()).with_context(|| {
        // A URL with a user name or password in it is not shown.
        let shown = if raw.contains('@') {
            "(not shown)"
        } else {
            raw
        };
        format!("{BASE_URL_ENV} (`openai_base_url`) is not a URL: `{shown}`")
    })?;
    // The address shows up in errors and in `junees doctor`, and a
    // password in it would go along: the key goes in `openai_api_key`.
    if !url.username().is_empty() || url.password().is_some() {
        bail!(
            "{BASE_URL_ENV} (`openai_base_url`) must not hold a user name or password: put \
             the key in openai_api_key ({API_KEY_ENV})"
        );
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(&url) => Ok(()),
        "http" => bail!(
            "{BASE_URL_ENV} (`openai_base_url`) must use https unless the server is on this \
             machine (localhost, 127.0.0.1 or ::1): the key and the conversation would cross \
             the network in plain text"
        ),
        scheme => bail!(
            "{BASE_URL_ENV} (`openai_base_url`) must be an https:// (or local http://) URL, \
             not {scheme}:"
        ),
    }
}

pub struct OpenAiProvider {
    client: reqwest::Client,
    settings: Settings,
    model: String,
    /// Stream [`Provider::complete_streaming`] calls (`stream: true`).
    stream: bool,
    /// See [`STREAM_IDLE_TIMEOUT`].
    idle_timeout: Duration,
}

impl OpenAiProvider {
    pub fn new(settings: Settings, model: String) -> Result<Self> {
        // Timeouts are set per request: a stream has no overall one. No
        // redirects: one could lead the key to plain http.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            client,
            settings,
            model,
            stream: false,
            idle_timeout: STREAM_IDLE_TIMEOUT,
        })
    }

    /// Streams the calls made with [`Provider::complete_streaming`].
    pub fn with_streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Changes how long a stream may go without an event.
    pub fn with_idle_timeout(mut self, idle: Duration) -> Self {
        self.idle_timeout = idle;
        self
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.settings.base_url)
    }

    fn body<'a>(&'a self, request: CompletionRequest<'a>) -> ChatRequest<'a> {
        let options = Options {
            max_completion_tokens: self.settings.uses_max_completion_tokens(),
            reasoning_effort: self.settings.reasoning_effort.as_deref(),
        };
        ChatRequest::new(&self.model, request, options)
    }

    fn post(&self, url: &str, body: &ChatRequest<'_>) -> reqwest::RequestBuilder {
        let request = self.client.post(url).json(body);
        match &self.settings.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    async fn send_once(
        &self,
        url: &str,
        body: &ChatRequest<'_>,
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
            return Err(error_failure(status, retry_after, &bytes));
        }
        decode(&bytes)
    }

    /// One streamed attempt. Sets `reported` once it reported anything.
    async fn stream_once(
        &self,
        url: &str,
        body: &ChatRequest<'_>,
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
            // a server that does not stream answers whole.
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
                return Err(error_failure(status, retry_after, &bytes));
            }
            let response = decode(&bytes)?;
            super::report_blocks(&response.content, events);
            return Ok(response);
        }

        let mut parser = SseParser::default();
        let mut chunks = Chunks::default();
        let mut report = |event| {
            *reported = true;
            // A receiver that is gone has nothing left to show.
            let _ = events.send(event);
        };
        let mut deadline = tokio::time::Instant::now() + idle;
        loop {
            let chunk = tokio::time::timeout_at(deadline, response.chunk())
                .await
                .map_err(|_| idle_failure(idle))?
                .map_err(body_failure)?;
            let Some(chunk) = chunk else {
                // Some servers close the stream after the finish reason
                // without `[DONE]`; before it, the answer is cut off.
                if chunks.finished() {
                    return Ok(chunks.finish(&mut report));
                }
                return Err(Failure {
                    retryable: true,
                    retry_after: None,
                    error: ProviderError::Transport(
                        "the response stream ended before the answer did".into(),
                    ),
                });
            };
            let parsed = parser.push(&chunk).map_err(|err| Failure {
                retryable: false,
                retry_after: None,
                error: ProviderError::Decode(err),
            })?;
            for event in parsed {
                deadline = tokio::time::Instant::now() + idle;
                let done = chunks.apply(&event.data, &mut report)?;
                if done {
                    return Ok(chunks.finish(&mut report));
                }
            }
        }
    }
}

fn decode(bytes: &[u8]) -> Result<CompletionResponse, Failure> {
    let decode_failure = |error| Failure {
        retryable: false,
        retry_after: None,
        error,
    };
    let parsed: ChatResponse = serde_json::from_slice(bytes)
        .map_err(|err| decode_failure(ProviderError::Decode(err.to_string())))?;
    // OpenRouter answers 200 once the model started, and puts an error
    // that comes after that in the body: it carries its status in `code`.
    if let Some(error) = parsed.error() {
        return Err(api_failure(stream_status(&error), None, error));
    }
    parsed.into_response().map_err(decode_failure)
}

/// A failed status: retried when waiting may help, which a 429 for an
/// account out of credit does not.
fn error_failure(status: StatusCode, retry_after: Option<u64>, body: &[u8]) -> Failure {
    api_failure(status.as_u16(), retry_after, ApiError::parse(body))
}

/// An error the server described, with the status it has or names: in an
/// error response, in a 200 body or in a stream chunk alike.
fn api_failure(status: u16, retry_after: Option<u64>, error: ApiError) -> Failure {
    Failure {
        retryable: StatusCode::from_u16(status).is_ok_and(is_retryable) && !error.is_quota(),
        retry_after,
        error: error.into_error(status),
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError> {
        let body = self.body(request);
        let url = self.url();
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

    /// Streams the call when streaming is on, retrying a stream that breaks
    /// off from the start ([`super::StreamEvent::Retrying`] first, if part
    /// of it was reported).
    async fn complete_streaming(
        &self,
        request: CompletionRequest<'_>,
        events: &StreamSender,
    ) -> Result<CompletionResponse, ProviderError> {
        if !self.stream {
            let response = self.complete(request).await?;
            super::report_blocks(&response.content, events);
            return Ok(response);
        }
        let body = self.body(request).streamed();
        let url = self.url();
        let mut attempt = 1;
        loop {
            let mut reported = false;
            match self.stream_once(&url, &body, events, &mut reported).await {
                Ok(response) => return Ok(response),
                Err(failure) if failure.retryable && attempt < MAX_ATTEMPTS => {
                    if reported {
                        let _ = events.send(super::StreamEvent::Retrying);
                    }
                    tokio::time::sleep(backoff(attempt, failure.retry_after)).await;
                    attempt += 1;
                }
                Err(failure) => return Err(failure.error),
            }
        }
    }

    fn model(&self) -> Option<&str> {
        Some(&self.model)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<Settings> {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        Settings::read(|key| map.get(key).map(|value| value.to_string()))
    }

    #[test]
    fn reads_the_address_and_the_key() {
        let settings = read(&[]).unwrap();
        assert_eq!(settings.base_url, DEFAULT_BASE_URL);
        assert_eq!(settings.api_key, None);
        assert!(settings.is_official());
        assert!(!settings.is_local());
        let err = settings.require_key().unwrap_err().to_string();
        assert!(err.contains("OPENAI_API_KEY"), "{err}");

        let settings = read(&[
            (BASE_URL_ENV, "https://openrouter.ai/api/v1/"),
            (API_KEY_ENV, "sk-or-v1-fake"),
        ])
        .unwrap();
        assert_eq!(settings.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(settings.api_key.as_deref(), Some("sk-or-v1-fake"));
        assert!(!settings.is_official());
        settings.require_key().unwrap();
    }

    #[test]
    fn plain_http_only_on_this_machine_and_no_key_needed_there() {
        for local in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://127.8.0.1/v1",
            "http://[::1]:8000/v1",
        ] {
            let settings = read(&[(BASE_URL_ENV, local)]).unwrap();
            assert!(settings.is_local(), "{local}");
            settings.require_key().unwrap();
        }
        for remote in [
            "http://192.168.0.10:11434/v1",
            "http://example.com/v1",
            "http://localhost.example.com/v1",
        ] {
            let err = read(&[(BASE_URL_ENV, remote)]).err().unwrap().to_string();
            assert!(err.contains("must use https"), "{remote}: {err}");
        }
        for bad in ["ftp://localhost/v1", "localhost:11434"] {
            assert!(read(&[(BASE_URL_ENV, bad)]).is_err(), "{bad}");
        }
        // A user name or password in the address is refused, and the
        // error does not repeat it.
        for with_user in [
            "https://user:hunter2@proxy.example/v1",
            "https://hunter2@proxy.example/v1",
            "https://:hunter2@proxy.example/v1",
            "http://user:hunter2@localhost:11434/v1",
            "https://user:hunter2@exa mple/v1",
        ] {
            let err = read(&[(BASE_URL_ENV, with_user)]).err().unwrap();
            let err = format!("{err:#}");
            assert!(!err.contains("hunter2"), "{with_user}: {err}");
        }
        // A remote https server without a key is refused only when used.
        let remote = read(&[(BASE_URL_ENV, "https://llm.example.com/v1")]).unwrap();
        assert!(remote.require_key().is_err());
    }

    #[test]
    fn the_url_and_the_output_limit_follow_the_address() {
        let provider = |base: &str| {
            OpenAiProvider::new(read(&[(BASE_URL_ENV, base)]).unwrap(), "m".into()).unwrap()
        };
        let request = CompletionRequest {
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 9,
            cache: false,
            thinking: false,
            web_search: false,
        };
        let official = provider("https://api.openai.com/v1/");
        assert_eq!(official.url(), "https://api.openai.com/v1/chat/completions");
        let body = serde_json::to_value(official.body(request)).unwrap();
        assert_eq!(body["max_completion_tokens"], 9);
        let local = provider("http://localhost:11434/v1");
        assert_eq!(local.url(), "http://localhost:11434/v1/chat/completions");
        let body = serde_json::to_value(local.body(request)).unwrap();
        assert_eq!(body["max_tokens"], 9);
        assert_eq!(local.model(), Some("m"));
        assert!(!local.manual_thinking());
    }

    #[test]
    fn openai_and_azure_hosts_get_max_completion_tokens() {
        let field = |pairs: &[(&str, &str)]| read(pairs).unwrap().uses_max_completion_tokens();
        for completion in [
            "https://api.openai.com/v1",
            "https://eu.api.openai.com/v1",
            "https://US.API.OPENAI.COM/v1",
            "https://my-res.openai.azure.com/openai/v1/",
            "https://my-res.services.ai.azure.com/openai/v1",
            "https://my-res.cognitiveservices.azure.com/openai/v1",
        ] {
            assert!(field(&[(BASE_URL_ENV, completion)]), "{completion}");
        }
        for plain in [
            "https://openrouter.ai/api/v1",
            "http://localhost:11434/v1",
            "https://api.openai.com.example/v1",
            "https://notapi.openai.com/v1",
            "https://openai.azure.com.example/v1",
            "https://cognitiveservices.azure.com.example/v1",
        ] {
            assert!(!field(&[(BASE_URL_ENV, plain)]), "{plain}");
        }
        assert!(
            read(&[(BASE_URL_ENV, "https://eu.api.openai.com/v1")])
                .unwrap()
                .is_official()
        );
        assert!(
            !read(&[(BASE_URL_ENV, "https://r.openai.azure.com/openai/v1")])
                .unwrap()
                .is_official()
        );
        // The setting decides when it is not auto.
        let local = (BASE_URL_ENV, "http://localhost:8000/v1");
        assert!(field(&[
            local,
            (MAX_TOKENS_FIELD_ENV, "max_completion_tokens")
        ]));
        assert!(!field(&[(MAX_TOKENS_FIELD_ENV, "max_tokens")]));
        assert!(field(&[(MAX_TOKENS_FIELD_ENV, "auto")]));
        let err = read(&[(MAX_TOKENS_FIELD_ENV, "max_output_tokens")])
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("openai_max_tokens_field"), "{err}");
    }

    #[test]
    fn a_reasoning_effort_is_read_and_sent_only_when_set() {
        assert_eq!(read(&[]).unwrap().reasoning_effort, None);
        let settings = read(&[(REASONING_EFFORT_ENV, "High")]).unwrap();
        assert_eq!(settings.reasoning_effort.as_deref(), Some("high"));
        for bad in ["very high", "low;", "1"] {
            let err = read(&[(REASONING_EFFORT_ENV, bad)])
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains("openai_reasoning_effort"), "{bad}: {err}");
        }
        let request = CompletionRequest {
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 9,
            cache: false,
            thinking: true,
            web_search: false,
        };
        let provider = OpenAiProvider::new(settings, "o".into()).unwrap();
        let body = serde_json::to_value(provider.body(request)).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["max_completion_tokens"], 9);
    }
}
