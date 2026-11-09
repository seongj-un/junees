//! The `claude-code` backend: work run by the official Claude Code CLI,
//! signed in with the user's own Claude subscription. Task sessions use it
//! (see [`crate::task::claude_code`]).
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
//! A session is one `claude -p` run with `--output-format stream-json`
//! ([`ClaudeCode::session`]). Its events are shown like an agent's (text,
//! `-> tool`, `<- tool`, see [`stream`]), saved raw to a file, and counted.
//! [`rules`] turn Junees's command policy into Claude Code deny rules.

pub mod chat;
pub mod rules;
mod stream;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::Instant;

use crate::agent::{Event, Limit};
use crate::cancel::Cancel;
use crate::cli::preview;
use crate::message::Usage;
use crate::task::budget::tokens;
use crate::task::git::REPO_ENV_VARS;
use crate::tools::{SECRET_ENV_VARS, ToolSettings};

pub use rules::{deny_rules, settings_rules};
use stream::Stream;

/// Where a session's events go: shown to the user like an agent's.
pub type Sink<'a> = dyn FnMut(Event<'_>) + 'a;

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

/// How to start one session for [`ClaudeCode::session`].
pub struct Spawn<'a> {
    /// Where `claude` runs.
    pub root: &'a Path,
    /// Every argument of `claude`.
    pub args: Vec<String>,
    /// The user's hidden variables (removed from `claude`'s environment).
    pub settings: &'a ToolSettings,
    /// Where the raw stream-json output is saved, if anywhere.
    pub log: Option<&'a Path>,
    /// `args` hold `--include-partial-messages`: text is shown as it comes.
    pub partial: bool,
    /// `args` hold `--resume`: the session continues an earlier one.
    pub resumed: bool,
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

impl std::error::Error for UsageLimit {}

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
    /// The final answer (the `result` text), when the session answered.
    pub answer: Option<String>,
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
    /// [`BIN_ENV`] and [`MODEL_ENV`]; the settings file from
    /// `JUNEES_HOME`/`HOME`, relative to the current folder if relative.
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

    /// Runs one session to its end. `cancel` and `limit` stop it (SIGINT to
    /// Claude Code's process group, which ends its turn, and after
    /// [`ClaudeCode::grace`] SIGKILL); `limit` is asked after each model
    /// response and every second, with the tokens the session used so far.
    /// [`ClaudeCode::check_auth`] runs first.
    pub async fn session(
        &self,
        spawn: &Spawn<'_>,
        cancel: &Cancel,
        limit: Option<&mut Limit>,
        sink: &mut Sink<'_>,
    ) -> Outcome {
        let mut stream = Stream::new(spawn.partial, spawn.resumed);
        let exit = self.drive(spawn, cancel, limit, &mut stream, sink).await;
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
        spawn: &Spawn<'_>,
        cancel: &Cancel,
        mut limit: Option<&mut Limit>,
        stream: &mut Stream,
        report: &mut Sink<'_>,
    ) -> Result<Exit> {
        self.check_auth(spawn.settings).await?;
        let mut command = self.command(spawn.root, spawn.settings);
        command.args(&spawn.args);
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
        let mut log = spawn.log.and_then(|path| match open_log(path) {
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
fn save(log: &mut Option<File>, line: &[u8], report: &mut Sink<'_>) {
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

fn warn(report: &mut Sink<'_>, text: &str) {
    report(Event::Warning(text));
}

#[cfg(test)]
pub(crate) mod testing {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

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

    /// How the tests run Claude Code: a short grace, the scrub on.
    pub fn claude(bin: PathBuf) -> super::ClaudeCode {
        super::ClaudeCode {
            bin,
            model: None,
            grace: std::time::Duration::from_millis(300),
            scrub: true,
        }
    }

    /// Collects what a session reports, one string per event.
    pub fn events(seen: &Arc<Mutex<Vec<String>>>) -> impl FnMut(crate::agent::Event<'_>) + use<> {
        use crate::agent::Event;
        let seen = Arc::clone(seen);
        move |event| {
            let text = match event {
                Event::TextDelta(text) => format!("text {text}"),
                Event::Warning(text) => format!("warn {text}"),
                Event::ToolCall { name, input } => format!("-> {name} {input}"),
                Event::ToolResult { name, output } => {
                    format!("<- {name} {} {}", output.is_error, output.content)
                }
                _ => return,
            };
            seen.lock().unwrap().push(text);
        }
    }

    /// The output of a session that reads a file and answers: two events
    /// for the first response (one per block, same id and usage), a tool
    /// result, the answer and the result.
    pub const SESSION: &str = r#"{"type":"system","subtype":"init","session_id":"s-1","model":"claude-x"}
{"type":"assistant","parent_tool_use_id":null,"session_id":"s-1","message":{"id":"msg_1","content":[{"type":"text","text":"Looking."}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100}}}
{"type":"assistant","parent_tool_use_id":null,"session_id":"s-1","message":{"id":"msg_1","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.txt"}}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100}}}
{"type":"user","parent_tool_use_id":null,"session_id":"s-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"hello"}]}]}}
{"type":"assistant","parent_tool_use_id":null,"session_id":"s-1","message":{"id":"msg_2","content":[{"type":"text","text":"Done."}],"usage":{"input_tokens":20,"output_tokens":7}}}
{"type":"result","subtype":"success","is_error":false,"result":"Done.","num_turns":2,"total_cost_usd":0.0123,"session_id":"s-1","usage":{"input_tokens":30,"output_tokens":12,"cache_read_input_tokens":100},"modelUsage":{"claude-x":{"inputTokens":30,"outputTokens":12,"cacheReadInputTokens":100,"cacheCreationInputTokens":0},"claude-small":{"inputTokens":40,"outputTokens":3,"cacheReadInputTokens":0,"cacheCreationInputTokens":2}}}"#;

    /// A fake's body that prints `lines`.
    pub fn print(lines: &str) -> String {
        format!("cat <<'JSON'\n{lines}\nJSON")
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{claude, fake_claude, fake_claude_auth};
    use super::*;

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
}
