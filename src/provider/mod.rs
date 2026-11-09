//! LLM providers. The agent loop only talks to the [`Provider`] trait, so
//! tests can swap in a scripted provider and new backends can be added later.

pub mod anthropic;
mod error;
mod stream;

pub use error::ProviderError;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

use crate::message::{ContentBlock, Message, StopReason, Usage};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// The next piece of a text block.
    TextDelta(String),
    /// The text block the last deltas belong to is complete.
    TextEnd,
    /// A `thinking` or `redacted_thinking` block started. Its content is
    /// not reported.
    Thinking,
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
    /// it arrives: every text and thinking block of the returned response
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
}

/// Reports the text and thinking blocks of a whole response, as a stream
/// would have. A receiver that is gone has nothing left to show.
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
            _ => {}
        }
    }
}
