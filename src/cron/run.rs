//! Running one job's prompt: the unattended path of `junees -p` (no
//! approval questions, the command policy applies), in a new session, with
//! the memory and the skills to read only, and a time and token limit.
//! Webhook runs (`crate::webhook`) take the same path with a [`Spec`] of
//! their own.
//!
//! The tools work in the process's current folder, so a run happens in a
//! process of its own (`junees cron run <id>`), which moves to the job's
//! folder first.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use super::store::{Job, RunStatus};
use crate::agent::{Agent, AgentOptions, Cancelled, Event, LimitReached, TurnLimit};
use crate::cancel::Cancel;
use crate::claude_code::ClaudeCode;
use crate::claude_code::chat::Chat;
use crate::config::Config;
use crate::gateway::engine::ProviderFactory;
use crate::mcp::Mcp;
use crate::memory::{self, Memory};
use crate::prompt;
use crate::provider::Provider;
use crate::provider::anthropic::AnthropicProvider;
use crate::session::{self, SessionWriter};
use crate::skills;
use crate::task::budget::{format_duration, tokens};
use crate::tools::{ToolRegistry, ToolSettings};

/// Added to the system prompt of every job.
pub const CRON_NOTE: &str = "\n\nYou are running a scheduled job (`junees cron`), \
    unattended: nobody reads along or answers questions, and tool calls are not asked \
    about. Do the job, then end with the result: your final message is delivered as the \
    job's output. The saved memory and skills can be read but not changed here.";

/// What started an unattended run. It decides the GitHub writes that run
/// without a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger<'a> {
    /// A scheduled job: the user's JUNEES_GITHUB_ALLOW_UNATTENDED.
    Cron,
    /// A request to webhook route `route`, which is anyone's text: only
    /// the writes both its `github_allow` and the user's list allow.
    /// `gh_config` is the run's empty GitHub CLI configuration folder
    /// ([`crate::tools::GhConfigDir`]).
    Webhook {
        route: &'a str,
        github_allow: &'a [crate::github::Write],
        gh_config: &'a Path,
    },
}

impl Trigger<'_> {
    /// The tool settings of a run with this trigger: a webhook run's bash
    /// gets the webhook rules ([`ToolSettings::webhook`]) and the run's
    /// GitHub CLI configuration folder.
    pub fn tool_settings(&self, settings: &ToolSettings) -> ToolSettings {
        match self {
            Trigger::Cron => ToolSettings {
                webhook: false,
                ..settings.clone()
            },
            Trigger::Webhook { gh_config, .. } => ToolSettings {
                webhook: true,
                gh_config_dir: Some(gh_config.to_path_buf()),
                ..settings.clone()
            },
        }
    }
}

/// What one unattended run does.
#[derive(Debug, Clone, Copy)]
pub struct Spec<'a> {
    pub prompt: &'a str,
    /// The folder it works in (the process is already there).
    pub dir: &'a Path,
    /// Its session's [`session::Meta::source`] and channel.
    pub source: &'a str,
    pub channel: &'a str,
    /// Added to the system prompt ([`CRON_NOTE`] for a job).
    pub note: &'a str,
    pub trigger: Trigger<'a>,
}

impl Job {
    /// The run of this job.
    pub fn spec(&self) -> Spec<'_> {
        Spec {
            prompt: &self.prompt,
            dir: &self.dir,
            source: session::CRON,
            channel: &self.id,
            note: CRON_NOTE,
            trigger: Trigger::Cron,
        }
    }
}

/// Limits of one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_time: Duration,
    /// Tokens the run may use (everything read and written, as in task
    /// budgets); `0` for no limit.
    pub max_tokens: u64,
}

/// How a run went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: RunStatus,
    /// The final answer; on a failure the text the model got to, if any.
    pub answer: String,
    pub note: String,
    pub session: Option<String>,
    pub tokens: u64,
}

impl Outcome {
    pub fn failed(note: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed,
            answer: String::new(),
            note: note.into(),
            session: None,
            tokens: 0,
        }
    }
}

