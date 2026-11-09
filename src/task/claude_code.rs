//! The `claude-code` backend: task sessions run by the official Claude Code
//! CLI, signed in with the user's own Claude subscription.
//!
//! Anthropic lets a subscription drive other programs only through the
//! unmodified `claude` program, signed in through Anthropic's own flow. So
//! Junees only starts that program as a child process. It never reads,
//! copies, stores or sends Claude credentials (`~/.claude`,
//! `CLAUDE_CODE_OAUTH_TOKEN`), and its own API client does not pretend to be
//! Claude Code. A `CLAUDE_CODE_OAUTH_TOKEN` in the user's environment
//! reaches `claude` the way every other variable does; the API credentials
//! in [`API_ENV_VARS`] do not, because Claude Code would bill them instead
//! of the subscription.
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

/// The oldest Claude Code that knows every flag Junees passes:
/// `--permission-prompts` came in 2.1.259.
pub const MIN_VERSION: [u32; 3] = [2, 1, 259];

/// What a user whose Claude Code is not signed in has to do. Junees never
/// signs in for them: that has to go through Anthropic's own flow.
pub const LOGIN_HINT: &str = "sign in to Claude Code yourself: run `claude` and then /login \
                              (or create a token with `claude setup-token`), then start again";

/// The built-in tools a session gets, all of them allowed without asking.
pub const TOOLS: &str = "Read,Edit,Write,Glob,Grep,Bash";

/// Variables Claude Code would use as API credentials or an API endpoint
/// instead of the subscription login. They never reach `claude`, nor do
/// the secrets bash never sees and the repository variables.
pub const API_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

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
/// Error codes of Claude Code's stream that mean its login was refused.
const AUTH_ERRORS: &[&str] = &["authentication_failed"];
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
        }
    }

    /// Checks that `claude --version` runs and is at least
    /// [`MIN_VERSION`]; returns what it printed. Whether it is signed in
    /// shows in the first session.
    pub async fn check(&self, root: &Path, settings: &ToolSettings) -> Result<String> {
        let bin = self.bin.display();
        let mut command = self.command(root, settings);
        command.arg("--version");
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
        Ok(stdout)
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
        let mut command = self.command(request.root, request.settings);
        command.args(self.arguments(request));
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
        // What a stopped session left running goes with it; a session that
        // ended on its own keeps its background processes, like bash.
        if stopped.is_some() {
            group.kill();
        } else {
            group.disarm();
        }
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

/// Claude Code's process group (it runs in its own), killed when dropped
/// unless disarmed.
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

    /// Asks Claude Code to end its turn; returns when to kill it instead.
    fn interrupt(&self, grace: Duration) -> Instant {
        self.signal(libc::SIGINT);
        Instant::now() + grace
    }

    fn kill(&self) {
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
    /// What a refused login said.
    auth: Option<String>,
    result: Option<Finished>,
    /// The last lines that were not JSON.
    plain: Vec<String>,
}

/// The `result` event.
struct Finished {
    subtype: String,
    is_error: bool,
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
            _ => {}
        }
        false
    }

    fn assistant(&mut self, event: &Value, report: &mut Reporter<'_>) -> bool {
        let message = &event["message"];
        let main = event["parent_tool_use_id"].is_null();
        if let Some(error) = event["error"].as_str()
            && AUTH_ERRORS.contains(&error)
        {
            let text = content_text(&message["content"]);
            self.auth = Some(if text.is_empty() {
                error.to_string()
            } else {
                text
            });
        }
        let mut changed = false;
        if message.get("usage").is_some_and(Value::is_object)
            && let Ok(usage) = serde_json::from_value::<Usage>(message["usage"].clone())
        {
            match message["id"].as_str() {
                Some(id) => {
                    self.usage.insert(id.to_string(), usage);
                }
                None => self.unnamed.add(usage),
            }
            changed = true;
        }
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") if main => {
                    let text = block["text"].as_str().unwrap_or_default().trim();
                    if !text.is_empty() {
                        report(Report::Event(Event::Text(text)));
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
                if AUTH_ERRORS.contains(&error) {
                    self.auth = Some(error.to_string());
                }
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

    /// Writes an executable bash script standing in for `claude`, and
    /// waits until it can be run: a test that forks while the file is open
    /// for writing would make `exec` fail with ETXTBSY. `--version` prints
    /// a current version unless `body` answers it first.
    pub fn fake_claude(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fake-claude");
        std::fs::write(
            &path,
            format!(
                "#!/bin/bash\n\
                 [ -n \"$JUNEES_FAKE_PROBE\" ] && exit 0\n\
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

    use super::testing::fake_claude;
    use super::*;

    /// Collects what a session reports, one string per event.
    fn collector(seen: &Arc<Mutex<Vec<String>>>) -> impl FnMut(Report<'_>) + use<> {
        let seen = Arc::clone(seen);
        move |report| {
            let text = match report {
                Report::Line(text) => format!("line {text}"),
                Report::Event(Event::Text(text)) => format!("text {text}"),
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
                grace: DEFAULT_GRACE
            }
        );
        let claude = ClaudeCode::from_lookup(|key| match key {
            BIN_ENV => Some(" /opt/claude ".into()),
            MODEL_ENV => Some("opus".into()),
            _ => None,
        });
        assert_eq!(claude.bin, PathBuf::from("/opt/claude"));
        assert_eq!(claude.model.as_deref(), Some("opus"));
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
                cache_read_input_tokens: 100
            }
        );
        assert_eq!(outcome.turns, Some(2));
        assert_eq!(outcome.cost_usd, Some(0.0123));
        assert_eq!(outcome.session_id.as_deref(), Some("s-1"));
        assert_eq!(outcome.end.unwrap(), End::Answered);
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

        // Retries that end in success are no failure.
        let retry = json!({"type": "system", "subtype": "api_retry", "attempt": 1,
            "max_retries": 10, "error": "authentication_failed"});
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

    #[tokio::test]
    async fn runs_a_session_and_saves_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            dir.path(),
            &format!(
                "[ \"$1\" = --version ] && exit 3\n\
                 printf '%s\\0' \"$@\" > args\n\
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

    /// A session that starts a long command (its pid in the file `pid`),
    /// prints its first response (then creates `printed`) and waits; `trap`
    /// runs first.
    fn hanging(trap: &str) -> String {
        format!(
            "{trap}\nsleep 30 &\necho $! > pid\n{}\ntouch printed\nwait\nsleep 30\n",
            print(&SESSION.lines().take(2).collect::<Vec<_>>().join("\n"))
        )
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
                std::fs::read_to_string(dir.path().join("pid")).unwrap()
            };
            let mut report = |_: Report<'_>| {};
            let (outcome, pid) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(claude.run(&request, &cancel, None, &mut report), stop)
            })
            .await
            .expect("the session did not stop");
            assert_eq!(outcome.end.unwrap(), End::Interrupted, "trap `{trap}`");
            assert_eq!(tokens(outcome.usage), 115);
            until_gone(&pid).await;
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
        until_gone(&wait_for(&dir.path().join("pid")).await).await;
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
