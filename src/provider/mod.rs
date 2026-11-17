//! LLM providers. The agent loop only talks to the [`Provider`] trait, so
//! tests can swap in a scripted provider and new backends can be added later.

pub mod anthropic;
mod error;
pub mod fallback;
mod http;
pub mod openai;
mod stream;

pub use error::ProviderError;
pub use http::{AddressNames, check_base_url};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::message::{ContentBlock, Message, StopReason, Usage};

/// The setting that picks the API of the `api` backend (`provider` in
/// the settings file).
pub const PROVIDER_ENV: &str = "JUNEES_PROVIDER";

/// Which API the `api` backend talks to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    /// Anthropic's Messages API ([`anthropic`]).
    #[default]
    Anthropic,
    /// The OpenAI Chat Completions API and the servers that speak it
    /// ([`openai`]).
    OpenAi,
}

impl Kind {
    /// The value of [`PROVIDER_ENV`]; unset is Anthropic.
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("" | "anthropic") => Ok(Kind::Anthropic),
            Some("openai") => Ok(Kind::OpenAi),
            Some(_) => anyhow::bail!(
                "{PROVIDER_ENV} (`provider`) must be anthropic or openai, got `{}`",
                value.unwrap_or_default().trim()
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Anthropic => "anthropic",
            Kind::OpenAi => "openai",
        }
    }
}

/// The model client the settings ask for: the provider, its address and
/// key, the model, streaming, and the Anthropic-only extras (thinking
/// budget, web search) where they apply.
///
/// With a fallback model, the calls go to it while the main one is blocked
/// ([`fallback::Switching`]). A person sees what happens, so nothing waits
/// for a usage limit to lift here; see [`unattended_from_config`].
pub fn from_config(config: &crate::config::Config) -> anyhow::Result<Box<dyn Provider>> {
    build(config, None)
}

/// [`from_config`] for runs nobody watches (cron, webhooks, `mcp serve`,
/// tasks): when no model is left to switch to, a call waits for a usage
/// limit to lift, up to `wait_limit` and never past `deadline` (the run's
/// time limit).
pub fn unattended_from_config(
    config: &crate::config::Config,
    deadline: Option<tokio::time::Instant>,
) -> anyhow::Result<Box<dyn Provider>> {
    build(config, Some(deadline))
}

/// A provider factory for a run of several sessions (a task) that share one
/// time limit: the deadline is fixed once, when the factory is made, and
/// every provider `build` makes gets that same deadline. A deadline taken
/// per session would move on with each one, and a late session could wait
/// past the run's limit.
pub fn unattended_factory<F>(
    max_time: Option<std::time::Duration>,
    build: F,
) -> impl Fn() -> anyhow::Result<Box<dyn Provider>>
where
    F: Fn(Option<tokio::time::Instant>) -> anyhow::Result<Box<dyn Provider>>,
{
    let deadline = max_time.map(|max_time| tokio::time::Instant::now() + max_time);
    move || build(deadline)
}

fn build(
    config: &crate::config::Config,
    unattended: Option<Option<tokio::time::Instant>>,
) -> anyhow::Result<Box<dyn Provider>> {
    let wait = unattended
        .filter(|_| !config.wait_limit.is_zero())
        .map(|deadline| fallback::Wait {
            limit: config.wait_limit,
            deadline,
        });
    if config.fallback.is_none() && wait.is_none() {
        return main_client(config, None);
    }
    // A long `retry-after` is not sat out in the retries: the switch or
    // the planned wait takes over.
    let give_up = if config.fallback.is_some() {
        config.fallback_after
    } else {
        std::time::Duration::from_secs(http::MAX_RETRY_AFTER_SECS)
    };
    let fallback = config
        .fallback
        .as_ref()
        .map(|settings| {
            fallback::client_giving_up(
                settings,
                config,
                wait.map(|_| std::time::Duration::from_secs(http::MAX_RETRY_AFTER_SECS)),
            )
        })
        .transpose()?;
    Ok(Box::new(fallback::Switching::new(
        main_client(config, Some(give_up))?,
        fallback,
        config.fallback_after,
        wait,
    )))
}

