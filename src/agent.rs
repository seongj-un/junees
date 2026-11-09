//! The agent loop: ask the model, run the tools it calls, feed the results
//! back, and repeat until it answers without calling a tool.

use anyhow::{Result, bail};
use serde_json::Value;

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
        }
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

        for _ in 0..self.options.max_turns {
            self.manage_context(&mut on_event).await;
            let response = match self.call(&specs).await {
                Ok(response) => response,
                // Shrink the history as far as it goes and try once more.
                Err(err) if err.is_context_overflow() => {
                    if !self.shrink(&mut on_event).await {
                        return Err(err.into());
                    }
                    on_event(Event::Warning(
                        "the conversation did not fit the context window; \
                         retrying once with the shrunk history",
                    ));
                    self.call(&specs).await?
                }
                Err(err) => return Err(err.into()),
            };
            self.record(Entry::Usage(response.usage), &mut on_event);

            let mut tool_calls = Vec::new();
            for block in &response.content {
                match block {
                    ContentBlock::Text { text } => on_event(Event::Text(text)),
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push((id.clone(), name.clone(), input.clone()));
                    }
                    _ => {}
                }
            }

            let reply = Message {
                role: Role::Assistant,
                content: response.content,
            };
            let text = reply.text();
            // The API rejects empty assistant turns in the history.
            if !reply.content.is_empty() {
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
                    on_event(Event::Warning(
                        "the answer hit the output token limit and may be cut off",
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
                on_event(Event::ToolCall {
                    name: &name,
                    input: &input,
                });
                let output = self.tools.dispatch(&name, input).await;
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
        }

        bail!(
            "stopped after {} model calls without a final answer",
            self.options.max_turns
        )
    }

    async fn call(&self, specs: &[ToolSpec]) -> Result<CompletionResponse, ProviderError> {
        self.provider
            .complete(CompletionRequest {
                system: &self.options.system,
                messages: &self.transcript.messages,
                tools: specs,
                max_tokens: self.options.max_tokens,
            })
            .await
    }

    /// After the API said the conversation does not fit: clears old tool
    /// output and compacts, whatever the tracked size says. Returns whether
    /// the history got any smaller.
    async fn shrink(&mut self, on_event: &mut impl FnMut(Event<'_>)) -> bool {
        let cleared = self.clear_tool_results(on_event);
        let compacted = self.summarize(on_event).await;
        self.cleared = false;
        cleared || compacted
    }

    /// Keeps the conversation under the compaction threshold: once the
    /// context size reaches it, old tool output is cleared first. If there
    /// is none to clear, or the next call still measures over the threshold,
    /// the conversation is compacted.
    async fn manage_context(&mut self, on_event: &mut impl FnMut(Event<'_>)) {
        if self.transcript.context_tokens < self.options.compact_limit() {
            self.cleared = false;
            return;
        }
        if !self.cleared && self.clear_tool_results(on_event) {
            self.cleared = true;
            return;
        }
        self.cleared = false;
        self.summarize(on_event).await;
    }

    /// Replaces the earlier part of the conversation with a summary written
    /// by the model, keeping the latest user inputs and what followed them
    /// (or, within one long turn, its input and latest tool rounds).
    /// Returns whether it did; if there is nothing to compact yet or the
    /// summary fails, it says so in a warning and changes nothing.
    pub async fn compact(&mut self, mut on_event: impl FnMut(Event<'_>)) -> bool {
        if self.transcript.compact_point().is_none() {
            on_event(Event::Warning(
                "nothing to compact yet: the conversation is too short to summarize",
            ));
            return false;
        }
        self.summarize(&mut on_event).await
    }

    /// Compacts at [`Transcript::compact_point`], if there is one.
    async fn summarize(&mut self, on_event: &mut impl FnMut(Event<'_>)) -> bool {
        let Some(keep_from) = self.transcript.compact_point() else {
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
        self.record(Entry::Compact { summary, keep_from }, on_event);
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
        match self.transcript.clear_entry() {
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

    /// Replays canned responses and records the history it was sent.
    #[derive(Clone, Default)]
    struct Scripted {
        responses: Arc<Mutex<VecDeque<Reply>>>,
        seen: Arc<Mutex<Vec<Vec<Message>>>>,
        /// System prompt, tool count and max_tokens of each request.
        settings: Arc<Mutex<Vec<(String, usize, u32)>>>,
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
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("no scripted response left")
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
                &Entry::ClearToolResults { upto: 4 },
                &Entry::ClearToolResults { upto: 8 }
            ]
        );
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
        let (system, tools, max_tokens) = &settings[7];
        assert_eq!(system, context::SUMMARY_SYSTEM);
        assert_eq!((*tools, *max_tokens), (0, 4096));
        assert_eq!(settings[8].1, 1);
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
            keep_from: 12
        }));
        assert_eq!(replay(entries), agent.transcript);
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
            keep_from: 9
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
}
