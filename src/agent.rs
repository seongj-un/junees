//! The agent loop: ask the model, run the tools it calls, feed the results
//! back, and repeat until it answers without calling a tool.

use anyhow::Result;
use serde_json::Value;

use crate::approval::ToolGate;
use crate::cancel::Cancel;
use crate::context;
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::{CompletionRequest, CompletionResponse, Provider, ProviderError, ToolSpec};
use crate::session::{Entry, Journal, Transcript};
use crate::tools::{ToolOutput, ToolRegistry};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub system: String,
    pub max_tokens: u32,
    /// Model calls allowed per user input before the loop gives up.
    pub max_turns: usize,
    /// The model's context window in tokens.
    pub context_window: u64,
    /// Share of the window at which the history is shrunk, in (0, 1].
    pub compact_threshold: f64,
}

impl AgentOptions {
    /// Context size in tokens from which the history is shrunk.
    pub fn compact_limit(&self) -> u64 {
        (self.context_window as f64 * self.compact_threshold) as u64
    }
}

/// Progress reported while [`Agent::run`] works.
#[derive(Debug)]
pub enum Event<'a> {
    Text(&'a str),
    /// The model thought before going on (a `thinking` or
    /// `redacted_thinking` block). The block is kept and sent back, but its
    /// content is not shown.
    Thinking,
    /// Something the user should know about, such as a cut-off response.
    Warning(&'a str),
    ToolCall {
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        name: &'a str,
        output: &'a ToolOutput,
    },
    /// The earlier part of the conversation was replaced by a summary.
    Compacted {
        before_messages: usize,
        after_messages: usize,
    },
}

/// The error of [`Agent::run`] when the model was still calling tools after
/// `max_turns` model calls. The history is valid and ends with the tool
/// results, so callers can tell it from a failure (`downcast_ref`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnLimit(pub usize);

impl std::fmt::Display for TurnLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stopped after {} model calls without a final answer",
            self.0
        )
    }
}

impl std::error::Error for TurnLimit {}

/// The result of a tool call that was cancelled while it ran, or before it
/// could start.
pub const CANCELLED: &str = "cancelled by the user";

/// The error of [`Agent::run`] when its [`Cancel`] was set. Running tools
/// were stopped and every tool call of the turn has a result, so the
/// history is valid and ends with a user message; the next input joins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Asked before each model call with the tokens used so far; a reason to
/// stop ends the run there. See [`Agent::with_limit`].
pub type Limit = Box<dyn FnMut(Usage) -> Option<String> + Send>;

/// The error of [`Agent::run`] when its [`Limit`] stopped it before a
/// model call, with the limit's reason. Like [`TurnLimit`], the history is
/// valid and ends with a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitReached(pub String);