fn main_client(
    config: &crate::config::Config,
    give_up: Option<std::time::Duration>,
) -> anyhow::Result<Box<dyn Provider>> {
    Ok(match &config.openai {
        None => {
            let client = anthropic::AnthropicProvider::new(
                config.api_key.clone(),
                config.base_url.clone(),
                config.model.clone(),
            )?
            .with_thinking(config.thinking_budget)
            .with_streaming(config.stream)
            .with_web_search(config.web_search.clone());
            match give_up {
                Some(give_up) => Box::new(client.with_give_up(give_up)),
                None => Box::new(client),
            }
        }
        Some(settings) => {
            let client = openai::OpenAiProvider::new(settings.clone(), config.model.clone())?
                .with_streaming(config.stream);
            match give_up {
                Some(give_up) => Box::new(client.with_give_up(give_up)),
                None => Box::new(client),
            }
        }
    })
}

/// A tool as advertised to the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct CompletionRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_tokens: u32,
    /// Ask the provider to cache the request's prefix for the next call.
    /// Off for one-off calls, whose prefix is never sent again.
    pub cache: bool,
    /// Let the model think before it answers, if the provider was set up
    /// with a thinking budget. Off for summary calls.
    pub thinking: bool,
    /// Offer the API's web search tool too, if the provider was set up
    /// with it. Off for summary calls.
    pub web_search: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Progress of a model call made with [`Provider::complete_streaming`],
/// reported while the response arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// The next piece of a text block.
    TextDelta(String),
    /// The text block the last deltas belong to is complete.
    TextEnd,
    /// A `thinking` or `redacted_thinking` block started. Its content is
    /// not reported.
    Thinking,
    /// The API ran one of its own tools (a `server_tool_use` block, such as
    /// a web search) with this input. Junees does not run it.
    ServerToolUse { name: String, input: Value },
    /// The calls went over to the fallback model ([`fallback::Switching`]).
    ModelSwitch(fallback::Switch),
    /// This model answered the call: sent last, by a provider with a
    /// fallback model ([`fallback::Switching`]), which alone knows which
    /// of its models it was. [`Provider::model`] says the same only while
    /// no other call runs on the same provider at once.
    AnsweredBy(String),
    /// Something the person should know about the calls, such as a wait
    /// for a usage limit to lift.
    Notice(String),
    /// The response broke off after part of it was reported. It is asked
    /// for again from the start, and what was reported since the last
    /// `Retrying` (or the start) is not part of the response.
    Retrying,
}

/// Where [`Provider::complete_streaming`] reports its progress. A channel
/// rather than a callback, so the call stays `Send` whatever the receiver
/// does with the events.
pub type StreamSender = tokio::sync::mpsc::UnboundedSender<StreamEvent>;

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError>;

    /// Like [`Provider::complete`], reporting the response to `events` as
    /// it arrives: every text, thinking and server tool block of the returned response
    /// is reported, in order, after the last [`StreamEvent::Retrying`].
    /// Providers that cannot stream report the whole response at the end,
    /// which is what this default does.
    async fn complete_streaming(
        &self,
        request: CompletionRequest<'_>,
        events: &StreamSender,
    ) -> Result<CompletionResponse, ProviderError> {
        let response = self.complete(request).await?;
        report_blocks(&response.content, events);
        Ok(response)
    }

    /// Whether requests that allow thinking ask for manual extended thinking
    /// (a fixed token budget). The API then wants the turn in progress to
    /// start with a thinking block, and only models that do not check what
    /// came before a returned thinking block (Claude 4.6 and earlier) accept
    /// it, so the history keeps its thinking blocks when it is shrunk.
    fn manual_thinking(&self) -> bool {
        false
    }

    /// A new turn (the person's or the run's next input) starts. A provider
    /// that switched to a fallback model goes back to the main one here
    /// once it may ([`fallback::Switching`]).
    fn begin_turn(&self) {}

    /// The model the calls go to, if the provider names one. Journals
    /// stamp it on usage entries, so costs follow a `/model` switch.
    fn model(&self) -> Option<&str> {
        None
    }
}

/// Reports the text, thinking and server tool blocks of a whole response,
/// as a stream would have. A receiver that is gone has nothing left to
/// show.
pub fn report_blocks(content: &[ContentBlock], events: &StreamSender) {
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => {
                let _ = events.send(StreamEvent::TextDelta(text.clone()));
                let _ = events.send(StreamEvent::TextEnd);
            }
            block if block.is_thinking() => {
                let _ = events.send(StreamEvent::Thinking);
            }
            ContentBlock::Other(value) if value["type"] == "server_tool_use" => {
                let _ = events.send(server_tool_use(value));
            }
            _ => {}
        }
    }
}

/// The event for a whole `server_tool_use` block.
pub(crate) fn server_tool_use(block: &Value) -> StreamEvent {
    StreamEvent::ServerToolUse {
        name: block["name"].as_str().unwrap_or_default().to_string(),
        input: block["input"].clone(),
    }
}