/// What runs a job's prompt.
#[async_trait(?Send)]
pub trait Engine {
    /// Runs `spec` in the current folder until done, `cancel`, or `limits`.
    async fn run(&self, spec: &Spec<'_>, cancel: &Cancel, limits: Limits) -> Outcome;
}

/// Keeps the text after the last tool call: the final answer.
#[derive(Default)]
struct Answer {
    blocks: Vec<String>,
    current: String,
}

impl Answer {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::TextDelta(text) => self.current.push_str(text),
            Event::TextEnd => self.blocks.push(std::mem::take(&mut self.current)),
            Event::Retrying => self.current.clear(),
            Event::ToolCall { .. } => {
                self.blocks.clear();
                self.current.clear();
            }
            _ => {}
        }
    }

    fn text(mut self) -> String {
        if !self.current.trim().is_empty() {
            self.blocks.push(self.current);
        }
        self.blocks.join("\n\n").trim().to_string()
    }
}

/// Sets `cancel` once `max_time` has passed; `fired` says it did.
struct Deadline {
    timer: tokio::task::JoinHandle<()>,
    fired: Arc<AtomicBool>,
}

impl Deadline {
    fn start(cancel: &Cancel, max_time: Duration) -> Self {
        let fired = Arc::new(AtomicBool::new(false));
        let (cancel, flag) = (cancel.clone(), Arc::clone(&fired));
        let timer = tokio::spawn(async move {
            tokio::time::sleep(max_time).await;
            flag.store(true, Ordering::SeqCst);
            cancel.cancel();
        });
        Self { timer, fired }
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

/// Why a run ended, from its error.
fn failure_note(err: &anyhow::Error, deadline: &Deadline, limits: Limits) -> String {
    if err.downcast_ref::<Cancelled>().is_some() {
        if deadline.fired() {
            format!("time limit reached ({})", format_duration(limits.max_time))
        } else {
            "interrupted".to_string()
        }
    } else if let Some(limit) = err.downcast_ref::<LimitReached>() {
        limit.0.clone()
    } else if let Some(turns) = err.downcast_ref::<TurnLimit>() {
        turns.to_string()
    } else {
        format!("{err:#}")
    }
}

/// Junees's own agent on the API.
pub struct ApiEngine {
    config: Arc<Config>,
    provider: ProviderFactory,
    /// Tests only: bash runs in the run's folder, in a sandbox there
    /// ([`crate::tools::Bash::sandboxed`]). A real run's process is
    /// already in that folder; a test's is in the crate.
    #[cfg(test)]
    sandbox: bool,
}

impl ApiEngine {
    pub fn new(config: Config) -> Self {
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
            provider,
            #[cfg(test)]
            sandbox: false,
        }
    }

    /// Runs bash in the run's folder with its user state there (tests).
    #[cfg(test)]
    pub fn sandboxed(mut self) -> Self {
        self.sandbox = true;
        self
    }

    /// Uses `provider` for the model calls (tests).
    pub fn with_provider(mut self, provider: ProviderFactory) -> Self {
        self.provider = provider;
        self
    }

    fn agent(&self, spec: &Spec<'_>, mcp: &Mcp, cancel: &Cancel, limits: Limits) -> Result<Agent> {
        let config = &self.config;
        let dir = spec.dir;
        let home = config.home.as_deref();
        let memory = home.map(|home| Memory::for_folder(home, dir, config.memory));
        let settings = spec.trigger.tool_settings(&config.tools);
        let mut tools = ToolRegistry::with_defaults(&settings);
        #[cfg(test)]
        if self.sandbox {
            tools.register(crate::tools::Bash::with_settings(&settings).sandboxed(dir));
        }
        skills::register(&mut tools, home, false);
        mcp.register(&mut tools);
        match spec.trigger {
            Trigger::Cron => crate::github::register(&mut tools, &config.github, dir, false),
            Trigger::Webhook {
                route,
                github_allow,
                ..
            } => crate::github::register_webhook(
                &mut tools,
                &config.github,
                dir,
                route,
                github_allow,
            ),
        }
        let system = prompt::system_prompt(dir)
            + spec.note
            + crate::subagent::prompt_note(config.subagents)
            + &prompt::data_sections(
                memory.as_ref(),
                memory::Mode::ReadOnly,
                home,
                skills::Mode::ReadOnly,
            );
        let started = Instant::now();
        let mut agent = Agent::new(
            (self.provider)()?,
            tools,
            AgentOptions {
                system,
                max_tokens: config.max_tokens,
                max_turns: config.max_turns,
                context_window: config.context_window,
                compact_threshold: config.compact_threshold,
            },
        )
        .with_cancel(cancel.clone())
        .with_limit(Box::new(move |usage| {
            let used = tokens(usage);
            if limits.max_tokens > 0 && used >= limits.max_tokens {
                return Some(format!(
                    "token limit reached: {used} of {} tokens",
                    limits.max_tokens
                ));
            }
            (started.elapsed() >= limits.max_time)
                .then(|| format!("time limit reached ({})", format_duration(limits.max_time)))
        }));
        if let Some(settings) = config.subagents {
            agent = agent.with_delegate(settings);
        }
        Ok(agent)
    }
}

#[async_trait(?Send)]
impl Engine for ApiEngine {
    async fn run(&self, spec: &Spec<'_>, cancel: &Cancel, limits: Limits) -> Outcome {
        let config = &self.config;
        let mcp = Mcp::start(config.home.as_deref(), &config.tools.hidden_env, spec.dir).await;
        for line in mcp.report() {
            eprintln!("{line}");
        }
        let mut agent = match self.agent(spec, &mcp, cancel, limits) {
            Ok(agent) => agent,
            Err(err) => {
                mcp.shutdown().await;
                return Outcome::failed(format!("{err:#}"));
            }
        };
        let mut session = None;
        if let Some(home) = config.home.as_deref() {
            match SessionWriter::create_for_channel(
                home,
                spec.dir,
                &config.model,
                None,
                spec.source,
                spec.channel,
            ) {
                Ok((meta, writer)) => {
                    session = Some(meta.id);
                    agent = agent.with_journal(Box::new(writer));
                }
                Err(err) => eprintln!("  !! the run is not saved as a session: {err:#}"),
            }
        }
        let deadline = Deadline::start(cancel, limits.max_time);
        let mut answer = Answer::default();
        let result = agent.run(spec.prompt, |event| answer.event(event)).await;
        mcp.shutdown().await;
        let used = tokens(agent.usage());
        match result {
            Ok(text) => Outcome {
                status: RunStatus::Ok,
                answer: text.trim().to_string(),
                note: String::new(),
                session,
                tokens: used,
            },
            Err(err) => Outcome {
                status: RunStatus::Failed,
                answer: answer.text(),
                note: failure_note(&err, &deadline, limits),
                session,
                tokens: used,
            },
        }
    }
}

/// Claude Code on the subscription (the claude-code backend). Its tokens
/// are counted after the run; the limit cannot stop it midway (its own
/// turn limit, `max_turns`, and the time limit do).
pub struct ClaudeEngine {
    claude: ClaudeCode,
    config: Arc<Config>,
}

impl ClaudeEngine {
    /// `claude` should have passed [`ClaudeCode::check`] already.
    pub fn new(claude: ClaudeCode, config: Config) -> Self {
        Self {
            claude,
            config: Arc::new(config),
        }
    }
}

#[async_trait(?Send)]
impl Engine for ClaudeEngine {
    async fn run(&self, spec: &Spec<'_>, cancel: &Cancel, limits: Limits) -> Outcome {
        let config = &self.config;
        let home = config.home.as_deref();
        let mut chat = Chat::new(
            self.claude.clone(),
            spec.dir.to_path_buf(),
            spec.trigger.tool_settings(&config.tools),
            config.max_turns,
            cancel.clone(),
        );
        let memory = home.map(|home| Memory::for_folder(home, spec.dir, config.memory));
        chat.set_system(prompt::claude_append(
            spec.note.trim_start().to_string(),
            memory.as_ref(),
            home,
        ));
        let mut session = None;
        if let Some(home) = home {
            let model = self.claude.model.clone().unwrap_or_default();
            match SessionWriter::create_for_channel(
                home,
                spec.dir,
                &model,
                Some(session::CLAUDE_CODE),
                spec.source,
                spec.channel,
            ) {
                Ok((meta, writer)) => {
                    session = Some(meta.id);
                    chat.reset(Some(Box::new(writer)));
                }
                Err(err) => eprintln!("  !! the run is not saved as a session: {err:#}"),
            }
        }
        let deadline = Deadline::start(cancel, limits.max_time);
        let mut answer = Answer::default();
        let result = chat.turn(spec.prompt, |event| answer.event(event)).await;
        let used = tokens(chat.usage());
        let mut outcome = Outcome {
            status: RunStatus::Ok,
            answer: answer.text(),
            note: String::new(),
            session,
            tokens: used,
        };
        if let Err(err) = result {
            outcome.status = RunStatus::Failed;
            outcome.note = failure_note(&err, &deadline, limits);
        } else if limits.max_tokens > 0 && used > limits.max_tokens {
            outcome.note = format!(
                "used {used} tokens, over the limit of {}",
                limits.max_tokens
            );
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::cron::schedule::Schedule;
    use crate::cron::store::tests::job;
    use crate::message::{ContentBlock, StopReason, Usage};
    use crate::provider::{CompletionRequest, CompletionResponse, ProviderError};

    /// The system prompt and tool names of each request.
    type Seen = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    /// Replies in order; records each request's system prompt and tools.
    struct Scripted {
        replies: Mutex<VecDeque<CompletionResponse>>,
        seen: Seen,
        delay: Duration,
    }

    #[async_trait]
    impl Provider for Scripted {
        async fn complete(
            &self,
            request: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, ProviderError> {
            self.seen.lock().unwrap().push((
                request.system.to_string(),
                request.tools.iter().map(|tool| tool.name.clone()).collect(),
            ));
            tokio::time::sleep(self.delay).await;
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("a scripted reply"))
        }
    }

    fn reply(content: Vec<ContentBlock>, stop_reason: StopReason) -> CompletionResponse {
        CompletionResponse {
            content,
            stop_reason,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
                ..Usage::default()
            },
        }
    }

    fn engine(home: &Path, replies: Vec<CompletionResponse>, delay: Duration) -> (ApiEngine, Seen) {
        let mut config = Config::from_lookup(|key| match key {
            "ANTHROPIC_API_KEY" => Some("sk-test".into()),
            "JUNEES_HOME" => Some(home.display().to_string()),
            _ => None,
        })
        .unwrap();
        config.home = Some(home.to_path_buf());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(Some(replies)));
        let record = Arc::clone(&seen);
        let provider: ProviderFactory = Arc::new(move || {
            Ok(Box::new(Scripted {
                replies: Mutex::new(replies.lock().unwrap().take().unwrap_or_default().into()),
                seen: Arc::clone(&record),
                delay,
            }) as Box<dyn Provider>)
        });
        (
            ApiEngine::new(config).with_provider(provider).sandboxed(),
            seen,
        )
    }

