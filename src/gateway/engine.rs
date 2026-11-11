//! What answers in a channel: Junees's own agent on the API, or Claude
//! Code on the subscription (the `backend` setting), behind one
//! [`Conversation`] trait, so the gateway's channel logic is the same for
//! both and tests can put a scripted conversation in.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::{Agent, AgentOptions, Event};
use crate::approval::{Approval, ApprovalMode, Ask};
use crate::cancel::Cancel;
use crate::claude_code::ClaudeCode;
use crate::claude_code::chat::Chat;
use crate::config::Config;
use crate::mcp::Mcp;
use crate::memory::{Memory, MemoryTool, Mode};
use crate::plugins::Plugins;
use crate::prompt;
use crate::provider::Provider;
use crate::provider::anthropic::AnthropicProvider;
use crate::session::{self, Journal, Transcript};
use crate::skills;
use crate::tools::{ToolRegistry, ToolSettings, Workspace};

/// Characters of a tool call's input shown in its one-line summary.
const PREVIEW_CHARS: usize = 120;

/// Added to the system prompt of every gateway conversation.
pub const GATEWAY_NOTE: &str = "\n\nYou are talking with the user through Discord, not a \
    terminal: each reply is posted as Discord messages (Markdown without tables; long \
    replies are cut into messages of 2000 characters). Keep replies short.";

/// What a turn reports while it runs, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// A finished text block of the reply.
    Text(String),
    /// A one-line summary of a tool call (`-> bash ls`).
    Tool(String),
    /// Something the user should know: a cut-off reply, a compaction.
    Note(String),
    /// An approval question (see [`super::ask`]).
    Question(String),
}

/// Turns the agent's events into [`Progress`]: text is held until its
/// block is complete, and dropped when the reply is retried.
pub struct Collector<'a> {
    out: &'a UnboundedSender<Progress>,
    text: String,
}

impl<'a> Collector<'a> {
    pub fn new(out: &'a UnboundedSender<Progress>) -> Self {
        Self {
            out,
            text: String::new(),
        }
    }

    pub fn event(&mut self, event: Event<'_>) {
        let progress = match event {
            Event::TextDelta(text) => {
                self.text.push_str(text);
                return;
            }
            Event::TextEnd => Progress::Text(std::mem::take(&mut self.text)),
            Event::Retrying => {
                self.text.clear();
                return;
            }
            Event::Thinking | Event::ToolResult { .. } => return,
            Event::Warning(warning) => Progress::Note(warning.to_string()),
            Event::ToolCall { name, input } => Progress::Tool(format!(
                "-> {name} {}",
                crate::cli::preview(&tool_target(input), PREVIEW_CHARS)
            )),
            Event::ServerToolUse { name, input } => Progress::Tool(format!(
                "-> {name} {} (run by the API)",
                crate::cli::preview(&tool_target(input), PREVIEW_CHARS)
            )),
            Event::Subagent { index, line } => Progress::Tool(format!("[{index}] {line}")),
            Event::Sources(urls) => Progress::Text(crate::web_search::sources_text(urls)),
            Event::Compacted {
                before_messages,
                after_messages,
            } => Progress::Note(format!(
                "(compacted the conversation: {before_messages} messages -> {after_messages})"
            )),
        };
        // The receiver lives as long as the channel's worker.
        let _ = self.out.send(progress);
    }

    /// Sends the text of a reply that was cut off (by a cancel or an
    /// error) as far as it got.
    pub fn finish(&mut self) {
        if !self.text.trim().is_empty() {
            let _ = self
                .out
                .send(Progress::Text(std::mem::take(&mut self.text)));
        }
    }
}

/// The part of a tool call's input worth a line: the command, the path or
/// the query, else the whole input.
fn tool_target(input: &serde_json::Value) -> String {
    ["command", "path", "query", "name"]
        .iter()
        .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
        .map_or_else(|| input.to_string(), str::to_string)
}

