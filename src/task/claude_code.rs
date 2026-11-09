//! The `claude-code` backend: task sessions run by the official Claude Code
//! CLI, signed in with the user's own Claude subscription.
//!
//! Anthropic lets a subscription drive other programs only through the
//! unmodified `claude` program, signed in through Anthropic's own flow. So
//! Junees only starts that program as a child process. It never reads,
//! copies, stores or sends Claude credentials (`~/.claude`,
//! `CLAUDE_CODE_OAUTH_TOKEN`), and its own API client does not pretend to be
//! Claude Code. A `CLAUDE_CODE_OAUTH_TOKEN` in the user's environment
//! reaches `claude` the way every other variable does, and Claude Code is
//! told to keep it from the commands it runs ([`SCRUB_ENV`]); the
//! variables in [`API_ENV_VARS`] do not reach it, because Claude Code
//! would bill them instead of the subscription, and
//! [`ClaudeCode::check_auth`] refuses any other sign-in before each
//! session.
//!
//! A session is one `claude -p` run with `--output-format stream-json` (see
//! [`ClaudeCode::arguments`]). Its events are shown like an agent's (text,
//! `-> tool`, `<- tool`), saved raw to a file, and counted against the
//! budget. Claude Code has no `mark_feature_passing`: after the session the
//! harness runs the verify commands itself (see the runner).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

use super::budget::tokens;
use super::git::REPO_ENV_VARS;
use super::runner::{Report, Reporter};
use crate::agent::{Event, Limit};
use crate::cancel::Cancel;
use crate::cli::preview;
use crate::message::Usage;
use crate::tools::{SECRET_ENV_VARS, ToolOutput, ToolSettings};

/// Picks the backend of `task init` and `task run`: `api` or `claude-code`.
pub const BACKEND_ENV: &str = "JUNEES_BACKEND";
/// The `claude` program to run (default: `claude` from `PATH`).
pub const BIN_ENV: &str = "JUNEES_CLAUDE_BIN";
/// The model Claude Code uses (`--model`); Claude Code's own choice if unset.
pub const MODEL_ENV: &str = "JUNEES_CLAUDE_MODEL";
/// Claude Code's own switch that keeps credentials, `CLAUDE_CODE_OAUTH_TOKEN`
/// included, out of the commands it runs
/// (<https://code.claude.com/docs/en/env-vars#what-the-subprocess-environment-scrub-removes>).
/// Junees turns it on for every session unless the user turned it off.
pub const SCRUB_ENV: &str = "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB";

/// The oldest Claude Code that knows every flag Junees passes:
/// `--permission-prompts` came in 2.1.259.
pub const MIN_VERSION: [u32; 3] = [2, 1, 259];

/// What a user whose Claude Code is not signed in has to do. Junees never
/// signs in for them: that has to go through Anthropic's own flow.
pub const LOGIN_HINT: &str = "sign in to Claude Code yourself: run `claude` and then /login \
                              (or create a token with `claude setup-token`), then start again";

/// The built-in tools a session gets, all of them allowed without asking.
pub const TOOLS: &str = "Read,Edit,Write,Glob,Grep,Bash";

/// Variables that would make Claude Code bill something other than the
/// subscription, so they never reach `claude` (nor do the secrets bash
/// never sees and the repository variables). By the authentication
/// precedence (<https://code.claude.com/docs/en/authentication#authentication-precedence>),
/// these rank above the `/login` subscription: a cloud provider
/// (`CLAUDE_CODE_USE_*`, 1), `ANTHROPIC_AUTH_TOKEN` (2), `ANTHROPIC_API_KEY`
/// (3), and a named Anthropic profile or federation credentials (6). The
/// rest are the endpoint and the provider keys that go with them
/// (<https://code.claude.com/docs/en/env-vars>). An `apiKeyHelper` (4) and
/// an `env` block in Claude Code's own settings are no variables of ours:
/// [`ClaudeCode::check_auth`] catches those.
pub const API_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_MANTLE",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "ANTHROPIC_FEDERATION_RULE_ID",
    "ANTHROPIC_ORGANIZATION_ID",
];

/// The `authMethod`s of `claude auth status` that use the subscription:
/// a `/login` (`claude.ai`) or `CLAUDE_CODE_OAUTH_TOKEN` (`oauth_token`).
/// The others are `none`, `api_key`, `api_key_helper` and `third_party`
/// (<https://code.claude.com/docs/en/cli-reference>).
pub const SUBSCRIPTION_AUTH: &[&str] = &["claude.ai", "oauth_token"];

/// How long Claude Code gets to end its turn after SIGINT before its
/// process group is killed.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10);

const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long output is still read after `claude` exits: a process it left
/// running in the background may hold the pipes open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How often the budget is asked while a session runs, for the time limit.
const BUDGET_TICK: Duration = Duration::from_secs(1);
const STDERR_TAIL_CHARS: usize = 2000;
const ERROR_CHARS: usize = 500;
/// Error codes of Claude Code's stream that mean its login or account was
/// refused (<https://code.claude.com/docs/en/headless>, `api_retry`).
const AUTH_ERRORS: &[&str] = &[
    "authentication_failed",
    "oauth_org_not_allowed",
    "account_on_hold",
];
/// The error code of Claude Code's stream for a 429 against the account's
/// quota: the usage limit of the subscription
/// (<https://code.claude.com/docs/en/agent-sdk/typescript>,
/// `SDKAssistantMessageError`; also an `api_retry` category,
/// <https://code.claude.com/docs/en/headless>).
const RATE_LIMIT_ERROR: &str = "rate_limit";
/// Texts of Claude Code's login failures (lowercase), for output without
/// an error code.
const LOGIN_TEXTS: &[&str] = &[
    "not logged in",
    "please run /login",
    "failed to authenticate",
    "login expired",
    "invalid api key",
];

/// Which program runs the sessions of `task init` and `task run`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    /// Junees's own agent on the Anthropic API (`ANTHROPIC_API_KEY`).
    #[default]
    Api,
    /// The Claude Code CLI, signed in by the user.
    ClaudeCode,
}

impl Backend {
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim() {
            "api" => Ok(Self::Api),
            "claude-code" => Ok(Self::ClaudeCode),
            other => bail!("unknown backend `{other}`; use `api` or `claude-code`"),
        }
    }

    /// [`BACKEND_ENV`], `api` when unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        match lookup(BACKEND_ENV).filter(|value| !value.trim().is_empty()) {
            None => Ok(Self::Api),
            Some(value) => Self::parse(&value).context(BACKEND_ENV),
        }
    }
}

/// How to run Claude Code. Holds no credential: `claude` finds its own
/// login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCode {
    pub bin: PathBuf,
    pub model: Option<String>,
    /// See [`DEFAULT_GRACE`].
    pub grace: Duration,
    /// Whether sessions run with [`SCRUB_ENV`]` = 1`: false only when the
    /// user set it to an off value, which then reaches `claude` unchanged.
    pub scrub: bool,
}

/// One session for [`ClaudeCode::run`].
pub struct Request<'a> {
    /// The repository root, where `claude` runs.
    pub root: &'a Path,
    /// Appended to Claude Code's own system prompt.
    pub system: &'a str,
    /// The prompt of the session.
    pub input: &'a str,
    /// `--max-turns`.
    pub max_turns: usize,
    /// The user's command rules and hidden variables.
    pub settings: &'a ToolSettings,
    /// Where the raw stream-json output is saved, if anywhere.
    pub log: Option<&'a Path>,
}

/// How a session that did not fail ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Claude Code finished with a final answer.
    Answered,
    /// Claude Code stopped at `--max-turns`.
    TurnLimit,
    /// The budget ran out, for this reason; the harness stopped Claude Code.
    Budget(String),
    /// The cancel flag was set; the harness stopped Claude Code.
    Interrupted,
    /// Claude Code hit the usage limit of the subscription and could not
    /// go on.
    UsageLimit(UsageLimit),
}

/// What Claude Code said when it hit the usage limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLimit {
    /// When the limit resets, in unix seconds: the `resetsAt` of the
    /// `rate_limit_event` that refused the session. `None` when there was
    /// none; Claude Code's message may still name the time, in its own
    /// words (`resets 3:45pm`).
    pub resets_at: Option<u64>,
    /// Claude Code's message.
    pub message: String,
}

impl std::fmt::Display for UsageLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Claude Code hit the usage limit")?;
        if let Some(at) = self.resets_at {
            write!(f, " (resets {} UTC)", crate::session::format_utc(at))?;
        }
        write!(f, ": {}", self.message)
    }
}

/// What a session did.
pub struct Outcome {
    pub end: Result<End>,
    /// Tokens the session used, as Claude Code reported them.
    pub usage: Usage,
    /// Claude Code's own cost estimate (a subscription is not billed by it).
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    /// Claude Code's id of the session (`claude --resume <id>` opens it).
    pub session_id: Option<String>,
}