    fn limits() -> Limits {
        Limits {
            max_time: Duration::from_secs(60),
            max_tokens: 0,
        }
    }

    #[tokio::test]
    async fn runs_the_prompt_in_a_new_session_with_memory_and_skills_read_only() {
        let home = tempfile::tempdir().unwrap();
        let mut job = job("0123abcd", Schedule::Once { at: 1 }, None);
        job.dir = home.path().to_path_buf();
        let (engine, seen) = engine(
            home.path(),
            vec![reply(
                vec![ContentBlock::text("오늘의 요약")],
                StopReason::EndTurn,
            )],
            Duration::ZERO,
        );
        let outcome = engine.run(&job.spec(), &Cancel::new(), limits()).await;
        assert_eq!(outcome.status, RunStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.answer, "오늘의 요약");
        assert_eq!(outcome.tokens, 110);
        let session = outcome.session.unwrap();
        let path = session::sessions_dir(home.path()).join(format!("{session}.jsonl"));
        let meta = session::read_meta(&path).unwrap();
        // Marked like a channel session, so `-c` and `/sessions` skip it.
        assert_eq!(meta.source.as_deref(), Some(session::CRON));
        assert_eq!(meta.channel.as_deref(), Some("0123abcd"));
        assert_eq!(
            session::find_latest(home.path(), &job.dir, None).unwrap(),
            None
        );
        assert!(session::list_recent(home.path(), 10).unwrap().is_empty());
        let shown = session::list_channel_sessions(home.path(), 10).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].id, session);
        let seen = seen.lock().unwrap();
        let (system, tools) = &seen[0];
        assert!(system.contains("scheduled job"), "{system}");
        assert!(tools.contains(&"skill_view".to_string()), "{tools:?}");
        for refused in ["memory", "skill_manage"] {
            assert!(!tools.contains(&refused.to_string()), "{tools:?}");
        }
    }

    #[tokio::test]
    async fn the_token_limit_stops_the_run_and_keeps_the_text_so_far() {
        let home = tempfile::tempdir().unwrap();
        let mut job = job("0123abcd", Schedule::Once { at: 1 }, None);
        job.dir = home.path().to_path_buf();
        let call = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "/nonexistent"}),
        };
        let (engine, _) = engine(
            home.path(),
            vec![reply(
                vec![ContentBlock::text("looking"), call],
                StopReason::ToolUse,
            )],
            Duration::ZERO,
        );
        let outcome = engine
            .run(
                &job.spec(),
                &Cancel::new(),
                Limits {
                    max_tokens: 100,
                    ..limits()
                },
            )
            .await;
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(outcome.note, "token limit reached: 110 of 100 tokens");
        // The text before the tool call is not the answer.
        assert_eq!(outcome.answer, "");
    }

    #[tokio::test]
    async fn the_time_limit_cancels_a_slow_run() {
        let home = tempfile::tempdir().unwrap();
        let mut job = job("0123abcd", Schedule::Once { at: 1 }, None);
        job.dir = home.path().to_path_buf();
        let (late, _) = engine(
            home.path(),
            vec![reply(vec![ContentBlock::text("late")], StopReason::EndTurn)],
            Duration::from_secs(30),
        );
        let start = Instant::now();
        let outcome = late
            .run(
                &job.spec(),
                &Cancel::new(),
                Limits {
                    max_time: Duration::from_millis(200),
                    ..limits()
                },
            )
            .await;
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(outcome.note, "time limit reached (0s)");
        // A cancel from outside is an interruption.
        let (slow, _) = engine(home.path(), vec![], Duration::from_secs(30));
        let cancel = Cancel::new();
        cancel.cancel();
        let outcome = slow.run(&job.spec(), &cancel, limits()).await;
        assert_eq!(outcome.note, "interrupted");
    }

    #[tokio::test]
    async fn webhook_runs_refuse_git_push_in_bash() {
        let home = tempfile::tempdir().unwrap();
        let call = |id: &str| ContentBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            // A remote that does not exist: harmless should the rule fail.
            input: serde_json::json!({"command": "git push /nonexistent-junees-remote HEAD:main"}),
        };
        let replies = || {
            vec![
                reply(vec![call("t1")], StopReason::ToolUse),
                reply(vec![ContentBlock::text("done")], StopReason::EndTurn),
            ]
        };
        let allow: &[crate::github::Write] = &[];
        let webhook = Spec {
            prompt: "push it",
            dir: home.path(),
            source: session::WEBHOOK,
            channel: "flow",
            note: "",
            trigger: Trigger::Webhook {
                route: "flow",
                github_allow: allow,
                gh_config: Path::new("/run/gh-config"),
            },
        };
        let (engine, _) = self::engine(home.path(), replies(), Duration::ZERO);
        let outcome = engine.run(&webhook, &Cancel::new(), limits()).await;
        assert_eq!(outcome.status, RunStatus::Ok, "{outcome:?}");
        let session = outcome.session.unwrap();
        let path = session::sessions_dir(home.path()).join(format!("{session}.jsonl"));
        let journal = std::fs::read_to_string(path).unwrap();
        assert!(
            journal.contains(
                "blocked by policy: `git push` is not allowed: this run was started by a webhook"
            ),
            "{journal}"
        );
        // A scheduled job's tools keep the user's settings.
        let settings = ToolSettings::default();
        let webhook = Trigger::Webhook {
            route: "flow",
            github_allow: allow,
            gh_config: Path::new("/run/gh-config"),
        }
        .tool_settings(&settings);
        assert!(webhook.webhook);
        assert_eq!(
            webhook.gh_config_dir.as_deref(),
            Some(Path::new("/run/gh-config"))
        );
        let cron = Trigger::Cron.tool_settings(&settings);
        assert!(!cron.webhook);
        assert_eq!(cron.gh_config_dir, None);
    }

    #[test]
    fn the_answer_is_the_text_after_the_last_tool_call() {
        let mut answer = Answer::default();
        answer.event(Event::TextDelta("first"));
        answer.event(Event::TextEnd);
        let input = serde_json::json!({});
        answer.event(Event::ToolCall {
            name: "bash",
            input: &input,
        });
        answer.event(Event::TextDelta("dropped"));
        answer.event(Event::Retrying);
        answer.event(Event::TextDelta("final "));
        answer.event(Event::TextDelta("one"));
        answer.event(Event::TextEnd);
        answer.event(Event::TextDelta("tail"));
        assert_eq!(answer.text(), "final one\n\ntail");
    }
}
