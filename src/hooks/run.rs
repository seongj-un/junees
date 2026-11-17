//! Running hooks: one JSON object on stdin, the session's working folder,
//! the bash tool's environment rules and sandbox, a time limit, and what
//! the exit code and stdout say.
//!
//! - exit 0: go on; stdout `{"message": "..."}` adds the text as a note
//!   for the model.
//! - exit 2 (`pre_tool`, `user_prompt`): block; the first 2 KB of stderr
//!   are the reason.
//! - anything else, a time-out, a program that does not start: the
//!   hook's `on_error` (allow, with a warning, by default).
//! - a pinned file that changed or cannot be checked: the hook does not
//!   run, and a hook that can block blocks whatever its `on_error` says
//!   (the file may have been tampered with).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use super::config::{Event, OnError, Spec};
use super::{Hook, Origin};
use crate::prompt_text::escape_hidden;
use crate::sandbox::{Prepared, Sandbox};
use crate::tools::{CLAUDE_CODE_SECRET_ENV_VARS, Capture, GroupGuard, SECRET_ENV_VARS};

/// The exit code with which a `pre_tool` or `user_prompt` hook blocks.
pub const BLOCK: i32 = 2;
/// Bytes of a tool result's text a hook gets, and of its stdout read.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// Bytes of a blocking hook's stderr kept as the reason.
pub const MAX_REASON_BYTES: usize = 2 * 1024;
/// Characters of one `message` passed on to the model.
pub const MAX_MESSAGE_CHARS: usize = 4000;
/// How long to keep reading after the command exits, for output still in
/// the pipes.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

/// Hides known secret values in a text, exactly where they are
/// ([`crate::doctor::hide_known_secrets`]).
pub type Hide = Arc<dyn Fn(&mut String) + Send + Sync>;

/// What a hook is told about.
#[derive(Default)]
pub struct Input<'a> {
    /// The session's id, once its journal exists.
    pub session_id: Option<&'a str>,
    pub tool: Option<(&'a str, &'a Value)>,
    /// A tool's result (`post_tool`) or how a turn ended (`turn_end`):
    /// whether it is an error, and its text.
    pub result: Option<(bool, &'a str)>,
    /// The person's input (`user_prompt`).
    pub prompt: Option<&'a str>,
}

/// How a hook ended, as far as Junees goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Passed,
    /// Exit 2 of a blocking hook, with the reason.
    Blocked(String),
    /// It did not finish within its time limit and was killed.
    TimedOut(Duration),
    /// Any other end, or it could not start: why.
    Failed(String),
}

impl Outcome {
    /// `passed`, `blocked`, `timed_out` or `failed`.
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Blocked(_) => "blocked",
            Outcome::TimedOut(_) => "timed_out",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// One hook that ran: what the session record, the dashboard and the
/// person are told.
#[derive(Debug, Clone)]
pub struct Record {
    pub event: Event,
    /// Its command, shown on one line.
    pub command: String,
    /// `user` or `project`.
    pub origin: &'static str,
    pub outcome: Outcome,
    pub took: Duration,
    /// Its `message` for the model, cleaned.
    pub message: Option<String>,
    /// Its failure blocked (`on_error = "block"`, or a pinned file
    /// that failed its check at a point that can block).
    pub failure_blocked: bool,
    /// It did not run because a file it runs failed its check: what
    /// happened, in the words of the record line ([`super::CHANGED`],
    /// [`super::NOT_PINNED`]).
    pub pin_failure: Option<&'static str>,
}

impl Record {
    /// The line in the session record (a note, which the model reads too).
    pub fn line(&self) -> String {
        let result = match &self.outcome {
            Outcome::Passed => "통과".to_string(),
            Outcome::Blocked(_) => "막음".to_string(),
            Outcome::TimedOut(limit) => format!("시간 초과({}초)", limit.as_secs_f64()),
            Outcome::Failed(_) => match self.pin_failure {
                Some(what) => format!("실패, {what}"),
                None => "실패".to_string(),
            },
        };
        let result = match (self.failure_blocked, self.pin_failure) {
            (false, _) => result,
            (true, Some(_)) => format!("{result}, 확인하지 못한 파일이라 막음"),
            (true, None) => format!("{result}, 실패 시 막는 설정이라 막음"),
        };
        format!(
            "[훅] {} `{}`: {result} ({:.2}초)",
            self.event.name(),
            self.command,
            self.took.as_secs_f64()
        )
    }