/// One channel's conversation. Its futures are not `Send` (the agent
/// keeps references across awaits), so the gateway runs the channels on
/// one thread of their own ([`tokio::task::LocalSet`]).
#[async_trait(?Send)]
pub trait Conversation {
    /// Runs one input, reporting through `out`. A cancel ends it with
    /// [`crate::agent::Cancelled`].
    async fn turn(&mut self, input: &str, out: &UnboundedSender<Progress>) -> Result<()>;
    /// Continues a saved conversation, recording to `journal`.
    fn resume(&mut self, transcript: Transcript, journal: Option<Box<dyn Journal>>);
    /// Starts over, recording to `journal`; the memory and the skills are
    /// read again.
    fn reset(&mut self, journal: Option<Box<dyn Journal>>);
    /// Tokens used in this session, for `!status`.
    fn usage(&self) -> String;
}

/// Opens a conversation per channel.
pub trait Engines: Send + Sync {
    /// The backend and model, for `!status` and the log.
    fn describe(&self) -> String;
    /// The [`session::Meta::backend`] of the sessions it writes.
    fn session_backend(&self) -> Option<&'static str>;
    /// The model a new session's meta line names.
    fn model(&self) -> String;
    /// Whether risky tool calls are asked about in the channel.
    fn asks(&self) -> bool;
    /// A new conversation that asks `ask` before risky tool calls (if it
    /// asks at all) and stops when `cancel` is set.
    fn open(&self, ask: Box<dyn Ask>, cancel: Cancel) -> Result<Box<dyn Conversation>>;
}

/// Makes the model client of a new conversation.
pub type ProviderFactory = Arc<dyn Fn() -> Result<Box<dyn Provider>> + Send + Sync>;

/// The API backend: Junees's own agent, with the tools, memory, skills and
/// approval questions of the terminal conversation.
pub struct ApiEngines {
    config: Arc<Config>,
    cwd: PathBuf,
    provider: ProviderFactory,
    mcp: Arc<Mcp>,
    plugins: Arc<Plugins>,
}

impl ApiEngines {
    /// The agent on Anthropic's API, as `config` sets it up.
    pub fn new(config: Config, cwd: PathBuf) -> Self {
        let config = Arc::new(config);
        let settings = Arc::clone(&config);
        let provider: ProviderFactory = Arc::new(move || {
            Ok(Box::new(
                AnthropicProvider::new(
                    settings.api_key.clone(),
                    settings.base_url.clone(),
                    settings.model.clone(),
                )?
                .with_thinking(settings.thinking_budget)
                .with_streaming(settings.stream)
                .with_web_search(settings.web_search.clone()),
            ) as Box<dyn Provider>)
        });
        Self {
            config,
            cwd,
            provider,
            mcp: Arc::new(Mcp::none()),
            plugins: Arc::new(Plugins::none()),
        }
    }

    /// Offers the tools of these MCP servers in every channel.
    pub fn with_mcp(mut self, mcp: Arc<Mcp>) -> Self {
        self.mcp = mcp;
        self
    }

    /// Offers the tools, hooks and prompt notes of these plugins in every
    /// channel.
    pub fn with_plugins(mut self, plugins: Arc<Plugins>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Uses `provider` for the model calls (tests).
    pub fn with_provider(mut self, provider: ProviderFactory) -> Self {
        self.provider = provider;
        self
    }
}

impl Engines for ApiEngines {
    fn describe(&self) -> String {
        format!("api backend, model {}", self.config.model)
    }

    fn session_backend(&self) -> Option<&'static str> {
        None
    }

    fn model(&self) -> String {
        self.config.model.clone()
    }

    fn asks(&self) -> bool {
        self.config.approval != ApprovalMode::Off
    }