impl Outcome {
    /// One line for the run log.
    pub fn summary(&self) -> String {
        let mut parts = vec![match &self.end {
            Ok(End::Answered) => "answered".to_string(),
            Ok(End::TurnLimit) => "stopped at --max-turns".to_string(),
            Ok(End::Budget(_)) => "stopped by the budget".to_string(),
            Ok(End::Interrupted) => "interrupted".to_string(),
            Ok(End::UsageLimit(_)) => "stopped at the usage limit".to_string(),
            Err(_) => "failed".to_string(),
        }];
        if let Some(turns) = self.turns {
            parts.push(format!("{turns} turns"));
        }
        parts.push(format!("{} tokens", tokens(self.usage)));
        if let Some(cost) = self.cost_usd {
            parts.push(format!("estimated cost ${cost:.4}"));
        }
        if let Some(id) = &self.session_id {
            parts.push(format!("claude session {id}"));
        }
        parts.join(", ")
    }
}

/// The error of a session whose Claude Code is not signed in, or whose
/// login was refused. It carries what Claude Code said.
#[derive(Debug)]
pub struct NotLoggedIn(pub String);

impl std::fmt::Display for NotLoggedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Claude Code is not signed in, or its login was refused ({}); {LOGIN_HINT}",
            preview(&self.0, ERROR_CHARS)
        )
    }
}

impl std::error::Error for NotLoggedIn {}

impl ClaudeCode {
    /// [`BIN_ENV`] and [`MODEL_ENV`].
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        Self {
            bin: get(BIN_ENV).map_or_else(|| PathBuf::from("claude"), PathBuf::from),
            model: get(MODEL_ENV),
            grace: DEFAULT_GRACE,
            scrub: !get(SCRUB_ENV).is_some_and(|value| is_off(&value)),
        }
    }

    /// What the user should know before the sessions start: whether the
    /// commands Claude Code runs can read its credentials.
    pub fn notices(&self) -> Vec<String> {
        self.notices_with(std::env::var_os("PATH").as_deref())
    }

    fn notices_with(&self, path: Option<&std::ffi::OsStr>) -> Vec<String> {
        if !self.scrub {
            return vec![format!(
                "{SCRUB_ENV} is off: the commands Claude Code runs see its environment, \
                 CLAUDE_CODE_OAUTH_TOKEN included if you set it"
            )];
        }
        if cfg!(target_os = "linux") && !on_path("bwrap", path) {
            return vec![format!(
                "`bwrap` is not on PATH: on Linux Claude Code needs bubblewrap to hide \
                 credentials from the commands it runs ({SCRUB_ENV}), and its sessions fail \
                 without it (unless managed settings name it in sandbox.bwrapPath). Install \
                 bubblewrap, or set {SCRUB_ENV}=0 to let the commands see \
                 CLAUDE_CODE_OAUTH_TOKEN"
            )];
        }
        Vec::new()
    }

    /// Checks that `claude --version` runs and is at least
    /// [`MIN_VERSION`], and that Claude Code signs in with the subscription
    /// ([`ClaudeCode::check_auth`]); returns what `--version` printed.
    pub async fn check(&self, root: &Path, settings: &ToolSettings) -> Result<String> {
        let bin = self.bin.display();
        let mut command = self.command(root, settings);
        command.arg("--version").env_remove(SCRUB_ENV);
        let output = match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
            Err(_) => bail!(
                "`{bin} --version` did not answer within {}s",
                VERSION_TIMEOUT.as_secs()
            ),
            Ok(Err(err)) => {
                return Err(err).with_context(|| {
                    format!(
                        "cannot run `{bin}` (install Claude Code from \
                         https://code.claude.com/docs/en/setup, or point {BIN_ENV} at it)"
                    )
                });
            }
            Ok(Ok(output)) => output,
        };
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() {
            bail!(
                "`{bin} --version` failed ({}): {}",
                output.status,
                preview(&String::from_utf8_lossy(&output.stderr), ERROR_CHARS)
            );
        }
        let version = parse_version(&stdout).with_context(|| {
            format!(
                "cannot read a version from `{bin} --version`: `{}`",
                preview(&stdout, 100)
            )
        })?;
        if version < MIN_VERSION {
            let [a, b, c] = MIN_VERSION;
            bail!(
                "Claude Code {} is too old: Junees needs {a}.{b}.{c} or later \
                 (update it with `claude update`)",
                preview(&stdout, 100)
            );
        }
        self.check_auth(settings).await?;
        Ok(stdout)
    }

    /// Checks with `claude auth status --json`, in the environment of a
    /// session, that Claude Code signs in with the subscription
    /// ([`SUBSCRIPTION_AUTH`]): not signed in is [`NotLoggedIn`], any other
    /// method an error. [`API_ENV_VARS`] never reach `claude`, but an
    /// `apiKeyHelper` or an `env` block in Claude Code's user settings, or
    /// an active federation profile, would still win over the subscription.
    /// Runs before every session, because a session's commands can change
    /// those settings. It runs in `/`, so that a repository's
    /// `.claude/settings.json`, which sessions do not load, plays no part.
    pub async fn check_auth(&self, settings: &ToolSettings) -> Result<()> {
        let bin = self.bin.display();
        let mut command = self.command(Path::new("/"), settings);
        command
            .args(["auth", "status", "--json"])
            .env_remove(SCRUB_ENV);
        let output = match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
            Err(_) => bail!(
                "`{bin} auth status` did not answer within {}s",
                VERSION_TIMEOUT.as_secs()
            ),
            Ok(result) => result.with_context(|| format!("cannot run `{bin} auth status`"))?,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let status = serde_json::from_str::<Value>(stdout.trim()).ok();
        let method = status
            .as_ref()
            .and_then(|status| status["authMethod"].as_str());
        match method {
            Some(method) if SUBSCRIPTION_AUTH.contains(&method) && output.status.success() => {
                Ok(())
            }
            Some(method) if SUBSCRIPTION_AUTH.contains(&method) || method == "none" => Err(
                NotLoggedIn(format!("`claude auth status`: {method}, {}", output.status)).into(),
            ),
            None if status.is_some() && !output.status.success() => {
                Err(NotLoggedIn(format!("`claude auth status`: {}", output.status)).into())
            }
            Some(method) => bail!(
                "Claude Code would not use the Claude subscription: `{bin} auth status` says it \
                 signs in with `{}`. Junees runs Claude Code only on the subscription, so remove \
                 that sign-in (an `apiKeyHelper`, or API or cloud provider variables in the `env` \
                 of ~/.claude/settings.json; see \
                 https://code.claude.com/docs/en/authentication#authentication-precedence)",
                preview(method, 100)
            ),
            None => bail!(
                "cannot tell how Claude Code signs in: `{bin} auth status --json` ({}) printed \
                 `{}` {}",
                output.status,
                preview(stdout.trim(), ERROR_CHARS),
                preview(String::from_utf8_lossy(&output.stderr).trim(), ERROR_CHARS)
            ),
        }
    }

    /// The arguments of a session. The session runs unattended, so nothing
    /// may wait for a person: edits and the [`TOOLS`] are allowed without
    /// asking, anything else that would ask is refused
    /// (`--permission-prompts none`), and [`deny_rules`] refuse the
    /// dangerous commands. `--dangerously-skip-permissions` is never used.
    /// Settings files of the repository are not loaded
    /// (`--setting-sources user`), nor are its MCP servers
    /// (`--strict-mcp-config`): a session can write them, and the next
    /// session would run their hooks and servers outside the rules.
    pub fn arguments(&self, request: &Request<'_>) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            request.input,
            "--output-format",
            "stream-json",
            "--verbose",
            "--append-system-prompt",
            request.system,
            "--permission-mode",
            "acceptEdits",
            "--permission-prompts",
            "none",
            "--tools",
            TOOLS,
            "--allowedTools",
            TOOLS,
            "--setting-sources",
            "user",
            "--strict-mcp-config",
            "--max-turns",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        args.push(request.max_turns.to_string());
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        // Last: the option takes every argument up to the next option.
        let deny = deny_rules(request.settings);
        if !deny.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(deny);
        }
        args
    }

    /// Runs one session to its end. `cancel` and `limit` stop it (SIGINT to
    /// Claude Code's process group, which ends its turn, and after
    /// [`ClaudeCode::grace`] SIGKILL); `limit` is asked after each model
    /// response and every second, with the tokens the session used so far.
    pub async fn run(
        &self,
        request: &Request<'_>,
        cancel: &Cancel,
        limit: Option<&mut Limit>,
        report: &mut Reporter<'_>,
    ) -> Outcome {
        let mut stream = Stream::default();
        let exit = self
            .drive(request, cancel, limit, &mut stream, report)
            .await;
        stream.outcome(exit)
    }

    fn command(&self, root: &Path, settings: &ToolSettings) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for name in API_ENV_VARS
            .iter()
            .chain(SECRET_ENV_VARS)
            .chain(REPO_ENV_VARS)
            .map(|name| name.to_string())
            .chain(settings.hidden_env.iter().cloned())
        {
            command.env_remove(name);
        }
        command
    }

    async fn drive(
        &self,
        request: &Request<'_>,
        cancel: &Cancel,
        mut limit: Option<&mut Limit>,
        stream: &mut Stream,
        report: &mut Reporter<'_>,
    ) -> Result<Exit> {
        self.check_auth(request.settings).await?;
        let mut command = self.command(request.root, request.settings);
        command.args(self.arguments(request));
        if self.scrub {
            command.env(SCRUB_ENV, "1");
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run `{}`", self.bin.display()))?;
        let mut group = Group(child.id().and_then(|id| i32::try_from(id).ok()));
        let stdout = child.stdout.take().context("no stdout from claude")?;
        let stderr = child.stderr.take().context("no stderr from claude")?;
        let mut stderr = tokio::spawn(read_tail(stderr));
        let mut log = request.log.and_then(|path| match open_log(path) {
            Ok(file) => Some(file),
            Err(err) => {
                warn(
                    report,
                    &format!(
                        "cannot save the session output to {}: {err}",
                        path.display()
                    ),
                );
                None
            }
        });
        let mut lines = BufReader::new(stdout).split(b'\n');
        let mut stdout_open = true;
        let mut tick = tokio::time::interval(BUDGET_TICK);
        let mut stopped = None;
        let mut kill_at: Option<Instant> = None;
        let status = loop {
            let mut ask = false;
            tokio::select! {
                biased;
                status = child.wait() => break status.context("cannot wait for claude")?,
                _ = cancel.cancelled(), if stopped.is_none() => {
                    stopped = Some(End::Interrupted);
                    kill_at = Some(group.interrupt(self.grace));
                }
                line = lines.next_segment(), if stdout_open => match line {
                    Ok(Some(line)) => {
                        save(&mut log, &line, report);
                        ask = stream.handle(&String::from_utf8_lossy(&line), report);
                    }
                    Ok(None) => stdout_open = false,
                    Err(err) => {
                        warn(report, &format!("cannot read the output of claude: {err}"));
                        stdout_open = false;
                    }
                },
                _ = tick.tick() => {
                    if kill_at.is_some_and(|at| Instant::now() >= at) {
                        group.kill();
                    }
                    ask = true;
                }
            }
            if ask
                && stopped.is_none()
                && let Some(limit) = limit.as_deref_mut()
                && let Some(reason) = limit(stream.usage())
            {
                warn(report, &format!("{reason}; stopping Claude Code"));
                stopped = Some(End::Budget(reason));
                kill_at = Some(group.interrupt(self.grace));
            }
        };
        if stdout_open {
            let drain = async {
                while let Ok(Some(line)) = lines.next_segment().await {
                    save(&mut log, &line, report);
                    stream.handle(&String::from_utf8_lossy(&line), report);
                }
            };
            let _ = tokio::time::timeout(DRAIN_GRACE, drain).await;
        }
        // What a stopped session left running in Claude Code's group goes
        // with it (its commands were killed before the stop); a session
        // that ended on its own keeps its background processes, like bash.
        // `claude` has been waited for, so only the group is signalled.
        if stopped.is_some() {
            group.signal(libc::SIGKILL);
        }
        group.disarm();
        let stderr = match tokio::time::timeout(DRAIN_GRACE, &mut stderr).await {
            Ok(Ok(text)) => text,
            _ => {
                stderr.abort();
                String::new()
            }
        };
        Ok(Exit {
            status,
            stopped,
            stderr,
        })
    }
}

