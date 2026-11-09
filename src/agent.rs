//! The agent loop: ask the model, run the tools it calls, feed the results
//! back, and repeat until it answers without calling a tool.

use anyhow::{Result, bail};
use serde_json::Value;

use crate::message::{ContentBlock, Message, Role, Usage};
use crate::provider::{CompletionRequest, Provider};
use crate::tools::{ToolOutput, ToolRegistry};

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub system: String,
    pub max_tokens: u32,
    /// Model calls allowed per user input before the loop gives up.
    pub max_turns: usize,
}

/// Progress reported while [`Agent::run`] works.
#[derive(Debug)]
pub enum Event<'a> {
    Text(&'a str),
    ToolCall {
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        name: &'a str,
        output: &'a ToolOutput,
    },
}

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: ToolRegistry,
    options: AgentOptions,
    messages: Vec<Message>,
    usage: Usage,
}

impl Agent {
    pub fn new(provider: Box<dyn Provider>, tools: ToolRegistry, options: AgentOptions) -> Self {
        Self {
            provider,
            tools,
            options,
            messages: Vec::new(),
            usage: Usage::default(),
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Forgets the conversation. Token usage keeps counting.
    pub fn reset(&mut self) {
        self.messages.clear();
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
        self.push_user_text(input);
        let specs = self.tools.specs();

        for _ in 0..self.options.max_turns {
            let response = self
                .provider
                .complete(CompletionRequest {
                    system: &self.options.system,
                    messages: &self.messages,
                    tools: &specs,
                    max_tokens: self.options.max_tokens,
                })
                .await?;
            self.usage.add(response.usage);

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
                self.messages.push(reply);
            }
            if tool_calls.is_empty() {
                return Ok(text);
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
            self.messages.push(Message {
                role: Role::User,
                content: results,
            });
        }

        bail!(
            "stopped after {} model calls without a final answer",
            self.options.max_turns
        )
    }

    fn push_user_text(&mut self, text: &str) {
        let block = ContentBlock::Text {
            text: text.to_string(),
        };
        match self.messages.last_mut() {
            Some(last) if last.role == Role::User => last.content.push(block),
            _ => self.messages.push(Message {
                role: Role::User,
                content: vec![block],
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::message::StopReason;
    use crate::provider::CompletionResponse;
    use crate::tools::Tool;

    /// Replays canned responses and records the history it was sent.
    #[derive(Clone, Default)]
    struct Scripted {
        responses: Arc<Mutex<VecDeque<Result<CompletionResponse>>>>,
        seen: Arc<Mutex<Vec<Vec<Message>>>>,
    }

    impl Scripted {
        fn new(responses: Vec<Result<CompletionResponse>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                seen: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl Provider for Scripted {
        async fn complete(&self, request: CompletionRequest<'_>) -> Result<CompletionResponse> {
            self.seen.lock().unwrap().push(request.messages.to_vec());
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
        fn name(&self) -> &'static str {
            "upper"
        }
        fn description(&self) -> &'static str {
            "uppercases text"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn run(&self, input: Value) -> ToolOutput {
            ToolOutput::ok(input["text"].as_str().unwrap_or_default().to_uppercase())
        }
    }

    fn text(text: &str) -> Result<CompletionResponse> {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 1,
            },
        })
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Result<CompletionResponse> {
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
                output_tokens: 3
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
        let provider = Scripted::new(vec![Err(anyhow::anyhow!("network down")), text("ok")]);
        let seen = provider.seen.clone();
        let mut agent = agent(provider, 5);

        assert!(agent.run("first", |_| {}).await.is_err());
        agent.run("second", |_| {}).await.unwrap();

        let sent = &seen.lock().unwrap()[1];
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text(), "first\nsecond");
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
        agent.reset();
        assert!(agent.messages().is_empty());
    }
}