    /// The dashboard's `hook` event.
    pub fn json(&self) -> Value {
        let mut value = json!({
            "type": "hook",
            "event": self.event.name(),
            "command": self.command,
            "origin": self.origin,
            "result": self.outcome.name(),
            "took_ms": u64::try_from(self.took.as_millis()).unwrap_or(u64::MAX),
        });
        if let Some(message) = &self.message {
            value["message"] = json!(message);
        }
        match &self.outcome {
            Outcome::Blocked(reason) => value["reason"] = json!(reason),
            Outcome::Failed(what) => value["error"] = json!(what),
            _ => {}
        }
        if self.failure_blocked {
            value["blocked"] = json!(true);
        }
        value
    }
}

/// What the hooks of one point decided.
#[derive(Debug, Default)]
pub struct Fired {
    /// Set when one blocked: the reason, for the model or the person.
    pub blocked: Option<String>,
    /// Lines for the session record and the model: one per hook that ran,
    /// and the messages.
    pub notes: Vec<String>,
    /// The hooks that ran, in order.
    pub records: Vec<Record>,
    /// Failures that did not block, for the person.
    pub warnings: Vec<String>,
}

/// The hooks of a run and what their commands get.
pub struct Hooks {
    hooks: Vec<Hook>,
    /// The session's working folder: where they run, and `cwd`.
    workdir: PathBuf,
    /// Variables hidden from them on top of the secret ones
    /// (`JUNEES_HIDDEN_ENV`).
    hidden_env: Vec<String>,
    /// The bash tool's sandbox, when it has one.
    sandbox: Option<Arc<Sandbox>>,
    hide: Hide,
    /// Runs every command in this folder as the bash tests do
    /// ([`crate::tools::testing::confine`]).
    #[cfg(test)]
    pub(crate) test_dir: Option<PathBuf>,
}

impl Hooks {
    pub fn new(
        hooks: Vec<Hook>,
        workdir: &Path,
        hidden_env: &[String],
        sandbox: Option<Sandbox>,
        hide: Hide,
    ) -> Self {
        Self {
            hooks,
            workdir: workdir.to_path_buf(),
            hidden_env: hidden_env.to_vec(),
            sandbox: sandbox.map(Arc::new),
            hide,
            #[cfg(test)]
            test_dir: None,
        }
    }