/// How the `claude` process ended.
struct Exit {
    status: ExitStatus,
    /// Set when the harness stopped it.
    stopped: Option<End>,
    /// The end of its stderr.
    stderr: String,
}

/// Claude Code's process group (it runs in its own), killed with every
/// process Claude Code started when dropped, unless disarmed.
///
/// Claude Code's Bash tool starts each command detached, in a session of
/// its own, so a signal to the group does not reach the commands. Stopping
/// a session therefore kills Claude Code's descendants first, found by
/// their parent links in `/proc`. That only works while `claude` has not
/// been waited for: after that its pid may belong to another process, and
/// the commands it left are children of init, out of reach.
struct Group(Option<i32>);

impl Group {
    fn signal(&self, signal: i32) {
        if let Some(id) = self.0 {
            // SAFETY: killpg only sends a signal to the group.
            unsafe {
                libc::killpg(id, signal);
            }
        }
    }

    /// Kills every process Claude Code started, in any session or group.
    /// Only while `claude` has not been waited for.
    fn kill_commands(&self) {
        if let Some(id) = self.0 {
            kill_descendants(id);
        }
    }

    /// Kills the commands of the session, then asks Claude Code to end its
    /// turn; returns when to kill Claude Code instead.
    fn interrupt(&self, grace: Duration) -> Instant {
        self.kill_commands();
        self.signal(libc::SIGINT);
        Instant::now() + grace
    }