    fn open(&self, ask: Box<dyn Ask>, cancel: Cancel) -> Result<Box<dyn Conversation>> {
        let config = &self.config;
        let memory = memory(config, &self.cwd);
        // With a gate, Junees's own state-changing commands go to its
        // question; without one bash refuses them.
        let settings = ToolSettings {
            approval_gate: self.asks(),
            ..config.tools.clone()
        };
        let mut tools = ToolRegistry::with_defaults(&settings);
        if let Some(memory) = &memory {
            tools.register(MemoryTool::new(memory.clone(), Mode::Conversation));
        }
        // Channel sessions stay out: a reply in a guild channel is read by
        // more people than the one who asked.
        crate::search::register(&mut tools, config.home.as_deref(), &self.cwd, false);
        skills::register(&mut tools, config.home.as_deref(), true);
        self.mcp.register(&mut tools);
        self.plugins.register(&mut tools);
        crate::github::register(&mut tools, &config.github, &self.cwd, self.asks());
        let mut agent = Agent::new(
            (self.provider)()?,
            tools,
            AgentOptions {
                system: api_system(config, &self.cwd, memory.as_ref(), &self.plugins),
                max_tokens: config.max_tokens,
                max_turns: config.max_turns,
                context_window: config.context_window,
                compact_threshold: config.compact_threshold,
            },
        )
        .with_cancel(cancel);
        if let Some(settings) = config.subagents {
            agent = agent.with_delegate(settings);
        }
        if config.approval != ApprovalMode::Off {
            let gate = Approval::new(
                config.approval,
                &config.tools,
                Workspace::new(&self.cwd).with_temp_dirs(),
            )
            .with_external_tools(self.mcp.external_tools())
            .with_external_tools(self.plugins.external_tools())
            .asking(ask);
            agent = agent.with_gate(Box::new(gate));
        }
        if let Some(after_turn) = self.plugins.after_turn() {
            agent = agent.with_after_turn(after_turn);
        }
        Ok(Box::new(ApiConversation {
            agent,
            config: Arc::clone(config),
            cwd: self.cwd.clone(),
            plugins: Arc::clone(&self.plugins),
        }))
    }
}

fn memory(config: &Config, cwd: &std::path::Path) -> Option<Memory> {
    let home = config.home.as_deref()?;
    Some(Memory::for_folder(home, cwd, config.memory))
}

/// The system prompt of the agent: the terminal conversation's, the
/// gateway note, then the memory and the skills as they are now.
fn api_system(
    config: &Config,
    cwd: &std::path::Path,
    memory: Option<&Memory>,
    plugins: &Plugins,
) -> String {
    prompt::system_prompt(cwd)
        + GATEWAY_NOTE
        + crate::subagent::prompt_note(config.subagents)
        + &prompt::data_sections(
            memory,
            Mode::Conversation,
            config.home.as_deref(),
            skills::Mode::Manage,
        )
        + plugins.prompt_section()
}

struct ApiConversation {
    agent: Agent,
    config: Arc<Config>,
    cwd: PathBuf,
    plugins: Arc<Plugins>,
}

#[async_trait(?Send)]
impl Conversation for ApiConversation {
    async fn turn(&mut self, input: &str, out: &UnboundedSender<Progress>) -> Result<()> {
        let mut collector = Collector::new(out);
        let result = self.agent.run(input, |event| collector.event(event)).await;
        collector.finish();
        result.map(drop)
    }

    fn resume(&mut self, transcript: Transcript, journal: Option<Box<dyn Journal>>) {
        self.agent.resume(transcript, journal);
    }

    fn reset(&mut self, journal: Option<Box<dyn Journal>>) {
        let memory = memory(&self.config, &self.cwd);
        self.agent.set_system(api_system(
            &self.config,
            &self.cwd,
            memory.as_ref(),
            &self.plugins,
        ));
        self.agent.reset(journal);
    }

    fn usage(&self) -> String {
        let usage = self.agent.usage();
        format!(
            "input {} tokens (cache write {}, cache read {}), output {} tokens; context {} / {}",
            usage.input_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens,
            self.agent.context_tokens(),
            self.agent.options().context_window
        )
    }
}