    pub fn hooks(&self) -> &[Hook] {
        &self.hooks
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Whether some hook runs at `event`.
    pub fn has(&self, event: Event) -> bool {
        self.hooks.iter().any(|hook| hook.spec.event == event)
    }

    /// Runs the hooks of `event` (for a tool call: those whose `match`
    /// takes the tool's name), one at a time in their order, until one
    /// blocks.
    pub async fn fire(&self, event: Event, input: Input<'_>) -> Fired {
        let mut fired = Fired::default();
        let tool = input.tool.map(|(name, _)| name);
        let selected: Vec<&Hook> = self
            .hooks
            .iter()
            .filter(|hook| hook.spec.event == event && hook.spec.applies_to(tool))
            .collect();
        if selected.is_empty() {
            return fired;
        }
        let payload = self.payload(event, &input);
        for hook in selected {
            let (outcome, message, took, pin_failure) =
                self.run(hook, event, input.session_id, &payload).await;
            let failed = matches!(outcome, Outcome::TimedOut(_) | Outcome::Failed(_));
            // A pinned file that changed is a sign of tampering, not a
            // broken hook: a policy hook the model disarms by editing its
            // script must still block.
            let failure_blocked = failed
                && event.can_block()
                && (hook.spec.on_error == OnError::Block || pin_failure.is_some());
            let record = Record {
                event,
                command: hook.spec.shown(),
                origin: hook.origin.kind(),
                outcome: outcome.clone(),
                took,
                message: message.clone(),
                failure_blocked,
                pin_failure,
            };
            fired.notes.push(record.line());
            if let Some(message) = &message {
                fired.notes.push(format!(
                    "[훅 {} `{}`] {message}",
                    event.name(),
                    record.command
                ));
            }
            fired.records.push(record);
            match outcome {
                Outcome::Blocked(reason) => {
                    fired.blocked = Some(format!(
                        "the {} hook `{}` blocked this: {reason}",
                        event.name(),
                        hook.spec.shown()
                    ));
                    break;
                }
                Outcome::TimedOut(_) | Outcome::Failed(_) => {
                    let what = failure(&outcome);
                    if failure_blocked && pin_failure.is_some() {
                        fired.blocked = Some(format!(
                            "the {} hook `{}` did not run ({what}); a hook whose file fails its \
                             check blocks whatever its on_error says, since the file may have \
                             been tampered with",
                            event.name(),
                            hook.spec.shown()
                        ));
                        break;
                    }
                    if failure_blocked {
                        fired.blocked = Some(format!(
                            "the {} hook `{}` failed ({what}), and it blocks when it fails \
                             (on_error = \"block\")",
                            event.name(),
                            hook.spec.shown()
                        ));
                        break;
                    }
                    fired.warnings.push(format!(
                        "{} hook `{}` ({}) failed ({what}); going on",
                        event.name(),
                        hook.spec.shown(),
                        origin_shown(&hook.origin)
                    ));
                }
                Outcome::Passed => {}
            }
        }
        fired
    }

    /// The JSON object the hooks of one point get on stdin, known secrets
    /// hidden.
    fn payload(&self, event: Event, input: &Input<'_>) -> Value {
        let hide = |text: &str| {
            let mut text = text.to_string();
            (self.hide)(&mut text);
            text
        };
        let mut payload = json!({
            "event": event.name(),
            "session_id": input.session_id,
            "cwd": self.workdir.display().to_string(),
        });
        if let Some((name, tool_input)) = input.tool {
            payload["tool"] = json!({"name": name, "input": hide_in(tool_input, &hide)});
        }
        if let Some((is_error, text)) = input.result {
            // Hidden first, then cut: a cut could leave a piece of a key
            // that no longer matches.
            payload["result"] = json!({
                "is_error": is_error,
                "text": head(&hide(text), MAX_TEXT_BYTES),
            });
        }
        if let Some(prompt) = input.prompt {
            payload["prompt"] = json!(hide(prompt));
        }
        payload
    }

    /// Runs one hook: how it ended, its message, how long it took, and
    /// what kept it from running when a file it runs failed its check.
    async fn run(
        &self,
        hook: &Hook,
        event: Event,
        session_id: Option<&str>,
        payload: &Value,
    ) -> (Outcome, Option<String>, Duration, Option<&'static str>) {
        let spec = &hook.spec;
        let started = tokio::time::Instant::now();
        if let Err(failure) = check_pin(hook) {
            let what = format!("{} ({})", failure.what, failure.detail);
            return (
                Outcome::Failed(what),
                None,
                started.elapsed(),
                Some(failure.what),
            );
        }
        let result = self.spawn(spec, event, session_id, payload).await;
        let took = started.elapsed();
        let (outcome, message) = match result {
            Err(why) => (Outcome::Failed(why), None),
            Ok((Exit::TimedOut, _, _)) => (Outcome::TimedOut(spec.timeout), None),
            Ok((Exit::Code(0), stdout, _)) => (Outcome::Passed, self.message(&stdout)),
            Ok((Exit::Code(BLOCK), _, stderr)) if event.can_block() => {
                let mut reason = head(stderr.trim(), MAX_REASON_BYTES).to_string();
                (self.hide)(&mut reason);
                let reason = escape_hidden(reason.trim());
                let reason = if reason.is_empty() {
                    "no reason given".to_string()
                } else {
                    reason
                };
                (Outcome::Blocked(reason), None)
            }
            Ok((exit, _, stderr)) => {
                let mut what = match exit {
                    Exit::Code(code) => format!("exit code {code}"),
                    _ => "terminated by a signal".to_string(),
                };
                if let Some(line) = last_line(&stderr) {
                    let mut line = line;
                    (self.hide)(&mut line);
                    what.push_str(&format!("; its stderr: {line}"));
                }
                (Outcome::Failed(what), None)
            }
        };
        (outcome, message, took, None)
    }

    /// The `message` of a hook's stdout, if it is `{"message": "..."}`:
    /// known secrets hidden, hidden characters escaped, cut.
    fn message(&self, stdout: &str) -> Option<String> {
        let value: Value = serde_json::from_str(stdout.trim()).ok()?;
        let mut message = value.get("message")?.as_str()?.trim().to_string();
        if message.is_empty() {
            return None;
        }
        (self.hide)(&mut message);
        let message = escape_hidden(&message);
        Some(crate::tools::truncate_middle(&message, MAX_MESSAGE_CHARS))
    }

    /// Starts the hook's command and waits for it.
    async fn spawn(
        &self,
        spec: &Spec,
        event: Event,
        session_id: Option<&str>,
        payload: &Value,
    ) -> Result<(Exit, String, String), String> {
        let shown = spec.shown();
        let program = PathBuf::from(&spec.command[0]);
        // A relative path is the session folder's, whatever Junees's own
        // working folder is.
        let program = if program.is_relative() && spec.command[0].contains('/') {
            self.workdir.join(program)
        } else {
            program
        };
        let mut process = tokio::process::Command::new(program);
        process
            .args(&spec.command[1..])
            .current_dir(&self.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(test)]
        if let Some(dir) = &self.test_dir {
            crate::tools::testing::confine(&mut process, dir);
        }
        for name in secret_names(&self.hidden_env) {
            process.env_remove(name);
        }
        process.env("JUNEES_HOOK_EVENT", event.name());
        match session_id {
            Some(id) => process.env("JUNEES_SESSION_ID", id),
            None => process.env_remove("JUNEES_SESSION_ID"),
        };
        let confinement = match &self.sandbox {
            None => None,
            Some(sandbox) => match sandbox.prepare(&self.workdir) {
                Prepared::Confined(confinement, _) => Some(confinement),
                Prepared::Unconfined(_) => None,
                Prepared::Refused(reason) => {
                    return Err(format!("the sandbox refused to run it: {reason}"));
                }
            },
        };
        match &confinement {
            // Starts a session of its own too.
            Some(confinement) => confinement.apply(&mut process),
            None => crate::tools::own_session(&mut process),
        }
        let spawned = process.spawn();
        drop(confinement);
        let mut child = spawned.map_err(|err| format!("cannot run `{shown}`: {err}"))?;
        let mut group = GroupGuard(child.id());
        let mut stdout = Capture::start(child.stdout.take());
        let mut stderr = Capture::start(child.stderr.take());
        // Written on the side: a command that does not read its input must
        // not block on a full pipe while Junees waits for it.
        let input = payload.to_string().into_bytes();
        let writer = child.stdin.take().map(|mut stdin| {
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
            })
        });
        let exit = match tokio::time::timeout(spec.timeout, child.wait()).await {
            Ok(Ok(status)) => status.code().map_or(Exit::Signal, Exit::Code),
            Ok(Err(err)) => return Err(format!("cannot wait for `{shown}`: {err}")),
            Err(_) => {
                group.kill();
                let _ = child.wait().await;
                Exit::TimedOut
            }
        };
        group.disarm();
        if let Some(writer) = writer {
            writer.abort();
        }
        let _ = tokio::time::timeout(DRAIN_GRACE, async {
            stdout.finished().await;
            stderr.finished().await;
        })
        .await;
        let stdout = head(&stdout.into_text(), MAX_TEXT_BYTES).to_string();
        Ok((exit, stdout, stderr.into_text()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Code(i32),
    Signal,
    TimedOut,
}

/// The variables a hook's process never gets: the secret ones the bash
/// tool hides, `hidden`, and every inherited `JUNEES_` variable named like
/// a credential.
fn secret_names(hidden: &[String]) -> Vec<String> {
    let mut names: Vec<String> = SECRET_ENV_VARS
        .iter()
        .chain(CLAUDE_CODE_SECRET_ENV_VARS)
        .map(|name| name.to_string())
        .chain(hidden.iter().cloned())
        .collect();
    for (name, _) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("JUNEES_")
            && ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                .iter()
                .any(|word| name.contains(word))
        {
            names.push(name.to_string());
        }
    }
    names
}

/// Why a hook did not run: a file it runs failed its check.
#[derive(Debug)]
struct PinFailure {
    /// What happened, in the record line's words.
    what: &'static str,
    /// Which file and why, in English.
    detail: String,
}

/// Refuses to run a hook whose pinned file ([`super::Pin`]) could not be
/// read when the hook was, or is no longer what it was then: the model may
/// have rewritten it during the session. (A swap between this check and
/// the start remains possible for a process already running there.)
fn check_pin(hook: &Hook) -> Result<(), PinFailure> {
    let Some(pin) = &hook.pin else {
        return Ok(());
    };
    let path = pin.path.display();
    match &pin.digest {
        Err(err) => Err(PinFailure {
            what: super::NOT_PINNED,
            detail: format!("{path} could not be pinned when the hook was read: {err}"),
        }),
        Ok(expected) => match super::pinned_digest(&pin.path) {
            Ok(now) if now == *expected => Ok(()),
            Ok(_) => Err(PinFailure {
                what: super::CHANGED,
                detail: format!("{path} changed since the hook was read"),
            }),
            Err(err) => Err(PinFailure {
                what: super::CHANGED,
                detail: format!("{path} cannot be read as it was pinned: {err}"),
            }),
        },
    }
}

/// Why a hook failed, in one line.
fn failure(outcome: &Outcome) -> String {
    match outcome {
        Outcome::TimedOut(limit) => format!(
            "it did not finish within {} and was killed",
            crate::mcp::server::seconds(*limit)
        ),
        Outcome::Failed(what) => what.clone(),
        _ => String::new(),
    }
}

fn origin_shown(origin: &Origin) -> String {
    format!("{} hook of {}", origin.kind(), origin.file().display())
}

/// The first `max` bytes of `text`, cut on a character boundary.
pub fn head(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The last non-empty line of `text`, shortened, hidden characters
/// escaped.
fn last_line(text: &str) -> Option<String> {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line: String = line.chars().take(300).collect();
    Some(escape_hidden(&line))
}

/// `value` with known secrets hidden in every string in it.
fn hide_in(value: &Value, hide: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::String(text) => Value::String(hide(text)),
        Value::Array(items) => Value::Array(items.iter().map(|item| hide_in(item, hide)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), hide_in(item, hide)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
pub(crate) fn test_secret_names(hidden: &[String]) -> Vec<String> {
    secret_names(hidden)
}