    /// Kills Claude Code and everything it started. Only while `claude`
    /// has not been waited for.
    fn kill(&self) {
        self.kill_commands();
        self.signal(libc::SIGKILL);
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// How often [`kill_descendants`] looks for processes started while it
/// was stopping the others.
const FREEZE_ROUNDS: usize = 16;

/// Kills every descendant of `root` (not `root` itself). They are stopped
/// (SIGSTOP) first, until a look at `/proc` finds no new one, so none can
/// start another process in between or die and leave its children to
/// init; then all of them get SIGKILL. Without `/proc` it does nothing.
fn kill_descendants(root: i32) {
    let mut found = std::collections::HashSet::new();
    for _ in 0..FREEZE_ROUNDS {
        let new: Vec<i32> = descendants(root)
            .into_iter()
            .filter(|pid| found.insert(*pid))
            .collect();
        if new.is_empty() {
            break;
        }
        for pid in new {
            // SAFETY: kill only sends a signal.
            unsafe {
                libc::kill(pid, libc::SIGSTOP);
            }
        }
    }
    for pid in found {
        // SAFETY: kill only sends a signal.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

/// The processes below `root` in the process tree, from the parent pid in
/// each `/proc/<pid>/stat`.
fn descendants(root: i32) -> Vec<i32> {
    let Ok(dir) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        if let Some(parent) = parent_of(pid) {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut found = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            found.push(child);
            queue.push(child);
        }
    }
    found
}

/// The parent pid in `/proc/<pid>/stat`: the field after the state, which
/// follows the parenthesized command name (which may hold spaces and `)`).
fn parent_of(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Reads a pipe to its end and returns the last [`STDERR_TAIL_CHARS`]
/// characters, never holding much more in memory.
async fn read_tail(mut pipe: impl AsyncRead + Unpin) -> String {
    const KEEP: usize = 4 * STDERR_TAIL_CHARS;
    let mut tail = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > KEEP {
            tail.drain(..tail.len() - KEEP);
        }
    }
    let text = String::from_utf8_lossy(&tail);
    let total = text.chars().count();
    text.chars()
        .skip(total.saturating_sub(STDERR_TAIL_CHARS))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Opens the file a session's output is saved to: private, like the
/// session journals, because it holds tool output.
fn open_log(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Adds a line to the saved output; after a failed write, saving stops.
fn save(log: &mut Option<File>, line: &[u8], report: &mut Reporter<'_>) {
    let failed = match log {
        Some(file) => file
            .write_all(line)
            .and_then(|()| file.write_all(b"\n"))
            .err(),
        None => None,
    };
    if let Some(err) = failed {
        warn(report, &format!("stopped saving the session output: {err}"));
        *log = None;
    }
}

/// `2.1.319 (Claude Code)` as `[2, 1, 319]`.
fn parse_version(text: &str) -> Option<[u32; 3]> {
    let word = text.split_whitespace().next()?;
    let mut parts = word.split('.').map(|part| part.parse::<u32>().ok());
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(version)
}

/// An off value of a Claude Code switch, as Claude Code reads it.
fn is_off(value: &str) -> bool {
    ["0", "false", "no", "off"].contains(&value.trim().to_lowercase().as_str())
}

/// Whether an executable file `name` is in one of the folders of `path`.
fn on_path(name: &str, path: Option<&std::ffi::OsStr>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.into_iter().flat_map(std::env::split_paths).any(|dir| {
        fs::metadata(dir.join(name))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

fn looks_like_login_failure(text: &str) -> bool {
    let text = text.to_lowercase();
    LOGIN_TEXTS.iter().any(|needle| text.contains(needle))
}

fn warn(report: &mut Reporter<'_>, text: &str) {
    report(Report::Event(Event::Warning(text)));
}

/// What the stream-json output of a session said so far.
#[derive(Default)]
struct Stream {
    /// Tool names by tool_use id, for showing the results.
    tool_names: HashMap<String, String>,
    /// The usage of each model response, by message id: Claude Code sends
    /// a response's blocks as separate events, each with the usage.
    usage: HashMap<String, Usage>,
    /// Usage of responses without an id.
    unnamed: Usage,
    session_id: Option<String>,
    /// What a refused login said, if the latest model call failed so.
    auth: Option<String>,
    /// What Claude Code said, if the latest model call failed with
    /// [`RATE_LIMIT_ERROR`].
    rate_limited: Option<String>,
    /// The latest `rate_limit_event`.
    rate_limit: Option<RateLimit>,
    /// Whether a response's usage could not be read (warned once).
    bad_usage: bool,
    result: Option<Finished>,
    /// The last lines that were not JSON.
    plain: Vec<String>,
}

/// The `rate_limit_info` of a `rate_limit_event`
/// (<https://code.claude.com/docs/en/agent-sdk/typescript>,
/// `SDKRateLimitEvent`).
struct RateLimit {
    /// `status` is `rejected`: requests are refused.
    rejected: bool,
    /// `resetsAt`, in unix seconds (the docs give no unit; the 2.1.319
    /// binary documents it as "unix epoch seconds", the unit of the
    /// status line's `rate_limits.*.resets_at`).
    resets_at: Option<u64>,
    /// `errorCode` is `credits_required`: the included usage is used up and
    /// only bought usage credits let the session go on.
    credits_required: bool,
}

/// The `result` event.
struct Finished {
    subtype: String,
    is_error: bool,
    /// `api_error_status`: the HTTP status of the API error that ended the
    /// session.
    api_error_status: Option<u64>,
    text: String,
    usage: Option<Usage>,
    cost_usd: Option<f64>,
    turns: Option<u64>,
}

impl Stream {
    /// Takes one line of output; returns whether the usage changed.
    fn handle(&mut self, line: &str, report: &mut Reporter<'_>) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            warn(report, &format!("claude: {}", preview(line, ERROR_CHARS)));
            if self.plain.len() == 20 {
                self.plain.remove(0);
            }
            self.plain.push(line.to_string());
            return false;
        };
        if let Some(id) = event["session_id"].as_str() {
            self.session_id = Some(id.to_string());
        }
        match event["type"].as_str() {
            Some("assistant") => return self.assistant(&event, report),
            Some("user") => self.user(&event, report),
            Some("system") => self.system(&event, line, report),
            Some("result") => self.result = Some(Finished::parse(&event)),
            Some("rate_limit_event") => self.rate_limit_event(&event, report),
            _ => {}
        }
        false
    }

    fn assistant(&mut self, event: &Value, report: &mut Reporter<'_>) -> bool {
        let message = &event["message"];
        let main = event["parent_tool_use_id"].is_null();
        // Only the latest model call counts: a refused login that a later
        // response or another error followed is not why the session ended.
        self.auth = match event["error"].as_str() {
            Some(error) if AUTH_ERRORS.contains(&error) => {
                let text = content_text(&message["content"]);
                Some(if text.is_empty() {
                    error.to_string()
                } else {
                    text
                })
            }
            _ => None,
        };
        // A response that came through means requests are not refused.
        if event["error"].is_null() {
            self.rate_limit = None;
        }
        self.rate_limited = (event["error"] == RATE_LIMIT_ERROR).then(|| {
            let text = content_text(&message["content"]);
            if text.trim().is_empty() {
                RATE_LIMIT_ERROR.to_string()
            } else {
                text
            }
        });
        let mut changed = false;
        if let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) {
            match serde_json::from_value::<Usage>(usage.clone()) {
                Ok(usage) => {
                    match message["id"].as_str() {
                        Some(id) => {
                            self.usage.insert(id.to_string(), usage);
                        }
                        None => self.unnamed.add(usage),
                    }
                    changed = true;
                }
                Err(err) if !self.bad_usage => {
                    self.bad_usage = true;
                    warn(
                        report,
                        &format!(
                            "cannot read the token usage of a Claude Code response ({err}); \
                             the budget does not count such responses"
                        ),
                    );
                }
                Err(_) => {}
            }
        }
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") if main => {
                    let text = block["text"].as_str().unwrap_or_default().trim();
                    if !text.is_empty() {
                        // Claude Code hands over whole blocks.
                        report(Report::Event(Event::TextDelta(text)));
                        report(Report::Event(Event::TextEnd));
                    }
                }
                Some("tool_use") => {
                    let name = block["name"].as_str().unwrap_or("?");
                    if let Some(id) = block["id"].as_str() {
                        self.tool_names.insert(id.to_string(), name.to_string());
                    }
                    if main {
                        report(Report::Event(Event::ToolCall {
                            name,
                            input: &block["input"],
                        }));
                    }
                }
                _ => {}
            }
        }
        changed
    }

    fn user(&mut self, event: &Value, report: &mut Reporter<'_>) {
        if !event["parent_tool_use_id"].is_null() {
            return;
        }
        for block in event["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] != "tool_result" {
                continue;
            }
            let name = block["tool_use_id"]
                .as_str()
                .and_then(|id| self.tool_names.get(id))
                .map_or("?", String::as_str);
            let output = ToolOutput {
                content: content_text(&block["content"]),
                is_error: block["is_error"].as_bool().unwrap_or(false),
            };
            report(Report::Event(Event::ToolResult {
                name,
                output: &output,
            }));
        }
    }

    fn system(&mut self, event: &Value, line: &str, report: &mut Reporter<'_>) {
        match event["subtype"].as_str() {
            Some("api_retry") => {
                let error = event["error"].as_str().unwrap_or("unknown");
                self.auth = AUTH_ERRORS.contains(&error).then(|| error.to_string());
                self.rate_limited = (error == RATE_LIMIT_ERROR).then(|| error.to_string());
                warn(
                    report,
                    &format!(
                        "Claude Code retries a model call (attempt {} of {}): {error}",
                        event["attempt"], event["max_retries"]
                    ),
                );
            }
            Some("permission_denied") => warn(
                report,
                &format!(
                    "Claude Code refused a tool call: {}",
                    preview(line, ERROR_CHARS)
                ),
            ),
            _ => {}
        }
    }

    fn rate_limit_event(&mut self, event: &Value, report: &mut Reporter<'_>) {
        let info = &event["rate_limit_info"];
        let limit = RateLimit {
            rejected: info["status"] == "rejected",
            resets_at: info["resetsAt"]
                .as_f64()
                .filter(|at| at.is_finite() && *at > 0.0)
                .map(|at| at as u64),
            credits_required: info["errorCode"] == "credits_required",
        };
        if !limit.rejected {
            // The limit let a request through.
            self.rate_limited = None;
        } else {
            let when = limit.resets_at.map_or_else(String::new, |at| {
                format!(" until {} UTC", crate::session::format_utc(at))
            });
            warn(
                report,
                &format!("Claude Code reports the usage limit reached; requests are refused{when}"),
            );
        }
        self.rate_limit = Some(limit);
    }

    /// Tokens of the model responses seen so far.
    fn usage(&self) -> Usage {
        let mut total = self.unnamed;
        for usage in self.usage.values() {
            total.add(*usage);
        }
        total
    }

    fn outcome(self, exit: Result<Exit>) -> Outcome {
        let streamed = self.usage();
        // The result's totals also count calls the stream does not show;
        // the stream counts responses of a session that never got there.
        let usage = match self.result.as_ref().and_then(|result| result.usage) {
            Some(reported) if tokens(reported) >= tokens(streamed) => reported,
            _ => streamed,
        };
        let end = exit.and_then(|exit| self.end(exit));
        let (cost_usd, turns) = self
            .result
            .as_ref()
            .map_or((None, None), |result| (result.cost_usd, result.turns));
        Outcome {
            end,
            usage,
            cost_usd,
            turns,
            session_id: self.session_id,
        }
    }

    fn end(&self, exit: Exit) -> Result<End> {
        if let Some(stopped) = exit.stopped {
            return Ok(stopped);
        }
        match &self.result {
            Some(result) if !result.is_error && result.subtype == "success" => {
                return Ok(End::Answered);
            }
            Some(result) if result.subtype == "error_max_turns" => return Ok(End::TurnLimit),
            _ => {}
        }
        if let Some(said) = self.login_failure(&exit.stderr) {
            return Err(NotLoggedIn(said).into());
        }
        if let Some(limit) = self.usage_limit() {
            return Ok(End::UsageLimit(limit));
        }
        let stderr = if exit.stderr.is_empty() {
            String::new()
        } else {
            format!("; stderr: {}", preview(&exit.stderr, ERROR_CHARS))
        };
        match &self.result {
            Some(result) => Err(anyhow!(
                "Claude Code ended the session with `{}` ({}): {}{stderr}",
                result.subtype,
                exit.status,
                preview(&result.text, ERROR_CHARS)
            )),
            None => Err(anyhow!(
                "`claude` exited ({}) without a result{stderr}",
                exit.status
            )),
        }
    }

    /// The usage limit, if that is why a session that did not finish
    /// ended: the latest model call failed with [`RATE_LIMIT_ERROR`], the
    /// result names a 429, or the latest `rate_limit_event` refuses
    /// requests.
    fn usage_limit(&self) -> Option<UsageLimit> {
        let refused = self.rate_limit.as_ref().filter(|limit| limit.rejected);
        let status_429 = self
            .result
            .as_ref()
            .is_some_and(|result| result.api_error_status == Some(429));
        if self.rate_limited.is_none() && refused.is_none() && !status_429 {
            return None;
        }
        let said = self
            .result
            .as_ref()
            .map(|result| result.text.trim())
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| self.rate_limited.clone())
            .unwrap_or_else(|| "requests refused by the usage limit".to_string());
        let mut message = preview(&said, ERROR_CHARS);
        if refused.is_some_and(|limit| limit.credits_required) {
            message.push_str(
                " (Claude Code says usage credits are required to go on: \
                 `credits_required`, which does not lift by itself)",
            );
        }
        Some(UsageLimit {
            resets_at: refused.and_then(|limit| limit.resets_at),
            message,
        })
    }

    /// What Claude Code said when its login was missing or refused.
    fn login_failure(&self, stderr: &str) -> Option<String> {
        if let Some(said) = &self.auth {
            return Some(said.clone());
        }
        match &self.result {
            Some(result) => (result.is_error && looks_like_login_failure(&result.text))
                .then(|| result.text.clone()),
            None => self
                .plain
                .iter()
                .map(String::as_str)
                .chain([stderr])
                .find(|text| looks_like_login_failure(text))
                .map(str::to_string),
        }
    }
}

impl Finished {
    fn parse(event: &Value) -> Self {
        let text = match event["result"].as_str() {
            Some(text) => text.to_string(),
            None => event["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("; "),
        };
        Self {
            subtype: event["subtype"].as_str().unwrap_or("?").to_string(),
            is_error: event["is_error"].as_bool().unwrap_or(false),
            api_error_status: event["api_error_status"].as_u64(),
            text,
            usage: result_usage(event),
            cost_usd: event["total_cost_usd"].as_f64(),
            turns: event["num_turns"].as_u64(),
        }
    }
}

/// The tokens a result reports: `modelUsage` counts every model the
/// session used (subagents and Claude Code's own small calls too),
/// `usage` only the main conversation.
fn result_usage(event: &Value) -> Option<Usage> {
    if let Some(models) = event["modelUsage"].as_object()
        && !models.is_empty()
    {
        let mut total = Usage::default();
        for model in models.values() {
            let count = |key: &str| model[key].as_u64().unwrap_or(0);
            total.add(Usage {
                input_tokens: count("inputTokens"),
                output_tokens: count("outputTokens"),
                cache_creation_input_tokens: count("cacheCreationInputTokens"),
                cache_read_input_tokens: count("cacheReadInputTokens"),
                ..Default::default()
            });
        }
        return Some(total);
    }
    event
        .get("usage")
        .filter(|usage| usage.is_object())
        .and_then(|usage| serde_json::from_value(usage.clone()).ok())
}

/// The text of a message's or tool result's content: a string, or the
/// text of its blocks.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().unwrap_or_default().to_string(),
                Some(other) => format!("[{other}]"),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Command rules Claude Code refuses (`--disallowedTools`), in its
/// permission rule syntax: the core of the bash command policy, unless
/// `JUNEES_ALLOW_DANGEROUS` turned it off, and every
/// `JUNEES_DENY_COMMANDS` prefix. Claude Code checks each command of a
/// compound line (`a && b`, pipes, `$(...)`) against them, but the rules
/// only match words as written: `/usr/bin/sudo`, `git -C . push --force`
/// or a command a script builds at run time get past them, like they get
/// past parts of the bash tool's own policy.
pub fn deny_rules(settings: &ToolSettings) -> Vec<String> {
    let mut rules = Vec::new();
    if !settings.allow_dangerous {
        for program in [
            "sudo", "su", "doas", "pkexec", "runuser", "shutdown", "reboot", "halt", "poweroff",
            "wipefs",
        ] {
            rules.push(format!("Bash({program} *)"));
        }
        // No space: also mkfs.ext4 and friends.
        rules.push("Bash(mkfs*)".into());
        for action in ["poweroff", "reboot", "halt", "kexec"] {
            rules.push(format!("Bash(systemctl {action} *)"));
        }
        rules.push("Bash(dd * of=/dev/*)".into());
        // A `*` in a rule always matches anything, so `rm -rf /*` cannot
        // be told apart from `rm -rf /tmp/x`; those are left to the rules
        // below and to the user's `JUNEES_DENY_COMMANDS`.
        for flags in ["-rf", "-fr", "-r", "-R", "-Rf", "-fR"] {
            for target in ["/", "~", "~/", "$HOME", "${HOME}", "..", "../"] {
                rules.push(format!("Bash(rm {flags} {target} *)"));
            }
        }
        for push in [
            "git push --force *",
            "git push * --force",
            "git push * --force *",
            "git push -f *",
            "git push * -f",
            "git push * -f *",
            "git push * +*",
        ] {
            rules.push(format!("Bash({push})"));
        }
        // A shell reading its script from a pipe: `curl ... | sh`.
        for shell in ["sh", "bash", "zsh", "dash", "ksh"] {
            rules.push(format!("Bash({shell})"));
            rules.push(format!("Bash({shell} -)"));
            rules.push(format!("Bash({shell} -s *)"));
        }
    }
    for prefix in &settings.deny_commands {
        rules.push(format!("Bash({prefix} *)"));
    }
    rules
}

#[cfg(test)]
pub(crate) mod testing {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// What a fake signed in with the subscription answers to `auth`.
    pub const SIGNED_IN: &str = r#"echo '{"loggedIn":true,"authMethod":"claude.ai"}'; exit 0"#;