impl std::fmt::Display for LimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LimitReached {}

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    options: AgentOptions,
    transcript: Transcript,
    /// Where history changes are recorded. `None` records nothing.
    journal: Option<Box<dyn Journal>>,
    /// Old tool output was cleared before the last model call, so the
    /// context size measured since tells whether that was enough.
    cleared: bool,
    /// Stops the current turn when set; see [`Agent::with_cancel`].
    cancel: Cancel,
    /// Checked before each model call; see [`Agent::with_limit`].
    limit: Option<Limit>,
    /// Asked before each tool call; see [`Agent::with_gate`].
    gate: Option<Box<dyn ToolGate>>,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>, tools: ToolRegistry, options: AgentOptions) -> Self {
        Self::restore(provider, tools, options, Transcript::default())
    }

    /// An agent that continues an earlier conversation.
    pub fn restore(
        provider: Box<dyn Provider>,
        tools: ToolRegistry,
        options: AgentOptions,
        transcript: Transcript,
    ) -> Self {
        Self {
            provider,
            tools,
            options,
            transcript,
            journal: None,
            cleared: false,
            cancel: Cancel::new(),
            limit: None,
            gate: None,
        }
    }

    /// Before each model call, asks `limit` with this session's usage so
    /// far (summary calls included) whether to go on. When it gives a
    /// reason, [`Agent::run`] fails with [`LimitReached`] instead of
    /// calling the model. A summary call that compaction makes before a
    /// model call is asked about on its own. Task runs use it for their token and time budget.
    pub fn with_limit(mut self, limit: Limit) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Stops [`Agent::run`] when `cancel` is set: a model call in flight is
    /// abandoned, a running tool is stopped (bash kills its process
    /// group), and the tool calls without a result get a [`CANCELLED`]
    /// error result. The run then fails with [`Cancelled`]. The caller
    /// resets the flag before the next input.
    pub fn with_cancel(mut self, cancel: Cancel) -> Self {
        self.cancel = cancel;
        self
    }

    /// Asks `gate` before each tool call; a call it turns down does not
    /// run and gets the gate's output as its result. A cancel while the
    /// gate waits (for a person's answer) cancels the call like a running
    /// tool.
    pub fn with_gate(mut self, gate: Box<dyn ToolGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Records every later history change to `journal`.
    pub fn with_journal(mut self, journal: Box<dyn Journal>) -> Self {
        self.journal = Some(journal);
        self
    }

    pub fn messages(&self) -> &[Message] {
        &self.transcript.messages
    }

    /// Tokens used by this session.
    pub fn usage(&self) -> Usage {
        self.transcript.usage
    }

    /// Size of the conversation as of the last model call, in tokens.
    pub fn context_tokens(&self) -> u64 {
        self.transcript.context_tokens
    }

    pub fn options(&self) -> &AgentOptions {
        &self.options
    }

    /// Starts a new session: forgets the conversation and its token usage,
    /// and records from now on to `journal` (or nowhere).
    pub fn reset(&mut self, journal: Option<Box<dyn Journal>>) {
        self.transcript = Transcript::default();
        self.journal = journal;
        self.cleared = false;
    }

    /// Handles one user input and returns the model's final text.
    ///
    /// If the loop fails midway, the history still ends with a user message,
    /// and the next input is appended to it, so the conversation stays valid.
    pub async fn run(
        &mut self,
        input: &str,
        mut on_event: impl FnMut(Event<'_>),
    ) -> Result<String> {
        let entry = self.transcript.user_text_entry(input);
        self.record(entry, &mut on_event);
        let specs = self.tools.specs();
        let cancel = self.cancel.clone();

        for _ in 0..self.options.max_turns {
            self.check_limit()?;
            // Nothing is recorded while a call is in flight, so abandoning
            // it leaves the history as it was: ending with a user message.
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Cancelled.into()),
                response = self.respond(&specs, &mut on_event) => response?,
            };
            self.record(Entry::Usage(response.usage), &mut on_event);

            let mut tool_calls = Vec::new();
            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => on_event(Event::Text(text)),
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push((id.clone(), name.clone(), input.clone()));
                    }
                    block if block.is_thinking() => on_event(Event::Thinking),
                    _ => {}
                }
            }

            let reply = Message {
                role: Role::Assistant,
                content: response.content,
            };
            let text = reply.text();
            // The API rejects empty assistant turns in the history. A reply
            // of nothing but thinking (cut off by the token limit before
            // the model got to answer) has nothing worth keeping either.
            let answered = reply.content.iter().any(|block| !block.is_thinking());
            let only_thought = !answered && !reply.content.is_empty();
            if answered {
                self.record(Entry::Message(reply), &mut on_event);
            }

            // A response cut off by the token limit or a refusal may end in a
            // tool call whose input is incomplete. Never run those; answer
            // each call with an error so tool_use/tool_result stay paired.
            let cut_off = match response.stop_reason {
                StopReason::MaxTokens => Some(
                    "not executed: the response hit the output token limit, so this tool \
                     input may be cut off. Retry with smaller pieces (for example, write a \
                     large file in several steps).",
                ),
                StopReason::Refusal => Some("not executed: the response was stopped by a refusal."),
                _ => None,
            };

            if tool_calls.is_empty() {
                if response.stop_reason == StopReason::MaxTokens {
                    on_event(Event::Warning(if answered {
                        "the answer hit the output token limit and may be cut off"
                    } else {
                        "the response hit the output token limit while the model was still \
                         thinking, so there is no answer; raise JUNEES_MAX_TOKENS or lower \
                         JUNEES_THINKING_BUDGET"
                    }));
                } else if only_thought {
                    on_event(Event::Warning(
                        "the model only thought and gave no answer; ask again or rephrase",
                    ));
                }
                return Ok(text);
            }

            if let Some(reason) = cut_off {
                on_event(Event::Warning(reason));
                let results = tool_calls
                    .into_iter()
                    .map(|(id, _, _)| ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: reason.to_string(),
                        is_error: true,
                    })
                    .collect();
                let results = Message {
                    role: Role::User,
                    content: results,
                };
                self.record(Entry::Message(results), &mut on_event);
                continue;
            }

            let mut results = Vec::with_capacity(tool_calls.len());
            for (id, name, input) in tool_calls {
                // After a cancel, the calls left get a result without running.
                let output = if cancel.is_cancelled() {
                    ToolOutput::error(CANCELLED)
                } else {
                    on_event(Event::ToolCall {
                        name: &name,
                        input: &input,
                    });
                    // Dropping the call stops the tool.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => ToolOutput::error(CANCELLED),
                        output = self.call_tool(&name, input) => output,
                    }
                };
                on_event(Event::ToolResult {
                    name: &name,
                    output: &output,
                });
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            let results = Message {
                role: Role::User,
                content: results,
            };
            self.record(Entry::Message(results), &mut on_event);
            if cancel.is_cancelled() {
                return Err(Cancelled.into());
            }
        }

        Err(TurnLimit(self.options.max_turns).into())
    }

    /// Runs one tool call, unless the gate turns it down.
    async fn call_tool(&mut self, name: &str, input: Value) -> ToolOutput {
        if let Some(gate) = &mut self.gate
            && let Some(refused) = gate.check(name, &input).await
        {
            return refused;
        }
        self.tools.dispatch(name, input).await
    }

    /// Fails with [`LimitReached`] if the limit says to stop.
    fn check_limit(&mut self) -> Result<()> {
        if let Some(limit) = &mut self.limit
            && let Some(reason) = limit(self.transcript.usage)
        {
            return Err(LimitReached(reason).into());
        }
        Ok(())
    }

    /// The next model response: keeps the context under its limit first,
    /// and on an overflow shrinks the history and retries once. The limit
    /// was asked before the first call this makes; it is asked again
    /// before each later one, summary calls included.
    async fn respond(
        &mut self,
        specs: &[ToolSpec],
        on_event: &mut impl FnMut(Event<'_>),
    ) -> Result<CompletionResponse> {
        if self.manage_context(on_event).await {
            self.check_limit()?;
        }
        match self.call(specs).await {
            Ok(response) => Ok(response),
            // Shrink the history as far as it goes and try once more.
            Err(err) if err.is_context_overflow() => {
                self.check_limit()?;
                if !self.shrink(on_event).await {
                    return Err(err.into());
                }
                self.check_limit()?;
                on_event(Event::Warning(
                    "the conversation did not fit the context window; \
                     retrying once with the shrunk history",
                ));
                Ok(self.call(specs).await?)
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn call(&self, specs: &[ToolSpec]) -> Result<CompletionResponse, ProviderError> {
        self.provider
            .complete(CompletionRequest {
                system: &self.options.system,
                messages: &self.transcript.messages,
                tools: specs,
                max_tokens: self.options.max_tokens,
                cache: true,
                thinking: true,
            })
            .await
    }

    /// After the API said the conversation does not fit: clears old tool
    /// output and compacts, whatever the tracked size says, then clears the
    /// latest tool output too. That is often what did not fit (several
    /// large reads in one reply), and compaction keeps it; the model can
    /// ask again for less. Returns whether the history got any smaller.
    async fn shrink(&mut self, on_event: &mut impl FnMut(Event<'_>)) -> bool {
        let cleared = self.clear_tool_results(on_event);
        let compacted = self.summarize(on_event).await;
        let cleared_latest = match self
            .transcript
            .clear_entry_keeping(0, self.provider.manual_thinking())
        {
            Some(entry) => {
                self.record(entry, on_event);
                true
            }
            None => false,
        };
        self.cleared = false;
        cleared || compacted || cleared_latest
    }

    /// Keeps the conversation under the compaction threshold: once the
    /// context size reaches it, old tool output is cleared first. If there
    /// is none to clear, or the next call still measures over the threshold,
    /// the conversation is compacted. Returns whether it tried to, which
    /// takes a summary call.
    async fn manage_context(&mut self, on_event: &mut impl FnMut(Event<'_>)) -> bool {
        if self.transcript.context_tokens < self.options.compact_limit() {
            self.cleared = false;
            return false;
        }
        if !self.cleared && self.clear_tool_results(on_event) {
            self.cleared = true;
            return false;
        }
        self.cleared = false;
        self.summarize(on_event).await;
        true
    }

    /// Replaces the earlier part of the conversation with a summary written
    /// by the model, keeping the latest user inputs and what followed them
    /// (or, within one long turn, its input and latest tool rounds).
    /// Returns whether it did; if there is nothing to compact yet or the
    /// summary fails, it says so in a warning and changes nothing.
    pub async fn compact(&mut self, mut on_event: impl FnMut(Event<'_>)) -> bool {
        if self
            .transcript
            .compact_point(self.provider.manual_thinking())
            .is_none()
        {
            on_event(Event::Warning(
                "nothing to compact yet: the conversation is too short to summarize",
            ));
            return false;
        }
        self.summarize(&mut on_event).await
    }

    /// Compacts at [`Transcript::compact_point`], if there is one.
    async fn summarize(&mut self, on_event: &mut impl FnMut(Event<'_>)) -> bool {
        let manual_thinking = self.provider.manual_thinking();
        let Some(keep_from) = self.transcript.compact_point(manual_thinking) else {
            return false;
        };
        let request = [context::summary_request(
            &self.transcript.messages[..keep_from],
        )];
        let result = self
            .provider
            .complete(CompletionRequest {
                system: context::SUMMARY_SYSTEM,
                messages: &request,
                tools: &[],
                max_tokens: context::SUMMARY_MAX_TOKENS,
                // The summarized prefix is sent once; caching it would only
                // add the cost of a cache write.
                cache: false,
                // Its own small output limit leaves no room for a thinking
                // budget, and a summary needs none.
                thinking: false,
            })
            .await;
        let response = match result {
            Ok(response) => response,
            Err(err) => {
                on_event(Event::Warning(&format!(
                    "could not compact the conversation ({err}); it stays as it is"
                )));
                return false;
            }
        };
        self.record(Entry::SummaryUsage(response.usage), on_event);
        // The summary ends with the work that remains; a cut-off one would
        // lose exactly that, and the messages it replaces are gone for good.
        if response.stop_reason == StopReason::MaxTokens {
            on_event(Event::Warning(
                "could not compact the conversation (the summary hit the output token \
                 limit and would be cut off); it stays as it is",
            ));
            return false;
        }
        let summary = Message {
            role: Role::Assistant,
            content: response.content,
        }
        .text();
        if summary.trim().is_empty() {
            on_event(Event::Warning(
                "could not compact the conversation (the summary came back empty); \
                 it stays as it is",
            ));
            return false;
        }

        let before_messages = self.transcript.messages.len();
        let entry = Entry::Compact {
            summary,
            keep_from,
            keep_thinking: manual_thinking,
        };
        self.record(entry, on_event);
        self.cleared = false;
        on_event(Event::Compacted {
            before_messages,
            after_messages: self.transcript.messages.len(),
        });
        true
    }

    /// Clears the output of all but the latest tool results. Returns
    /// whether there was anything to clear.
    fn clear_tool_results(&mut self, on_event: &mut impl FnMut(Event<'_>)) -> bool {
        match self.transcript.clear_entry(self.provider.manual_thinking()) {
            Some(entry) => {
                self.record(entry, on_event);
                true
            }
            None => false,
        }
    }

    /// Applies a history change and records it in the journal. If the
    /// journal cannot be written, the turn goes on without it: one warning,
    /// and nothing more is recorded, so the file stays a clean prefix of the
    /// conversation instead of one with holes.
    fn record(&mut self, entry: Entry, on_event: &mut impl FnMut(Event<'_>)) {
        if let Some(journal) = &mut self.journal
            && let Err(err) = journal.record(&entry)
        {
            self.journal = None;
            on_event(Event::Warning(&format!(
                "could not write the session journal ({err}); \
                 the rest of this session will not be saved"
            )));
        }
        self.transcript.apply(entry);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::session::{self, Session, SessionWriter};

    type Reply = std::result::Result<CompletionResponse, ProviderError>;
    use crate::tools::Tool;

    /// System prompt, tool count, max_tokens, caching and thinking of a
    /// request.
    type Settings = (String, usize, u32, bool, bool);

    /// Replays canned responses and records the history it was sent.
    #[derive(Clone, Default)]
    struct Scripted {
        responses: Arc<Mutex<VecDeque<Reply>>>,
        seen: Arc<Mutex<Vec<Vec<Message>>>>,
        settings: Arc<Mutex<Vec<Settings>>>,
        /// What [`Provider::manual_thinking`] says.
        manual_thinking: bool,
    }

    impl Scripted {
        fn new(responses: Vec<Reply>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl Provider for Scripted {
        async fn complete(
            &self,
            request: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, ProviderError> {
            self.seen.lock().unwrap().push(request.messages.to_vec());
            self.settings.lock().unwrap().push((
                request.system.to_string(),
                request.tools.len(),
                request.max_tokens,
                request.cache,
                request.thinking,
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("no scripted response left")
        }

        fn manual_thinking(&self) -> bool {
            self.manual_thinking
        }
    }

    struct Upper;

    #[async_trait]
    impl Tool for Upper {
        fn name(&self) -> &str {
            "upper"
        }
        fn description(&self) -> &str {
            "uppercases text"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn run(&self, input: Value) -> ToolOutput {
            ToolOutput::ok(input["text"].as_str().unwrap_or_default().to_uppercase())
        }
    }

    fn text(text: &str) -> Reply {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 1,
                ..Default::default()
            },
        })
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Reply {
        Ok(CompletionResponse {
            content: vec![
                ContentBlock::Text {
                    text: "let me check".into(),
                },
                ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input,
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 2,
                ..Default::default()
            },
        })
    }

    fn agent(provider: Scripted, max_turns: usize) -> Agent {
        let mut tools = ToolRegistry::new();
        tools.register(Upper);
        Agent::new(
            Box::new(provider),
            tools,
            AgentOptions {
                system: "test".into(),
                max_tokens: 100,
                max_turns,
                context_window: 1000,
                compact_threshold: 0.8,
            },
        )
    }

    #[tokio::test]
    async fn answers_without_tools() {
        let provider = Scripted::new(vec![text("hello")]);
        let mut agent = agent(provider, 5);
        let answer = agent.run("hi", |_| {}).await.unwrap();
        assert_eq!(answer, "hello");
        assert_eq!(agent.messages().len(), 2);
    }

    #[tokio::test]
    async fn runs_tools_and_feeds_results_back() {
        let provider = Scripted::new(vec![
            tool_use("toolu_1", "upper", json!({"text": "abc"})),
            text("it is ABC"),
        ]);
        let seen = provider.seen.clone();
        let mut agent = agent(provider, 5);

        let mut events = Vec::new();
        let answer = agent
            .run("shout abc", |event| events.push(format!("{event:?}")))
            .await
            .unwrap();

        assert_eq!(answer, "it is ABC");
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(events[1].starts_with("ToolCall"));
        assert!(events[2].contains("ABC"));

        let history = agent.messages();
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].role, Role::User);
        assert_eq!(
            history[2].content,
            vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "ABC".into(),
                is_error: false,
            }]
        );
        // The second model call saw the tool result.
        assert_eq!(seen.lock().unwrap()[1].len(), 3);
        assert_eq!(
            agent.usage(),
            Usage {
                input_tokens: 15,
                output_tokens: 3,
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn unknown_tools_come_back_as_errors() {
        let provider = Scripted::new(vec![
            tool_use("toolu_1", "missing", json!({})),
            text("sorry"),
        ]);
        let mut agent = agent(provider, 5);
        agent.run("go", |_| {}).await.unwrap();
        assert!(matches!(
            &agent.messages()[2].content[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
    }

    #[tokio::test]
    async fn gives_up_after_max_turns() {
        let provider = Scripted::new(vec![
            tool_use("toolu_1", "upper", json!({"text": "a"})),
            tool_use("toolu_2", "upper", json!({"text": "b"})),
        ]);
        let mut agent = agent(provider, 2);
        let err = agent.run("loop", |_| {}).await.unwrap_err();
        assert!(err.to_string().contains("stopped after 2 model calls"));
        assert_eq!(err.downcast_ref::<TurnLimit>(), Some(&TurnLimit(2)));
    }

    #[tokio::test]
    async fn a_limit_stops_the_run_between_model_calls() {
        let provider = Scripted::new(vec![
            tool_use("toolu_1", "upper", json!({"text": "a"})),
            tool_use("toolu_2", "upper", json!({"text": "b"})),
            text("never asked for"),
        ]);
        let seen = provider.seen.clone();
        let mut asked = Vec::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut agent = agent(provider, 5).with_limit(Box::new(move |usage: Usage| {
            sender.send(usage.input_tokens).unwrap();
            (usage.input_tokens >= 10).then(|| format!("used {}", usage.input_tokens))
        }));
        let err = agent.run("go", |_| {}).await.unwrap_err();
        asked.extend(receiver.try_iter());

        assert_eq!(
            err.downcast_ref::<LimitReached>(),
            Some(&LimitReached("used 10".into()))
        );
        // Asked before each call: 0, 5, then 10 stops it.
        assert_eq!(asked, [0, 5, 10]);
        assert_eq!(seen.lock().unwrap().len(), 2);
        let history = agent.messages();
        assert_eq!(history.len(), 5);
        assert_eq!(tool_output(&history[4]), [("toolu_2", "B", false)]);
        assert_valid(history);
    }

    #[tokio::test]
    async fn input_after_a_failure_joins_the_trailing_user_message() {
        let provider = Scripted::new(vec![
            Err(ProviderError::Transport("network down".into())),
            text("ok"),
        ]);
        let seen = provider.seen.clone();
        let mut agent = agent(provider, 5);

        assert!(agent.run("first", |_| {}).await.is_err());
        agent.run("second", |_| {}).await.unwrap();

        let sent = &seen.lock().unwrap()[1];
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text(), "first\nsecond");
    }

    #[tokio::test]
    async fn provider_errors_stay_typed() {
        let provider = Scripted::new(vec![Err(ProviderError::Api {
            status: 400,
            kind: Some("invalid_request_error".into()),
            message: "prompt is too long: 300000 tokens > 200000 maximum".into(),
        })]);
        let mut agent = agent(provider, 5);
        let err = agent.run("hi", |_| {}).await.unwrap_err();
        let provider_error = err.downcast_ref::<ProviderError>().unwrap();
        assert!(provider_error.is_context_overflow());
    }

    #[tokio::test]
    async fn empty_replies_are_not_stored() {
        let provider = Scripted::new(vec![Ok(CompletionResponse {
            content: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })]);
        let mut agent = agent(provider, 5);
        assert_eq!(agent.run("hi", |_| {}).await.unwrap(), "");
        assert_eq!(agent.messages().len(), 1);
    }

    #[tokio::test]
    async fn reset_clears_history() {
        let provider = Scripted::new(vec![text("a")]);
        let mut agent = agent(provider, 5);
        agent.run("hi", |_| {}).await.unwrap();
        agent.reset(None);
        assert!(agent.messages().is_empty());
        assert_eq!(agent.usage(), Usage::default());
    }

    /// Counts how often it runs.
    struct Counter(Arc<AtomicUsize>);

    #[async_trait]
    impl Tool for Counter {
        fn name(&self) -> &str {
            "count"
        }
        fn description(&self) -> &str {
            "counts calls"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn run(&self, _input: Value) -> ToolOutput {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("counted")
        }
    }

    fn counting_agent(provider: Scripted) -> (Agent, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut agent = agent(provider, 5);
        agent.tools.register(Counter(calls.clone()));
        (agent, calls)
    }

    fn with_stop(response: Reply, stop: StopReason) -> Reply {
        response.map(|mut r| {
            r.stop_reason = stop;
            r
        })
    }

    #[tokio::test]
    async fn tool_calls_cut_off_by_max_tokens_are_not_run() {
        let provider = Scripted::new(vec![
            with_stop(
                tool_use("toolu_1", "count", json!({"content": "fn main() {"})),
                StopReason::MaxTokens,
            ),
            text("done"),
        ]);
        let (mut agent, calls) = counting_agent(provider);
        let mut warnings = Vec::new();
        let answer = agent
            .run("write it", |event| {
                if let Event::Warning(w) = event {
                    warnings.push(w.to_string());
                }
            })
            .await
            .unwrap();

        assert_eq!(answer, "done");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(warnings.len(), 1);
        match &agent.messages()[2].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "toolu_1");
                assert!(content.starts_with("not executed"));
                assert!(is_error);
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_calls_in_a_refusal_are_not_run() {
        let provider = Scripted::new(vec![
            with_stop(tool_use("toolu_1", "count", json!({})), StopReason::Refusal),
            text("ok"),
        ]);
        let (mut agent, calls) = counting_agent(provider);
        agent.run("go", |_| {}).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn warns_when_a_text_answer_is_cut_off() {
        let provider = Scripted::new(vec![with_stop(text("half an ans"), StopReason::MaxTokens)]);
        let mut agent = agent(provider, 5);
        let mut warned = false;
        let answer = agent
            .run("explain", |event| {
                warned |= matches!(event, Event::Warning(_))
            })
            .await
            .unwrap();
        assert_eq!(answer, "half an ans");
        assert!(warned);
    }

    fn thinking_block(signature: &str) -> ContentBlock {
        ContentBlock::Other(json!({"type": "thinking", "thinking": "first upper it",
                                   "signature": signature}))
    }

    #[tokio::test]
    async fn thinking_blocks_are_reported_kept_and_sent_back() {
        let redacted = ContentBlock::Other(json!({"type": "redacted_thinking", "data": "x"}));
        let mut call = tool_use("toolu_1", "upper", json!({"text": "abc"}));
        if let Ok(response) = &mut call {
            response
                .content
                .splice(0..0, [thinking_block("sig-1"), redacted]);
        }
        let reply = call.as_ref().unwrap().content.clone();
        let provider = Scripted::new(vec![call, text("ABC")]);
        let seen = provider.seen.clone();
        let mut agent = agent(provider, 5);
        let mut events = Vec::new();
        agent
            .run("shout", |event| events.push(format!("{event:?}")))
            .await
            .unwrap();

        // Reported in the order they came, ahead of the text and the call.
        assert_eq!(events[..2], ["Thinking", "Thinking"]);
        assert!(events[2].starts_with("Text"), "{events:?}");
        assert!(events[3].starts_with("ToolCall"), "{events:?}");
        assert_eq!(agent.messages()[1].content, reply);
        assert_eq!(seen.lock().unwrap()[1][1].content, reply);
        assert_valid(agent.messages());
    }

    #[tokio::test]
    async fn a_reply_cut_off_while_thinking_is_not_stored() {
        let provider = Scripted::new(vec![
            Ok(CompletionResponse {
                content: vec![thinking_block("")],
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
            }),
            text("ok"),
        ]);
        let seen = provider.seen.clone();
        let mut agent = agent(provider, 5);
        let mut warnings = Vec::new();
        let answer = agent
            .run("think hard", |event| {
                if let Event::Warning(w) = event {
                    warnings.push(w.to_string());
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("still thinking"), "{warnings:?}");
        assert_eq!(agent.messages(), [Message::user_text("think hard")]);

        // The next input joins the input left without an answer.
        agent.run("again", |_| {}).await.unwrap();
        assert_eq!(seen.lock().unwrap()[1][0].text(), "think hard\nagain");
        assert_valid(agent.messages());
    }

    #[tokio::test]
    async fn a_reply_of_only_thinking_warns_that_there_is_no_answer() {
        let provider = Scripted::new(vec![
            Ok(CompletionResponse {
                content: vec![thinking_block("sig")],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }),
        ]);
        let mut agent = agent(provider, 5);
        let mut warnings = Vec::new();
        let answer = agent
            .run("hm", |event| {
                if let Event::Warning(w) = event {
                    warnings.push(w.to_string());
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("only thought"), "{warnings:?}");
        assert_eq!(agent.messages(), [Message::user_text("hm")]);
        // An empty reply is no thinking: nothing to say about it.
        agent
            .run("again", |event| {
                assert!(!matches!(event, Event::Warning(_)), "{event:?}")
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn several_tool_calls_share_one_result_message_in_order() {
        let provider = Scripted::new(vec![
            Ok(CompletionResponse {
                content: vec![
                    ContentBlock::ToolUse {
                        id: "a".into(),
                        name: "upper".into(),
                        input: json!({"text": "x"}),
                    },
                    ContentBlock::ToolUse {
                        id: "b".into(),
                        name: "upper".into(),
                        input: json!({"text": "y"}),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            }),
            text("done"),
        ]);
        let mut agent = agent(provider, 5);
        agent.run("go", |_| {}).await.unwrap();
        let ids: Vec<(&str, &str)> = agent.messages()[2]
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => (tool_use_id.as_str(), content.as_str()),
                other => panic!("unexpected block {other:?}"),
            })
            .collect();
        assert_eq!(ids, [("a", "X"), ("b", "Y")]);
    }

    /// Keeps recorded entries in memory.
    #[derive(Clone, Default)]
    struct Memory(Arc<Mutex<Vec<Entry>>>);

    impl Journal for Memory {
        fn record(&mut self, entry: &Entry) -> std::io::Result<()> {
            self.0.lock().unwrap().push(entry.clone());
            Ok(())
        }
    }

    /// Fails every write and counts the attempts.
    struct Broken(Arc<AtomicUsize>);

    impl Journal for Broken {
        fn record(&mut self, _entry: &Entry) -> std::io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("disk full"))
        }
    }

    fn replay(entries: impl IntoIterator<Item = Entry>) -> Transcript {
        let mut transcript = Transcript::default();
        for entry in entries {
            transcript.apply(entry);
        }
        transcript
    }

    /// A failed turn (so the next input joins its user message), a tool
    /// call, a cut-off tool call and a final answer.
    fn eventful_script() -> Vec<Reply> {
        vec![
            Err(ProviderError::Transport("network down".into())),
            tool_use("toolu_1", "upper", json!({"text": "abc"})),
            with_stop(
                tool_use("toolu_2", "upper", json!({"text": "de"})),
                StopReason::MaxTokens,
            ),
            text("done"),
        ]
    }

    #[tokio::test]
    async fn journal_replays_to_the_same_history() {
        let journal = Memory::default();
        let mut agent =
            agent(Scripted::new(eventful_script()), 5).with_journal(Box::new(journal.clone()));
        assert!(agent.run("first", |_| {}).await.is_err());
        agent.run("second", |_| {}).await.unwrap();

        let entries = journal.0.lock().unwrap().clone();
        assert!(matches!(entries[0], Entry::Message(_)));
        assert_eq!(
            entries[1],
            Entry::AppendText {
                text: "second".into()
            }
        );
        assert_eq!(agent.messages().len(), 6);
        let replayed = replay(entries);
        assert_eq!(replayed, agent.transcript);
        // The size is the last call's, not a sum.
        assert_eq!(agent.context_tokens(), 11);
    }

    #[tokio::test]
    async fn a_session_file_restores_the_conversation() {
        let home = tempfile::tempdir().unwrap();
        let (meta, writer) =
            SessionWriter::create(home.path(), std::path::Path::new("/work"), "m").unwrap();
        let mut agent = agent(Scripted::new(eventful_script()), 5).with_journal(Box::new(writer));
        assert!(agent.run("first", |_| {}).await.is_err());
        agent.run("second", |_| {}).await.unwrap();

        let path = crate::session::sessions_dir(home.path()).join(format!("{}.jsonl", meta.id));
        // Close the first writer (and its lock), as an exiting process would.
        let (messages, usage) = (agent.messages().to_vec(), agent.usage());
        agent.reset(None);
        let (session, writer) = Session::resume(&path).unwrap();
        assert_eq!(session.transcript.messages, messages);
        assert_eq!(session.transcript.usage, usage);
        assert_eq!(session.transcript.context_tokens, 11);

        let provider = Scripted::new(vec![text("again")]);
        let seen = provider.seen.clone();
        let mut tools = ToolRegistry::new();
        tools.register(Upper);
        let mut restored = Agent::restore(
            Box::new(provider),
            tools,
            agent.options.clone(),
            session.transcript,
        )
        .with_journal(Box::new(writer));
        restored.run("third", |_| {}).await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].len(), 7);
        assert_eq!(restored.usage().input_tokens, usage.input_tokens + 10);

        let reloaded = Session::load(&path).unwrap();
        assert_eq!(reloaded.transcript.messages, restored.messages());
        assert_eq!(reloaded.transcript.usage, restored.usage());
    }

    #[tokio::test]
    async fn journal_failures_warn_once_and_the_turn_goes_on() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Scripted::new(vec![
            tool_use("toolu_1", "upper", json!({"text": "abc"})),
            text("done"),
        ]);
        let mut agent = agent(provider, 5).with_journal(Box::new(Broken(attempts.clone())));
        let mut warnings = Vec::new();
        let answer = agent
            .run("go", |event| {
                if let Event::Warning(w) = event {
                    warnings.push(w.to_string());
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "done");
        assert_eq!(agent.messages().len(), 4);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("disk full"));
    }

    #[tokio::test]
    async fn reset_switches_to_the_new_journal() {
        let old = Memory::default();
        let new = Memory::default();
        let provider = Scripted::new(vec![text("a"), text("b")]);
        let mut agent = agent(provider, 5).with_journal(Box::new(old.clone()));
        agent.run("one", |_| {}).await.unwrap();
        agent.reset(Some(Box::new(new.clone())));
        agent.run("two", |_| {}).await.unwrap();

        assert_eq!(old.0.lock().unwrap().len(), 3);
        let entries = new.0.lock().unwrap().clone();
        assert_eq!(entries[0], Entry::Message(Message::user_text("two")));
        assert_eq!(replay(entries).messages, agent.messages());
    }

    /// The reply with its usage set so the context size reads `tokens`.
    fn sized(reply: Reply, tokens: u64) -> Reply {
        reply.map(|mut r| {
            r.usage = Usage {
                input_tokens: tokens - 1,
                output_tokens: 1,
                ..Default::default()
            };
            r
        })
    }

    /// `n` calls of the `upper` tool, toolu_1..=toolu_n; the first is an
    /// unknown tool, so its result is an error.
    fn tool_rounds(n: usize, tokens: u64) -> Vec<Reply> {
        (1..=n)
            .map(|i| {
                let name = if i == 1 { "missing" } else { "upper" };
                let text = format!("t{i}");
                sized(
                    tool_use(&format!("toolu_{i}"), name, json!({ "text": text })),
                    tokens,
                )
            })
            .collect()
    }

    fn tool_output(message: &Message) -> Vec<(&str, &str, bool)> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => Some((tool_use_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            })
            .collect()
    }

    /// Roles alternate starting with the user, and every tool call is
    /// answered in the next message by a result with its id (and only then).
    fn assert_valid(messages: &[Message]) {
        for (i, message) in messages.iter().enumerate() {
            let expected = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            assert_eq!(message.role, expected, "message {i}: {messages:#?}");
            let calls: Vec<&str> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            let answered: Vec<&str> = messages
                .get(i + 1)
                .map(|next| tool_output(next).into_iter().map(|(id, _, _)| id).collect())
                .unwrap_or_default();
            assert_eq!(calls, answered, "message {i}: {messages:#?}");
            if i == 0 {
                assert!(tool_output(message).is_empty(), "{messages:#?}");
            }
        }
    }

    fn entries_of(journal: &Memory) -> Vec<Entry> {
        journal.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn nothing_changes_below_the_threshold() {
        // The limit is 800 of 1000 tokens.
        let mut script = tool_rounds(5, 799);
        script.push(sized(text("done"), 799));
        let provider = Scripted::new(script);
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        agent.run("go", |_| {}).await.unwrap();

        assert_eq!(agent.context_tokens(), 799);
        let history = agent.messages();
        assert_eq!(history.len(), 12);
        assert_eq!(tool_output(&history[4]), [("toolu_2", "T2", false)]);
        // Each call saw the history exactly as it was built.
        for (i, sent) in seen.lock().unwrap().iter().enumerate() {
            assert_eq!(sent[..], history[..2 * i + 1]);
        }
        assert!(
            entries_of(&journal)
                .iter()
                .all(|entry| matches!(entry, Entry::Message(_) | Entry::Usage(_)))
        );
    }

    /// `replies` with their context sizes set to `sizes`, in order.
    fn resized(replies: Vec<Reply>, sizes: &[u64]) -> Vec<Reply> {
        assert_eq!(replies.len(), sizes.len());
        replies
            .into_iter()
            .zip(sizes)
            .map(|(reply, &tokens)| sized(reply, tokens))
            .collect()
    }

    #[tokio::test]
    async fn clears_all_but_the_last_three_tool_results_over_the_threshold() {
        // Clearing is enough the first time (700 after it), so no
        // compaction comes between the two clears.
        let mut script = resized(tool_rounds(6, 1), &[100, 100, 100, 900, 700, 900]);
        script.push(sized(text("done"), 900));
        let provider = Scripted::new(script);
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        agent.run("go", |_| {}).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 7);
        // Before the fifth call: results at 2, 4, 6, 8; the first is cleared,
        // keeping its id and error flag.
        assert_eq!(
            tool_output(&seen[4][2]),
            [("toolu_1", session::CLEARED, true)]
        );
        assert_eq!(tool_output(&seen[4][4]), [("toolu_2", "T2", false)]);
        assert_eq!(tool_output(&seen[3][2])[0].1, "unknown tool `missing`");
        // The fifth call measured under the threshold; before the seventh,
        // the two results added since are past the last three.
        assert_eq!(seen[5][4], seen[4][4]);
        assert_eq!(
            tool_output(&seen[6][4]),
            [("toolu_2", session::CLEARED, false)]
        );
        assert_eq!(
            tool_output(&seen[6][6]),
            [("toolu_3", session::CLEARED, false)]
        );
        for (i, id) in [(8, "toolu_4"), (10, "toolu_5"), (12, "toolu_6")] {
            let output = tool_output(&seen[6][i]);
            assert_eq!(output[0].0, id);
            assert_ne!(output[0].1, session::CLEARED);
        }

        let entries = entries_of(&journal);
        let clears: Vec<&Entry> = entries
            .iter()
            .filter(|entry| matches!(entry, Entry::ClearToolResults { .. }))
            .collect();
        assert_eq!(
            clears,
            [
                &Entry::ClearToolResults {
                    upto: 4,
                    keep_thinking: false
                },
                &Entry::ClearToolResults {
                    upto: 8,
                    keep_thinking: false
                }
            ]
        );
        assert_valid(agent.messages());
        assert_eq!(replay(entries), agent.transcript);
    }

    fn thinking_in(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|block| block.is_thinking())
            .map(|block| match block {
                ContentBlock::Other(value) => value["signature"].to_string(),
                _ => unreachable!(),
            })
            .collect()
    }

    #[tokio::test]
    async fn thinking_is_not_sent_back_after_tool_output_was_cleared() {
        // Every reply thinks first, as Claude 5 models do by default.
        let mut script = resized(tool_rounds(6, 1), &[100, 100, 100, 900, 100, 100]);
        for (i, reply) in script.iter_mut().enumerate() {
            if let Ok(response) = reply {
                let signature = format!("sig-{}", i + 1);
                response.content.insert(0, thinking_block(&signature));
            }
        }
        script.push(sized(text("done"), 100));
        let provider = Scripted::new(script);
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        agent.run("go", |_| {}).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 7);
        // Sent back unchanged while nothing before them changed.
        assert_eq!(
            thinking_in(&seen[3]),
            ["\"sig-1\"", "\"sig-2\"", "\"sig-3\""]
        );
        // The fourth call went over the threshold, so old tool output was
        // cleared before the fifth: none of the thinking made before that
        // goes out again, and what came after it does.
        assert_eq!(tool_output(&seen[4][2])[0].1, session::CLEARED);
        assert!(thinking_in(&seen[4]).is_empty(), "{:#?}", seen[4]);
        assert_eq!(thinking_in(&seen[6]), ["\"sig-5\"", "\"sig-6\""]);
        assert_valid(agent.messages());
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    #[tokio::test]
    async fn manual_thinking_keeps_thinking_and_compacts_a_turn_whole() {
        // Manual extended thinking: only the turn's first reply thinks.
        let mut script = resized(tool_rounds(5, 1), &[100, 100, 100, 900, 900]);
        if let Ok(response) = &mut script[0] {
            response.content.insert(0, thinking_block("sig-1"));
        }
        script.push(sized(text("SUMMARY"), 50));
        script.push(sized(text("done"), 100));
        let provider = Scripted {
            manual_thinking: true,
            ..Scripted::new(script)
        };
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        agent.run("go", |_| {}).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 7);
        // Old tool output was cleared before the fifth call; the models
        // that take a thinking budget do not check what came before a
        // thinking block, and the turn has to keep starting with it.
        assert_eq!(tool_output(&seen[4][2])[0].1, session::CLEARED);
        assert_eq!(thinking_in(&seen[4]), ["\"sig-1\""]);
        // Still over, so the turn was compacted: all of it, since a kept
        // tail would start with a reply that has no thinking. The model
        // starts the turn again, thinking first, from summary and input.
        assert_eq!(seen[6].len(), 1, "{:#?}", seen[6]);
        assert_eq!(summary_text(&seen[6][0]).as_deref(), Some("SUMMARY"));
        assert_eq!(
            seen[6][0].content[1],
            ContentBlock::Text { text: "go".into() }
        );
        let entries = entries_of(&journal);
        assert!(entries.contains(&Entry::Compact {
            summary: "SUMMARY".into(),
            keep_from: 11,
            keep_thinking: true,
        }));
        assert_valid(agent.messages());
        assert_eq!(replay(entries), agent.transcript);
    }

    fn summary_text(message: &Message) -> Option<String> {
        match message.content.first() {
            Some(ContentBlock::Text { text }) => text
                .strip_prefix(session::SUMMARY_HEADER)
                .map(str::to_string),
            _ => None,
        }
    }

    /// Four inputs: a turn with four tool calls, then three plain ones, the
    /// later two over the threshold.
    fn long_conversation(summary: Reply) -> Vec<Reply> {
        let mut script = tool_rounds(4, 100);
        script.push(sized(text("one"), 100));
        script.push(sized(text("two"), 900));
        script.push(sized(text("three"), 900));
        script.push(summary);
        script.push(sized(text("four"), 300));
        script
    }

    #[tokio::test]
    async fn compacts_when_clearing_was_not_enough() {
        let summary = Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: "SUMMARY".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 50,
                output_tokens: 7,
                ..Default::default()
            },
        });
        let provider = Scripted::new(long_conversation(summary));
        let (seen, settings) = (provider.seen.clone(), provider.settings.clone());
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        let mut compacted = Vec::new();
        for input in ["first", "second", "third", "fourth"] {
            agent
                .run(input, |event| {
                    if let Event::Compacted {
                        before_messages,
                        after_messages,
                    } = event
                    {
                        compacted.push((before_messages, after_messages));
                    }
                })
                .await
                .unwrap();
        }

        // "second" went over; before "third" old tool output was cleared;
        // "third" still measured over, so "fourth" compacts first.
        let seen = seen.lock().unwrap();
        let settings = settings.lock().unwrap();
        assert_eq!(seen.len(), 9);
        assert_eq!(tool_output(&seen[6][2])[0].1, session::CLEARED);
        let (system, tools, max_tokens, cache, thinking) = &settings[7];
        assert_eq!(system, context::SUMMARY_SYSTEM);
        assert_eq!(
            (*tools, *max_tokens, *cache, *thinking),
            (0, 4096, false, false)
        );
        assert_eq!(
            (settings[8].1, settings[8].3, settings[8].4),
            (1, true, true)
        );
        // The summary call saw the history before the second-last input
        // ("third", at 12) as text in one message.
        assert_eq!(seen[7].len(), 1);
        let asked = seen[7][0].text();
        for part in [
            "[user]\nfirst",
            "[tool call toolu_4: upper]",
            "[user]\nsecond",
            "two",
        ] {
            assert!(asked.contains(part), "{part} missing from {asked}");
        }
        assert!(!asked.contains("third"), "{asked}");

        // The last call started from the summary, glued to "third".
        let sent = &seen[8];
        assert_eq!(sent.len(), 3);
        assert_eq!(summary_text(&sent[0]).as_deref(), Some("SUMMARY"));
        assert_eq!(
            sent[0].content[1],
            ContentBlock::Text {
                text: "third".into()
            }
        );
        assert_eq!(sent[1].text(), "three");
        assert_eq!(sent[2], Message::user_text("fourth"));
        assert_valid(sent);
        assert_valid(agent.messages());
        assert_eq!(compacted, [(15, 3)]);

        // The summary call counts toward usage, not toward the context size.
        assert_eq!(
            agent.usage().input_tokens,
            4 * 99 + 99 + 899 + 899 + 50 + 299
        );
        assert_eq!(agent.context_tokens(), 300);
        let entries = entries_of(&journal);
        assert!(entries.contains(&Entry::Compact {
            summary: "SUMMARY".into(),
            keep_from: 12,
            keep_thinking: false,
        }));
        assert_eq!(replay(entries), agent.transcript);
    }

    #[tokio::test]
    async fn the_limit_is_asked_again_after_a_summary_call() {
        let summary = sized(text("SUMMARY"), 51);
        let provider = Scripted::new(long_conversation(summary));
        let seen = provider.seen.clone();
        // Under 2300 tokens until the summary call (50 in) of "fourth".
        let mut agent = agent(provider, 10).with_limit(Box::new(|usage: Usage| {
            (usage.input_tokens >= 2300).then(|| format!("used {}", usage.input_tokens))
        }));
        for input in ["first", "second", "third"] {
            agent.run(input, |_| {}).await.unwrap();
        }
        let err = agent.run("fourth", |_| {}).await.unwrap_err();

        assert_eq!(
            err.downcast_ref::<LimitReached>(),
            Some(&LimitReached("used 2343".into()))
        );
        // The summary call was made, the model call after it was not.
        assert_eq!(seen.lock().unwrap().len(), 8);
        let history = agent.messages();
        assert_eq!(summary_text(&history[0]).as_deref(), Some("SUMMARY"));
        assert_eq!(history.last(), Some(&Message::user_text("fourth")));
        assert_valid(history);
    }

    #[tokio::test]
    async fn a_failed_summary_leaves_the_history_alone() {
        let failure = Err(ProviderError::Transport("network down".into()));
        let provider = Scripted::new(long_conversation(failure));
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        for input in ["first", "second", "third"] {
            agent.run(input, |_| {}).await.unwrap();
        }
        let before = agent.transcript.clone();
        let entries_before = entries_of(&journal).len();

        let mut warnings = Vec::new();
        let answer = agent
            .run("fourth", |event| {
                if let Event::Warning(w) = event {
                    warnings.push(w.to_string());
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "four");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("could not compact"), "{warnings:?}");
        assert!(warnings[0].contains("network down"), "{warnings:?}");

        // The model got the history as it was, plus the new input.
        let seen = seen.lock().unwrap();
        assert_eq!(seen[8][..14], before.messages[..]);
        assert_eq!(agent.messages()[..14], before.messages[..]);
        let new_entries = &entries_of(&journal)[entries_before..];
        assert!(
            new_entries
                .iter()
                .all(|entry| matches!(entry, Entry::Message(_) | Entry::Usage(_))),
            "{new_entries:?}"
        );
    }

    #[tokio::test]
    async fn a_summary_cut_off_by_the_token_limit_is_not_used() {
        let cut_off = with_stop(text("Goals: ... Remaining wo"), StopReason::MaxTokens);
        let provider = Scripted::new(long_conversation(cut_off));
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        for input in ["first", "second", "third"] {
            agent.run(input, |_| {}).await.unwrap();
        }
        let before = agent.transcript.clone();
        let entries_before = entries_of(&journal).len();

        let mut warnings = Vec::new();
        let answer = agent
            .run("fourth", |event| {
                if let Event::Warning(w) = event {
                    warnings.push(w.to_string());
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "four");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("output token limit"), "{warnings:?}");
        assert_eq!(agent.messages()[..14], before.messages[..]);
        // The call still counts toward usage.
        let new_entries = &entries_of(&journal)[entries_before..];
        assert!(
            new_entries
                .iter()
                .all(|entry| !matches!(entry, Entry::Compact { .. })),
            "{new_entries:?}"
        );
        assert!(
            new_entries
                .iter()
                .any(|entry| matches!(entry, Entry::SummaryUsage(_))),
            "{new_entries:?}"
        );
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    #[tokio::test]
    async fn compact_on_request() {
        let provider = Scripted::new(vec![text("a"), text("b"), text("so far: a, b")]);
        let mut agent = agent(provider, 5);
        let mut warnings = Vec::new();
        agent.run("one", |_| {}).await.unwrap();
        // One input is not enough to compact.
        assert!(
            !agent
                .compact(|event| {
                    if let Event::Warning(w) = event {
                        warnings.push(w.to_string())
                    }
                })
                .await
        );
        assert!(warnings[0].starts_with("nothing to compact yet"));
        assert_eq!(agent.messages().len(), 2);

        agent.run("two", |_| {}).await.unwrap();
        assert!(agent.compact(|_| {}).await);
        let history = agent.messages();
        assert_eq!(history.len(), 2);
        assert_eq!(summary_text(&history[0]).as_deref(), Some("so far: a, b"));
        assert_eq!(
            history[0].content[1],
            ContentBlock::Text { text: "two".into() }
        );
        assert_eq!(history[1].text(), "b");
    }

    /// One input, a tool round per call, the reported size growing past
    /// the threshold: once clearing is not enough, the turn is compacted
    /// inside itself.
    #[tokio::test]
    async fn a_single_long_turn_is_compacted_inside_the_turn() {
        let mut script = resized(tool_rounds(6, 1), &[300, 450, 600, 750, 900, 900]);
        script.push(text("SUMMARY"));
        script.push(sized(text("done"), 300));
        let provider = Scripted::new(script);
        let (seen, settings) = (provider.seen.clone(), provider.settings.clone());
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        let mut compacted = Vec::new();
        let answer = agent
            .run("go", |event| {
                if let Event::Compacted {
                    before_messages,
                    after_messages,
                } = event
                {
                    compacted.push((before_messages, after_messages));
                }
            })
            .await
            .unwrap();
        assert_eq!(answer, "done");

        // Before the sixth call old output was cleared; the sixth still
        // measured over, so a summary was asked for before the seventh.
        let seen = seen.lock().unwrap();
        let settings = settings.lock().unwrap();
        assert_eq!(seen.len(), 8);
        assert_eq!(tool_output(&seen[5][2])[0].1, session::CLEARED);
        assert_eq!(settings[6].0, context::SUMMARY_SYSTEM);
        let asked = seen[6][0].text();
        assert!(asked.contains("[user]\ngo"), "{asked}");
        assert!(asked.contains("[tool call toolu_4: upper]"), "{asked}");
        assert!(!asked.contains("toolu_5"), "{asked}");

        // The next request starts with the summary and the input as typed,
        // then the last two tool rounds, and is a valid conversation.
        let sent = &seen[7];
        assert_eq!(sent.len(), 5);
        assert_eq!(summary_text(&sent[0]).as_deref(), Some("SUMMARY"));
        assert_eq!(sent[0].content[1], ContentBlock::Text { text: "go".into() });
        assert_eq!(sent[1].role, Role::Assistant);
        assert_eq!(tool_output(&sent[2])[0].0, "toolu_5");
        assert_valid(sent);
        assert_valid(agent.messages());
        assert_eq!(compacted, [(13, 5)]);
        assert!(entries_of(&journal).contains(&Entry::Compact {
            summary: "SUMMARY".into(),
            keep_from: 9,
            keep_thinking: false,
        }));
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    fn overflow() -> Reply {
        Err(ProviderError::Api {
            status: 400,
            kind: Some("invalid_request_error".into()),
            message: "prompt is too long: 215000 tokens > 200000 maximum".into(),
        })
    }

    /// A first turn with four tool calls, then the given replies.
    fn after_a_tool_turn(rest: Vec<Reply>) -> Vec<Reply> {
        let mut script = tool_rounds(4, 100);
        script.push(sized(text("one"), 100));
        script.extend(rest);
        script
    }

    #[tokio::test]
    async fn a_context_overflow_compacts_and_retries_once() {
        let provider = Scripted::new(after_a_tool_turn(vec![
            overflow(),
            text("SUMMARY"),
            text("two"),
        ]));
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        agent.run("first", |_| {}).await.unwrap();
        let mut events = Vec::new();
        let answer = agent
            .run("second", |event| events.push(format!("{event:?}")))
            .await
            .unwrap();
        assert_eq!(answer, "two");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 8);
        assert_eq!(seen[5].len(), 11);
        // Old tool output went first, then everything before "second".
        assert!(seen[6][0].text().contains(session::CLEARED));
        let retried = &seen[7];
        assert_eq!(retried.len(), 1);
        assert_eq!(summary_text(&retried[0]).as_deref(), Some("SUMMARY"));
        assert_eq!(
            retried[0].content[1],
            ContentBlock::Text {
                text: "second".into()
            }
        );
        assert!(events[0].starts_with("Compacted"), "{events:?}");
        assert!(events[1].contains("retrying once"), "{events:?}");
        assert_valid(agent.messages());
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    /// Momus's reproduction: one input, five tool rounds under the
    /// threshold, then the API reports an overflow. The turn is compacted
    /// inside itself and the retry goes through.
    #[tokio::test]
    async fn an_overflow_in_a_single_turn_compacts_and_retries() {
        let mut script = tool_rounds(5, 100);
        script.extend([overflow(), text("SUMMARY"), text("done")]);
        let provider = Scripted::new(script);
        let (seen, settings) = (provider.seen.clone(), provider.settings.clone());
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        assert_eq!(agent.run("go", |_| {}).await.unwrap(), "done");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 8);
        assert_eq!(settings.lock().unwrap()[6].0, context::SUMMARY_SYSTEM);
        let retried = &seen[7];
        assert_eq!(summary_text(&retried[0]).as_deref(), Some("SUMMARY"));
        assert_eq!(
            retried[0].content[1],
            ContentBlock::Text { text: "go".into() }
        );
        assert_eq!(retried.len(), 5);
        assert_valid(retried);
        assert_valid(agent.messages());
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    /// The overflow comes from the results of one reply's parallel calls:
    /// there is nothing older to clear or compact, so those results are
    /// cleared and the retry goes through.
    #[tokio::test]
    async fn an_overflow_from_the_latest_tool_results_clears_them() {
        let calls = Ok(CompletionResponse {
            content: ["a", "b"]
                .map(|id| ContentBlock::ToolUse {
                    id: id.into(),
                    name: "upper".into(),
                    input: json!({"text": "a big file"}),
                })
                .to_vec(),
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        });
        let provider = Scripted::new(vec![calls, overflow(), text("done")]);
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let mut agent = agent(provider, 10).with_journal(Box::new(journal.clone()));
        assert_eq!(agent.run("read both", |_| {}).await.unwrap(), "done");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(tool_output(&seen[1][2])[0].1, "A BIG FILE");
        let retried = &seen[2];
        assert_eq!(
            tool_output(&retried[2]),
            [
                ("a", session::CLEARED, false),
                ("b", session::CLEARED, false)
            ]
        );
        assert_valid(retried);
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    /// A provider whose calls never finish.
    struct Hang;

    #[async_trait]
    impl Provider for Hang {
        async fn complete(
            &self,
            _request: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, ProviderError> {
            std::future::pending().await
        }
    }

    /// Sets `cancel` once `pid_file` holds a pid, and returns the pid.
    fn cancel_when_started(
        cancel: Cancel,
        pid_file: std::path::PathBuf,
    ) -> tokio::task::JoinHandle<u32> {
        tokio::spawn(async move {
            loop {
                let text = std::fs::read_to_string(&pid_file).unwrap_or_default();
                if let Ok(pid) = text.trim().parse() {
                    cancel.cancel();
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
    }

    #[tokio::test]
    async fn cancelling_a_running_tool_stops_it_and_the_next_input_works() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let provider = Scripted::new(vec![
            Ok(CompletionResponse {
                content: vec![
                    ContentBlock::ToolUse {
                        id: "slow".into(),
                        name: "bash".into(),
                        input: json!({ "command": command }),
                    },
                    ContentBlock::ToolUse {
                        id: "next".into(),
                        name: "count".into(),
                        input: json!({}),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            }),
            text("fine"),
        ]);
        let seen = provider.seen.clone();
        let journal = Memory::default();
        let (agent, calls) = counting_agent(provider);
        let cancel = Cancel::new();
        let mut agent = agent
            .with_journal(Box::new(journal.clone()))
            .with_cancel(cancel.clone());
        agent.tools.register(crate::tools::Bash::default());
        let canceller = cancel_when_started(cancel.clone(), pid_file);

        let err = agent.run("go", |_| {}).await.unwrap_err();
        assert_eq!(err.downcast_ref::<Cancelled>(), Some(&Cancelled));
        let pid = canceller.await.unwrap();
        assert!(
            crate::tools::testing::gone_soon(pid).await,
            "the tool's child {pid} survived"
        );
        // The call after the cancelled one never ran; both are answered.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            tool_output(&agent.messages()[2]),
            [("slow", CANCELLED, true), ("next", CANCELLED, true)]
        );
        assert_valid(agent.messages());

        cancel.reset();
        assert_eq!(agent.run("next", |_| {}).await.unwrap(), "fine");
        let sent = &seen.lock().unwrap()[1];
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[2].content.last(),
            Some(&ContentBlock::Text {
                text: "next".into()
            })
        );
        assert_valid(sent);
        assert_eq!(replay(entries_of(&journal)), agent.transcript);
    }

    #[tokio::test]
    async fn cancelling_a_model_call_leaves_the_input_for_the_next_one() {
        let cancel = Cancel::new();
        let mut agent = Agent::new(
            Box::new(Hang),
            ToolRegistry::new(),
            agent(Scripted::default(), 5).options.clone(),
        )
        .with_cancel(cancel.clone());
        let setter = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            setter.cancel();
        });
        let err = agent.run("go", |_| {}).await.unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err}");
        assert_eq!(agent.messages(), [Message::user_text("go")]);
        // Still set: the next run stops before calling the model.
        let err = agent.run("again", |_| {}).await.unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err}");
        assert_eq!(agent.messages()[0].text(), "go\nagain");
    }

    #[tokio::test]
    async fn a_second_overflow_is_an_error() {
        let provider = Scripted::new(after_a_tool_turn(vec![
            overflow(),
            text("SUMMARY"),
            overflow(),
        ]));
        let seen = provider.seen.clone();
        let mut agent = agent(provider, 10);
        agent.run("first", |_| {}).await.unwrap();
        let err = agent.run("second", |_| {}).await.unwrap_err();
        assert!(
            err.downcast_ref::<ProviderError>()
                .unwrap()
                .is_context_overflow()
        );
        // One retry only; the compacted history stays and ends with the
        // input, so the next one joins it.
        assert_eq!(seen.lock().unwrap().len(), 8);
        assert_eq!(agent.messages().len(), 1);
        assert_eq!(agent.messages()[0].role, Role::User);
    }

    /// Turns down calls whose input says `"deny": true`; never answers
    /// when it says `"hang": true`.
    struct TestGate(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl ToolGate for TestGate {
        async fn check(&mut self, tool: &str, input: &Value) -> Option<ToolOutput> {
            self.0.lock().unwrap().push(tool.to_string());
            if input["hang"] == true {
                std::future::pending::<()>().await;
            }
            (input["deny"] == true).then(|| ToolOutput::error("the user declined: not now"))
        }
    }

    #[tokio::test]
    async fn a_gate_decides_which_calls_run() {
        let provider = Scripted::new(vec![
            Ok(CompletionResponse {
                content: vec![
                    ContentBlock::ToolUse {
                        id: "no".into(),
                        name: "count".into(),
                        input: json!({"deny": true}),
                    },
                    ContentBlock::ToolUse {
                        id: "yes".into(),
                        name: "count".into(),
                        input: json!({}),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            }),
            text("ok"),
        ]);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (agent, calls) = counting_agent(provider);
        let mut agent = agent.with_gate(Box::new(TestGate(asked.clone())));
        assert_eq!(agent.run("go", |_| {}).await.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*asked.lock().unwrap(), ["count", "count"]);
        assert_eq!(
            tool_output(&agent.messages()[2]),
            [
                ("no", "the user declined: not now", true),
                ("yes", "counted", false)
            ]
        );
        assert_valid(agent.messages());
    }

    #[tokio::test]
    async fn a_cancel_while_the_gate_waits_cancels_the_turn() {
        let provider = Scripted::new(vec![Ok(CompletionResponse {
            content: vec![
                ContentBlock::ToolUse {
                    id: "waiting".into(),
                    name: "count".into(),
                    input: json!({"hang": true}),
                },
                ContentBlock::ToolUse {
                    id: "next".into(),
                    name: "count".into(),
                    input: json!({}),
                },
            ],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        })]);
        let cancel = Cancel::new();
        let (agent, calls) = counting_agent(provider);
        let mut agent = agent
            .with_cancel(cancel.clone())
            .with_gate(Box::new(TestGate(Arc::default())));
        let setter = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            setter.cancel();
        });
        let err = agent.run("go", |_| {}).await.unwrap_err();
        assert_eq!(err.downcast_ref::<Cancelled>(), Some(&Cancelled));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            tool_output(&agent.messages()[2]),
            [("waiting", CANCELLED, true), ("next", CANCELLED, true)]
        );
        assert_valid(agent.messages());
    }
}
