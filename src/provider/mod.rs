//! LLM providers. The agent loop only talks to the [`Provider`] trait, so
//! tests can swap in a scripted provider and new backends can be added later.

pub mod anthropic;
mod error;

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
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> Result<CompletionResponse, ProviderError>;

    /// Whether requests that allow thinking ask for manual extended thinking
    /// (a fixed token budget). The API then wants the turn in progress to
    /// start with a thinking block, and only models that do not check what
    /// came before a returned thinking block (Claude 4.6 and earlier) accept
    /// it, so the history keeps its thinking blocks when it is shrunk.
    fn manual_thinking(&self) -> bool {
        false
    }
}