    /// Writes an executable bash script standing in for `claude`, and
    /// waits until it can be run: a test that forks while the file is open
    /// for writing would make `exec` fail with ETXTBSY. `auth status` says
    /// it is signed in with the subscription, and `--version` prints a
    /// current version unless `body` answers it first.
    pub fn fake_claude(dir: &Path, body: &str) -> PathBuf {
        fake_claude_auth(dir, SIGNED_IN, body)
    }

    /// [`fake_claude`] answering `auth` (`auth status --json`) with `auth`.
    pub fn fake_claude_auth(dir: &Path, auth: &str, body: &str) -> PathBuf {
        let path = dir.join("fake-claude");
        std::fs::write(
            &path,
            format!(
                "#!/bin/bash\n\
                 [ -n \"$JUNEES_FAKE_PROBE\" ] && exit 0\n\
                 if [ \"$1\" = auth ]; then\n{auth}\nfi\n\
                 {body}\n\
                 [ \"$1\" = --version ] && echo '2.1.319 (Claude Code)' && exit 0\n\
                 exit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..200 {
            match std::process::Command::new(&path)
                .env("JUNEES_FAKE_PROBE", "1")
                .status()
            {
                Ok(_) => return path,
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("cannot run the fake claude: {err}"),
            }
        }
        panic!("the fake claude stayed busy");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::testing::{fake_claude, fake_claude_auth};
    use super::*;

    /// Collects what a session reports, one string per event.
    fn collector(seen: &Arc<Mutex<Vec<String>>>) -> impl FnMut(Report<'_>) + use<> {
        let seen = Arc::clone(seen);
        move |report| {
            let text = match report {
                Report::Line(text) => format!("line {text}"),
                Report::Event(Event::TextDelta(text)) => format!("text {text}"),
                Report::Event(Event::Warning(text)) => format!("warn {text}"),
                Report::Event(Event::ToolCall { name, input }) => format!("-> {name} {input}"),
                Report::Event(Event::ToolResult { name, output }) => {
                    format!("<- {name} {} {}", output.is_error, output.content)
                }
                Report::Event(_) => return,
            };
            seen.lock().unwrap().push(text);
        }
    }

    fn claude(bin: PathBuf) -> ClaudeCode {
        ClaudeCode {
            bin,
            model: None,
            grace: Duration::from_millis(300),
            scrub: true,
        }
    }

    fn request<'a>(
        root: &'a Path,
        settings: &'a ToolSettings,
        log: Option<&'a Path>,
    ) -> Request<'a> {
        Request {
            root,
            system: "rules",
            input: "do it",
            max_turns: 7,
            settings,
            log,
        }
    }