/// The claude-code backend: one Claude Code session per channel, on the
/// user's subscription. Claude Code runs without Junees's approval
/// questions (`-p` has nobody to ask); only the deny rules apply.
pub struct ClaudeEngines {
    claude: ClaudeCode,
    config: Arc<Config>,
    cwd: PathBuf,
}

impl ClaudeEngines {
    /// `claude` should have passed [`ClaudeCode::check`] already.
    pub fn new(claude: ClaudeCode, config: Config, cwd: PathBuf) -> Self {
        Self {
            claude,
            config: Arc::new(config),
            cwd,
        }
    }
}

impl Engines for ClaudeEngines {
    fn describe(&self) -> String {
        format!(
            "claude-code backend, model {}",
            self.claude
                .model
                .as_deref()
                .unwrap_or("of Claude Code's choice")
        )
    }

    fn session_backend(&self) -> Option<&'static str> {
        Some(session::CLAUDE_CODE)
    }

    fn model(&self) -> String {
        self.claude.model.clone().unwrap_or_default()
    }

    fn asks(&self) -> bool {
        false
    }

    fn open(&self, _ask: Box<dyn Ask>, cancel: Cancel) -> Result<Box<dyn Conversation>> {
        let mut chat = Chat::new(
            self.claude.clone(),
            self.cwd.clone(),
            self.config.tools.clone(),
            self.config.max_turns,
            cancel,
        );
        chat.set_system(claude_system(&self.config, &self.cwd));
        Ok(Box::new(ClaudeConversation {
            chat,
            config: Arc::clone(&self.config),
            cwd: self.cwd.clone(),
        }))
    }
}

fn claude_system(config: &Config, cwd: &std::path::Path) -> String {
    prompt::claude_append(
        GATEWAY_NOTE.trim_start().to_string(),
        memory(config, cwd).as_ref(),
        config.home.as_deref(),
    )
}

struct ClaudeConversation {
    chat: Chat,
    config: Arc<Config>,
    cwd: PathBuf,
}

#[async_trait(?Send)]
impl Conversation for ClaudeConversation {
    async fn turn(&mut self, input: &str, out: &UnboundedSender<Progress>) -> Result<()> {
        let mut collector = Collector::new(out);
        let result = self.chat.turn(input, |event| collector.event(event)).await;
        collector.finish();
        result
    }

    fn resume(&mut self, transcript: Transcript, journal: Option<Box<dyn Journal>>) {
        self.chat.resume(transcript, journal);
    }

    fn reset(&mut self, journal: Option<Box<dyn Journal>>) {
        self.chat.set_system(claude_system(&self.config, &self.cwd));
        self.chat.reset(journal);
    }

    fn usage(&self) -> String {
        let usage = self.chat.usage();
        format!(
            "input {} tokens (cache write {}, cache read {}), output {} tokens, {} Claude Code turns",
            usage.input_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens,
            self.chat.turns()
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;
    use crate::tools::ToolOutput;

    #[test]
    fn collects_whole_text_blocks_and_tool_lines() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut collector = Collector::new(&tx);
        collector.event(Event::TextDelta("broken "));
        collector.event(Event::Retrying);
        collector.event(Event::TextDelta("Hel"));
        collector.event(Event::TextDelta("lo"));
        collector.event(Event::TextEnd);
        let input = json!({"command": "ls -la\n  /tmp"});
        collector.event(Event::ToolCall {
            name: "bash",
            input: &input,
        });
        collector.event(Event::ToolResult {
            name: "bash",
            output: &ToolOutput::ok("x"),
        });
        collector.event(Event::Warning("cut off"));
        collector.event(Event::TextDelta("half"));
        collector.finish();
        drop(tx);
        let mut got = Vec::new();
        while let Ok(progress) = rx.try_recv() {
            got.push(progress);
        }
        assert_eq!(
            got,
            [
                Progress::Text("Hello".into()),
                Progress::Tool("-> bash ls -la /tmp".into()),
                Progress::Note("cut off".into()),
                Progress::Text("half".into()),
            ]
        );
    }
}