    /// The output of a session that reads a file and answers: two events
    /// for the first response (one per block, same id and usage), a tool
    /// result, the answer and the result.
    const SESSION: &str = r#"{"type":"system","subtype":"init","session_id":"s-1","model":"claude-x"}
{"type":"assistant","parent_tool_use_id":null,"session_id":"s-1","message":{"id":"msg_1","content":[{"type":"text","text":"Looking."}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100}}}
{"type":"assistant","parent_tool_use_id":null,"session_id":"s-1","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.txt"}}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100}}}
{"type":"user","parent_tool_use_id":null,"session_id":"s-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"hello"}]}]}}
{"type":"assistant","parent_tool_use_id":null,"session_id":"s-1","message":{"id":"msg_2","content":[{"type":"text","text":"Done."}],"usage":{"input_tokens":20,"output_tokens":7}}}
{"type":"result","subtype":"success","is_error":false,"result":"Done.","num_turns":2,"total_cost_usd":0.0123,"session_id":"s-1","usage":{"input_tokens":30,"output_tokens":12,"cache_read_input_tokens":100},"modelUsage":{"claude-x":{"inputTokens":30,"outputTokens":12,"cacheReadInputTokens":100,"cacheCreationInputTokens":0},"claude-small":{"inputTokens":40,"outputTokens":3,"cacheReadInputTokens":0,"cacheCreationInputTokens":2}}}"#;

    fn print(lines: &str) -> String {
        format!("cat <<'JSON'\n{lines}\nJSON")
    }

    #[test]
    fn reads_the_backend_and_the_claude_settings() {
        let none = |_: &str| None;
        assert_eq!(Backend::from_lookup(none).unwrap(), Backend::Api);
        let set =
            |value: &'static str| move |key: &str| (key == BACKEND_ENV).then(|| value.to_string());
        assert_eq!(
            Backend::from_lookup(set(" claude-code ")).unwrap(),
            Backend::ClaudeCode
        );
        assert_eq!(Backend::from_lookup(set("api")).unwrap(), Backend::Api);
        assert_eq!(Backend::from_lookup(set(" ")).unwrap(), Backend::Api);
        let err = Backend::from_lookup(set("claude")).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "JUNEES_BACKEND: unknown backend `claude`; use `api` or `claude-code`"
        );

        assert_eq!(
            ClaudeCode::from_lookup(none),
            ClaudeCode {
                bin: PathBuf::from("claude"),
                model: None,
                grace: DEFAULT_GRACE,
                scrub: true,
            }
        );
        let claude = ClaudeCode::from_lookup(|key| match key {
            BIN_ENV => Some(" /opt/claude ".into()),
            MODEL_ENV => Some("opus".into()),
            _ => None,
        });
        assert_eq!(claude.bin, PathBuf::from("/opt/claude"));
        assert_eq!(claude.model.as_deref(), Some("opus"));
        for (value, scrub) in [
            ("1", true),
            ("yes", true),
            (" ", true),
            ("0", false),
            (" Off ", false),
            ("FALSE", false),
            ("no", false),
        ] {
            let claude = ClaudeCode::from_lookup(|key| (key == SCRUB_ENV).then(|| value.into()));
            assert_eq!(claude.scrub, scrub, "{value:?}");
        }
    }

    #[test]
    fn warns_when_commands_can_see_the_credentials() {
        let mut claude = claude(PathBuf::from("claude"));
        let dir = tempfile::tempdir().unwrap();
        let bwrap = dir.path().join("bwrap");
        std::fs::write(&bwrap, "").unwrap();
        let with_bwrap = Some(dir.path().as_os_str());
        // Not executable yet.
        let without = claude.notices_with(with_bwrap);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(claude.notices_with(with_bwrap), Vec::<String>::new());
        if cfg!(target_os = "linux") {
            assert_eq!(without.len(), 1);
            assert!(
                without[0].starts_with("`bwrap` is not on PATH"),
                "{without:?}"
            );
            assert_eq!(claude.notices_with(None), without);
        }

        claude.scrub = false;
        let notices = claude.notices_with(with_bwrap);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].starts_with("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB is off")
                && notices[0].contains("CLAUDE_CODE_OAUTH_TOKEN"),
            "{notices:?}"
        );
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("2.1.319 (Claude Code)"), Some([2, 1, 319]));
        assert_eq!(parse_version("10.0.1"), Some([10, 0, 1]));
        assert_eq!(parse_version("2.1 (Claude Code)"), None);
        assert_eq!(parse_version("2.1.3.4"), None);
        assert_eq!(parse_version("claude 2.1.3"), None);
        assert_eq!(parse_version(""), None);
        assert!(parse_version("2.1.258").unwrap() < MIN_VERSION);
        assert!(parse_version("2.1.259").unwrap() >= MIN_VERSION);
    }

    #[test]
    fn arguments_allow_the_tools_without_skipping_permissions() {
        let settings = ToolSettings::default();
        let root = Path::new("/repo");
        let mut claude = claude(PathBuf::from("claude"));
        let args = claude.arguments(&request(root, &settings, None));
        assert_eq!(
            args[..21],
            [
                "-p",
                "do it",
                "--output-format",
                "stream-json",
                "--verbose",
                "--append-system-prompt",
                "rules",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none",
                "--tools",
                "Read,Edit,Write,Glob,Grep,Bash",
                "--allowedTools",
                "Read,Edit,Write,Glob,Grep,Bash",
                "--setting-sources",
                "user",
                "--strict-mcp-config",
                "--max-turns",
                "7",
                "--disallowedTools",
            ]
        );
        assert_eq!(args[21..], deny_rules(&settings)[..]);
        for skip in [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "bypassPermissions",
            "--bare",
        ] {
            assert!(!args.iter().any(|arg| arg == skip), "{skip}");
        }

        claude.model = Some("opus".into());
        let args = claude.arguments(&request(root, &settings, None));
        assert_eq!(args[20..22], ["--model", "opus"]);
        assert_eq!(args[22], "--disallowedTools");
    }

    #[test]
    fn deny_rules_carry_the_command_policy() {
        let rules = deny_rules(&ToolSettings::default());
        for rule in [
            "Bash(sudo *)",
            "Bash(su *)",
            "Bash(shutdown *)",
            "Bash(mkfs*)",
            "Bash(dd * of=/dev/*)",
            "Bash(rm -rf / *)",
            "Bash(rm -fr ~ *)",
            "Bash(rm -r $HOME *)",
            "Bash(rm -rf .. *)",
            "Bash(git push --force *)",
            "Bash(git push * -f)",
            "Bash(git push * +*)",
            "Bash(sh)",
            "Bash(bash -s *)",
        ] {
            assert!(rules.iter().any(|r| r == rule), "{rule} missing");
        }
        // No rule may deny a whole tool or `--force-with-lease`.
        assert!(
            rules
                .iter()
                .all(|rule| rule.starts_with("Bash(") && rule.ends_with(')') && rule.len() > 6)
        );
        assert!(!rules.iter().any(|rule| rule.contains("force-with-lease")));

        let settings = ToolSettings {
            deny_commands: vec!["docker rm".into()],
            allow_dangerous: true,
            ..ToolSettings::default()
        };
        assert_eq!(deny_rules(&settings), ["Bash(docker rm *)"]);
        let mut settings = settings;
        settings.allow_dangerous = false;
        assert_eq!(deny_rules(&settings).last().unwrap(), "Bash(docker rm *)");
    }

    #[test]
    fn the_stream_shows_events_and_counts_each_response_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut report = collector(&seen);
        let mut stream = Stream::default();
        let mut changed = Vec::new();
        for line in SESSION.lines() {
            changed.push(stream.handle(line, &mut report));
        }
        assert_eq!(changed, [false, true, true, false, true, false]);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "text Looking.",
                "-> Read {\"file_path\":\"a.txt\"}",
                "<- Read false hello",
                "text Done.",
            ]
        );
        assert_eq!(tokens(stream.usage()), 115 + 27);
        let exit = Exit {
            status: ExitStatus::default(),
            stopped: None,
            stderr: String::new(),
        };
        let outcome = stream.outcome(Ok(exit));
        assert_eq!(
            outcome.summary(),
            "answered, 2 turns, 187 tokens, estimated cost $0.0123, claude session s-1"
        );
        // modelUsage counts the small model too.
        assert_eq!(
            outcome.usage,
            Usage {
                input_tokens: 70,
                output_tokens: 15,
                cache_creation_input_tokens: 2,
                cache_read_input_tokens: 100,
                ..Default::default()
            }
        );
        assert_eq!(outcome.turns, Some(2));
        assert_eq!(outcome.cost_usd, Some(0.0123));
        assert_eq!(outcome.session_id.as_deref(), Some("s-1"));
        assert_eq!(outcome.end.unwrap(), End::Answered);
    }

    #[test]
    fn warns_once_about_usage_it_cannot_read() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut report = collector(&seen);
        let mut stream = Stream::default();
        for id in ["msg_1", "msg_2"] {
            let event = json!({"type": "assistant", "parent_tool_use_id": null,
                "message": {"id": id, "content": [],
                "usage": {"input_tokens": null, "output_tokens": 3}}});
            assert!(!stream.handle(&event.to_string(), &mut report));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(
            seen[0].starts_with("warn cannot read the token usage of a Claude Code response"),
            "{seen:?}"
        );
    }

    fn end_of(lines: &[Value], stderr: &str) -> Result<End> {
        let mut stream = Stream::default();
        for line in lines {
            stream.handle(&line.to_string(), &mut |_| {});
        }
        stream
            .outcome(Ok(Exit {
                status: ExitStatus::default(),
                stopped: None,
                stderr: stderr.into(),
            }))
            .end
    }

    #[test]
    fn tells_login_failures_from_other_failures() {
        let not_logged_in = json!({"type": "result", "subtype": "success", "is_error": true,
            "result": "Not logged in · Please run /login"});
        let err = end_of(&[not_logged_in], "").unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{err:#}");
        assert!(
            format!("{err:#}").contains("run `claude` and then /login"),
            "{err:#}"
        );

        let refused = json!({"type": "assistant", "error": "authentication_failed",
            "parent_tool_use_id": null, "message": {"content": [{"type": "text",
            "text": "Failed to authenticate. API Error: 401"}]}});
        let failed =
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true});
        let err = end_of(&[refused, failed.clone()], "").unwrap_err();
        assert!(
            format!("{err:#}").contains("(Failed to authenticate. API Error: 401)"),
            "{err:#}"
        );
        assert!(err.downcast_ref::<NotLoggedIn>().is_some());

        let err = end_of(&[], "Invalid API key · Fix external API key").unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{err:#}");

        // Only the latest failure counts: a refused login, then another
        // error or a response, is no login failure.
        let retry = |error: &str| {
            json!({"type": "system", "subtype": "api_retry", "attempt": 1,
                "max_retries": 10, "error": error})
        };
        let answer = json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"content": [{"type": "text", "text": "hi"}]}});
        for later in [retry("overloaded"), answer] {
            let err = end_of(
                &[
                    retry("authentication_failed"),
                    later.clone(),
                    failed.clone(),
                ],
                "",
            )
            .unwrap_err();
            assert!(err.downcast_ref::<NotLoggedIn>().is_none(), "{later}");
        }
        for error in ["oauth_org_not_allowed", "account_on_hold"] {
            let err = end_of(&[retry("overloaded"), retry(error), failed.clone()], "").unwrap_err();
            assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{error}");
        }

        // Retries that end in success are no failure.
        let retry = retry("authentication_failed");
        let success =
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "ok"});
        assert_eq!(end_of(&[retry, success], "").unwrap(), End::Answered);

        let turns = json!({"type": "result", "subtype": "error_max_turns", "is_error": true});
        assert_eq!(end_of(&[turns], "").unwrap(), End::TurnLimit);

        let err = end_of(&[failed], "boom").unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_none());
        assert!(
            format!("{err:#}")
                .starts_with("Claude Code ended the session with `error_during_execution`"),
            "{err:#}"
        );
        let err = end_of(&[], "segfault").unwrap_err();
        assert!(
            format!("{err:#}").contains("without a result; stderr: segfault"),
            "{err:#}"
        );
    }

    #[test]
    fn tells_the_usage_limit_from_other_failures() {
        // As Claude Code reports a refused request: the event with the
        // reset time, the model call's error, then the result.
        let refused = |resets_at: Value| {
            json!({"type": "rate_limit_event", "session_id": "s",
                "rate_limit_info": {"status": "rejected", "resetsAt": resets_at}})
        };
        let call = json!({"type": "assistant", "error": "rate_limit",
            "parent_tool_use_id": null, "message": {"content": [{"type": "text",
            "text": "You've hit your session limit · resets 3:45pm"}]}});
        let result = json!({"type": "result", "subtype": "success", "is_error": true,
            "api_error_status": 429, "result": "You've hit your session limit · resets 3:45pm"});
        let limit = |resets_at| {
            End::UsageLimit(UsageLimit {
                resets_at,
                message: "You've hit your session limit · resets 3:45pm".into(),
            })
        };
        assert_eq!(
            end_of(
                &[refused(json!(1_794_205_024)), call.clone(), result.clone()],
                ""
            )
            .unwrap(),
            limit(Some(1_794_205_024))
        );
        // Each signal alone is enough; only the event has the reset time.
        assert_eq!(
            end_of(
                &[
                    call.clone(),
                    json!({"type": "result", "subtype": "error_during_execution",
                "is_error": true})
                ],
                ""
            )
            .unwrap(),
            limit(None)
        );
        assert_eq!(
            end_of(std::slice::from_ref(&result), "").unwrap(),
            limit(None)
        );
        let End::UsageLimit(alone) = end_of(&[refused(json!(1_794_205_024))], "").unwrap() else {
            panic!("not a usage limit");
        };
        assert_eq!(alone.resets_at, Some(1_794_205_024));
        assert_eq!(alone.message, "requests refused by the usage limit");
        let End::UsageLimit(retried) = end_of(
            &[
                json!({"type": "system", "subtype": "api_retry", "attempt": 3,
                "max_retries": 10, "error": "rate_limit"}),
            ],
            "",
        )
        .unwrap() else {
            panic!("not a usage limit");
        };
        assert_eq!(retried.message, "rate_limit");
        // A reset time Junees cannot read is none.
        let End::UsageLimit(odd) = end_of(&[refused(json!("3:45pm")), result.clone()], "").unwrap()
        else {
            panic!("not a usage limit");
        };
        assert_eq!(odd.resets_at, None);
        // Only bought credits help.
        let credits = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "rejected",
            "errorCode": "credits_required", "canUserPurchaseCredits": true}});
        let End::UsageLimit(credits) = end_of(&[credits, result.clone()], "").unwrap() else {
            panic!("not a usage limit");
        };
        assert!(
            credits.message.contains("`credits_required`"),
            "{}",
            credits.message
        );

        // A session that got past the limit, or a limit that lifted, is no
        // usage limit; neither is an overloaded server.
        let success =
            json!({"type": "result", "subtype": "success", "is_error": false, "result": "ok"});
        assert_eq!(
            end_of(&[refused(json!(1)), call.clone(), success], "").unwrap(),
            End::Answered
        );
        let answer = json!({"type": "assistant", "parent_tool_use_id": null,
            "message": {"content": [{"type": "text", "text": "hi"}]}});
        let failed =
            json!({"type": "result", "subtype": "error_during_execution", "is_error": true});
        let allowed = json!({"type": "rate_limit_event",
            "rate_limit_info": {"status": "allowed_warning", "resetsAt": 1_794_205_024}});
        for later in [answer, allowed] {
            let err = end_of(
                &[
                    call.clone(),
                    refused(json!(1)),
                    later.clone(),
                    failed.clone(),
                ],
                "",
            )
            .unwrap_err();
            assert!(
                format!("{err:#}").contains("error_during_execution"),
                "{later}"
            );
        }
        let overloaded = json!({"type": "assistant", "error": "overloaded",
            "parent_tool_use_id": null, "message": {"content": []}});
        assert!(end_of(&[overloaded, failed], "").is_err());
    }

    #[test]
    fn the_stream_warns_when_the_limit_refuses_requests() {
        let mut stream = Stream::default();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut report = collector(&seen);
        for status in ["allowed", "rejected"] {
            let event = json!({"type": "rate_limit_event", "rate_limit_info":
                {"status": status, "resetsAt": 1_794_205_024}});
            stream.handle(&event.to_string(), &mut report);
        }
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "warn Claude Code reports the usage limit reached; requests are refused \
                 until 2026-11-09 06:17 UTC"
            ]
        );
    }

    #[test]
    fn a_usage_limit_names_the_reset() {
        let outcome = Outcome {
            end: Ok(End::UsageLimit(UsageLimit {
                resets_at: Some(1_794_205_024),
                message: "You've hit your session limit · resets 6:17am".into(),
            })),
            usage: Usage::default(),
            cost_usd: None,
            turns: None,
            session_id: None,
        };
        assert!(
            outcome
                .summary()
                .starts_with("stopped at the usage limit, ")
        );
        let Ok(End::UsageLimit(limit)) = outcome.end else {
            unreachable!()
        };
        assert_eq!(
            limit.to_string(),
            "Claude Code hit the usage limit (resets 2026-11-09 06:17 UTC): \
             You've hit your session limit · resets 6:17am"
        );
    }

    #[tokio::test]
    async fn runs_a_session_and_saves_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            dir.path(),
            &format!(
                "[ \"$1\" = --version ] && exit 3\n\
                 printf '%s\\0' \"$@\" > args\n\
                 echo \"${{CLAUDE_CODE_SUBPROCESS_ENV_SCRUB-unset}}\" > scrub\n\
                 echo made > made.txt\n\
                 echo 'not json'\n\
                 {}",
                print(SESSION)
            ),
        );
        let settings = ToolSettings::default();
        let log = dir.path().join("out/session-1.jsonl");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = claude(bin)
            .run(
                &request(dir.path(), &settings, Some(&log)),
                &Cancel::new(),
                None,
                &mut collector(&seen),
            )
            .await;
        assert_eq!(outcome.end.unwrap(), End::Answered);
        assert_eq!(tokens(outcome.usage), 187);
        assert!(dir.path().join("made.txt").exists());
        // Claude Code keeps credentials from the commands it runs.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("scrub")).unwrap(),
            "1\n"
        );
        let args = std::fs::read(dir.path().join("args")).unwrap();
        let args: Vec<String> = args
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        assert_eq!(
            args,
            claude(PathBuf::new()).arguments(&request(dir.path(), &settings, None))
        );
        let saved = std::fs::read_to_string(&log).unwrap();
        assert_eq!(saved, format!("not json\n{SESSION}\n"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], "warn claude: not json");
        assert_eq!(seen[1], "text Looking.");
    }

    #[tokio::test]
    async fn checks_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ToolSettings::default();
        let current = claude(fake_claude(dir.path(), ""));
        assert_eq!(
            current.check(dir.path(), &settings).await.unwrap(),
            "2.1.319 (Claude Code)"
        );
        let old = claude(fake_claude(
            dir.path(),
            "echo '2.1.100 (Claude Code)'; exit 0",
        ));
        let err = old.check(dir.path(), &settings).await.unwrap_err();
        assert!(
            format!("{err:#}")
                .starts_with("Claude Code 2.1.100 (Claude Code) is too old: Junees needs 2.1.259"),
            "{err:#}"
        );
        let broken = claude(fake_claude(dir.path(), "echo nope >&2; exit 2"));
        let err = broken.check(dir.path(), &settings).await.unwrap_err();
        assert!(format!("{err:#}").contains("--version` failed"), "{err:#}");
        assert!(format!("{err:#}").ends_with("nope"), "{err:#}");
        let missing = claude(dir.path().join("no-such-claude"));
        let err = missing.check(dir.path(), &settings).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("install Claude Code"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn only_a_subscription_sign_in_passes_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ToolSettings::default();
        let answering = |json: &str, code: i32| {
            claude(fake_claude_auth(
                dir.path(),
                &format!("echo '{json}'; exit {code}"),
                "",
            ))
        };
        for method in ["claude.ai", "oauth_token"] {
            let json = format!(r#"{{"loggedIn":true,"authMethod":"{method}"}}"#);
            answering(&json, 0).check_auth(&settings).await.unwrap();
        }
        for method in ["api_key", "api_key_helper", "third_party", "something_new"] {
            let json = format!(r#"{{"loggedIn":true,"authMethod":"{method}"}}"#);
            let err = answering(&json, 0).check_auth(&settings).await.unwrap_err();
            assert!(err.downcast_ref::<NotLoggedIn>().is_none(), "{err:#}");
            assert!(
                format!("{err:#}").contains(&format!("signs in with `{method}`")),
                "{err:#}"
            );
        }
        for json in [
            r#"{"loggedIn":false,"authMethod":"none"}"#,
            r#"{"loggedIn":false}"#,
        ] {
            let err = answering(json, 1).check_auth(&settings).await.unwrap_err();
            assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{err:#}");
        }
        // A subscription login `auth status` fails on is no login.
        let json = r#"{"loggedIn":false,"authMethod":"claude.ai"}"#;
        let err = answering(json, 1).check_auth(&settings).await.unwrap_err();
        assert!(err.downcast_ref::<NotLoggedIn>().is_some(), "{err:#}");
        let err = answering("nonsense", 2)
            .check_auth(&settings)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").starts_with("cannot tell how Claude Code signs in"),
            "{err:#}"
        );

        // `check` asks too, after the version.
        let json = r#"{"loggedIn":true,"authMethod":"api_key"}"#;
        let err = answering(json, 0)
            .check(dir.path(), &settings)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("signs in with `api_key`"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn every_session_checks_the_sign_in_first() {
        let dir = tempfile::tempdir().unwrap();
        // Records where and how it was asked; the session records that it ran.
        let calls = dir.path().join("auth-calls");
        let ran = dir.path().join("ran");
        let fake = |method: &str| {
            claude(fake_claude_auth(
                dir.path(),
                &format!(
                    "printf '%s\\n' \"$PWD\" \"$*\" \"${{{SCRUB_ENV}-unset}}\" >> '{}'\n\
                     echo '{{\"loggedIn\":true,\"authMethod\":\"{method}\"}}'; exit 0",
                    calls.display()
                ),
                &format!("touch '{}'\n{}", ran.display(), print(SESSION)),
            ))
        };
        let settings = ToolSettings::default();
        let request = request(dir.path(), &settings, None);
        let run = async |claude: ClaudeCode| {
            claude
                .run(&request, &Cancel::new(), None, &mut |_| {})
                .await
        };
        let outcome = run(fake("api_key_helper")).await;
        let err = outcome.end.unwrap_err();
        assert!(
            format!("{err:#}").contains("signs in with `api_key_helper`"),
            "{err:#}"
        );
        assert!(!ran.exists(), "the session ran");
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            "/\nauth status --json\nunset\n"
        );

        let outcome = run(fake("claude.ai")).await;
        assert_eq!(outcome.end.unwrap(), End::Answered);
        assert!(ran.exists());
    }

    /// Whether a process is gone (a zombie counts as gone).
    fn gone(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    async fn until_gone(pid: &str) {
        for _ in 0..100 {
            if gone(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("process {pid} is still there");
    }

    /// A session that starts long commands, prints its first response (then
    /// creates `printed`) and waits; `trap` runs first. Like Claude Code's
    /// Bash tool, it starts one command detached, in a session of its own
    /// (`setsid`), and that command starts another. Their pids are in the
    /// files [`COMMANDS`]: in Claude Code's group, detached, its child.
    fn hanging(trap: &str) -> String {
        format!(
            "{trap}\nsleep 30 &\necho $! > pid\n\
             setsid bash -c 'sleep 30 & echo $! > gpid; wait' &\necho $! > dpid\n\
             until [ -s gpid ]; do sleep 0.01; done\n\
             {}\ntouch printed\nwait\nsleep 30\n",
            print(&SESSION.lines().take(2).collect::<Vec<_>>().join("\n"))
        )
    }

    /// The pid files of [`hanging`].
    const COMMANDS: [&str; 3] = ["pid", "dpid", "gpid"];

    async fn commands_gone(dir: &Path) {
        for name in COMMANDS {
            until_gone(&wait_for(&dir.join(name)).await).await;
        }
    }

    async fn wait_for(path: &Path) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && (text.ends_with('\n') || path.ends_with("printed"))
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{} never appeared", path.display());
    }

    #[tokio::test]
    async fn cancel_stops_the_session_and_its_commands() {
        for trap in ["", "trap '' INT"] {
            let dir = tempfile::tempdir().unwrap();
            let bin = fake_claude(dir.path(), &hanging(trap));
            let settings = ToolSettings::default();
            let cancel = Cancel::new();
            let claude = claude(bin);
            let request = request(dir.path(), &settings, None);
            let stop = async {
                wait_for(&dir.path().join("printed")).await;
                cancel.cancel();
            };
            let mut report = |_: Report<'_>| {};
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(claude.run(&request, &cancel, None, &mut report), stop)
            })
            .await
            .expect("the session did not stop");
            assert_eq!(outcome.end.unwrap(), End::Interrupted, "trap `{trap}`");
            assert_eq!(tokens(outcome.usage), 115);
            commands_gone(dir.path()).await;
        }
    }

    #[tokio::test]
    async fn the_budget_stops_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path(), &hanging(""));
        let settings = ToolSettings::default();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut limit: Limit = Box::new({
            let asked = Arc::clone(&asked);
            move |usage| {
                asked.lock().unwrap().push(tokens(usage));
                (tokens(usage) > 100).then(|| "token budget used up".to_string())
            }
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            claude(bin).run(
                &request(dir.path(), &settings, None),
                &Cancel::new(),
                Some(&mut limit),
                &mut collector(&seen),
            ),
        )
        .await
        .expect("the budget did not stop the session");
        assert_eq!(
            outcome.end.unwrap(),
            End::Budget("token budget used up".into())
        );
        assert_eq!(asked.lock().unwrap().last(), Some(&115));
        assert!(
            seen.lock()
                .unwrap()
                .contains(&"warn token budget used up; stopping Claude Code".to_string())
        );
        commands_gone(dir.path()).await;
    }

    #[tokio::test]
    async fn a_missing_program_fails_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ToolSettings::default();
        let outcome = claude(dir.path().join("nothing"))
            .run(
                &request(dir.path(), &settings, None),
                &Cancel::new(),
                None,
                &mut |_| {},
            )
            .await;
        let err = outcome.end.unwrap_err();
        assert!(format!("{err:#}").starts_with("cannot run `"), "{err:#}");
        assert_eq!(outcome.usage, Usage::default());
    }
}
