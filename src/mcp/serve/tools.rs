//! The tools of `junees mcp serve` and the state they run with.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use super::{Options, Out, send};
use crate::agent::{Cancelled, Event, LimitReached, TurnLimit};
use crate::cancel::Cancel;
use crate::config::Config;
use crate::config_file::Settings;
use crate::cron::run::{Answer as Collected, ApiEngine, Limits, Spec, Trigger};
use crate::mcp::Mcp;
use crate::session::{self, Session, SessionWriter};
use crate::tools::GhConfigDir;

/// Added to the system prompt of every `junees_run`.
pub const MCP_NOTE: &str = "\n\nYou are running a task another agent handed you through MCP \
    (`junees mcp serve`), unattended: nobody reads along or answers questions, and calls that \
    would need the user's approval are refused. Do the task, then end with the result: your \
    final message is returned to the agent that asked. The saved memory and skills can be read \
    but not changed here.";

/// What `initialize` tells the client about the server.
pub const INSTRUCTIONS: &str = "Junees is a coding agent working in one folder of this \
    computer. junees_run hands it a task and returns its final answer (pass session_id to \
    continue a conversation); junees_sessions and junees_session show its past sessions in that \
    folder. Runs are unattended: what would need the user's approval is refused.";

/// The longest prompt taken, in characters.
pub const MAX_PROMPT_CHARS: usize = 100_000;
/// The most turns a call may ask for.
pub const MAX_TURNS: u64 = 200;
/// How long one `junees_run` may take.
pub const MAX_TIME: Duration = Duration::from_secs(30 * 60);
/// Journals read to find the sessions of the folder.
const SCAN: usize = 1000;
/// `junees_sessions`'s `limit`: default and most.
const SESSIONS: (u64, u64) = (20, 100);
/// `junees_session`'s `limit`: default and most.
const MESSAGES: (u64, u64) = (100, 200);
/// Characters of a session's title.
const TITLE_CHARS: usize = 80;

/// How a `tools/call` ends.
pub enum Answer {
    /// A `CallToolResult`, `isError` or not.
    Result(Value),
    /// A JSON-RPC error: the call itself was wrong (an unknown tool).
    Error(i64, String),
    /// The client cancelled it: no answer.
    Cancelled,
}

/// A tool result holding `text` only, as an error the calling model reads.
fn failed(text: impl Into<String>) -> Answer {
    Answer::Result(json!({
        "content": [{"type": "text", "text": text.into()}],
        "isError": true,
    }))
}

/// A tool result: `structured` as JSON text and as `structuredContent`.
fn structured(text: String, value: Value) -> Answer {
    Answer::Result(json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": false,
    }))
}

/// The server: its folder, settings, MCP servers and allowed tools.
pub struct Server {
    pub dir: PathBuf,
    settings: Settings,
    config: Config,
    allow_write: bool,
    allow_bash: bool,
    mcp: Mcp,
    /// The runs' empty GitHub CLI configuration folder.
    gh_config: GhConfigDir,
}

impl Server {
    /// Checks the folder and the backend, moves to the folder and starts
    /// the trusted MCP servers.
    pub async fn start(options: &Options, settings: Settings) -> Result<Self> {
        let dir = super::work_dir(options)?;
        let env: crate::cron::deliver::Lookup = Arc::new(|key: &str| std::env::var(key).ok());
        crate::cron::command::check_home_folder(&env, &dir, options.allow_home, None).map_err(
            |err| anyhow::anyhow!("{err:#}").context("junees mcp serve refuses this --dir"),
        )?;
        if matches!(
            crate::claude_code::Backend::from_lookup(|key| settings.get(key))?,
            crate::claude_code::Backend::ClaudeCode
        ) {
            bail!(
                "junees mcp serve runs Junees's own agent on the API only: the claude-code \
                 backend runs its commands without Junees's unattended rules (set backend = \
                 \"api\" for it, or JUNEES_BACKEND=api)"
            );
        }
        let config = Config::from_lookup(|key| settings.get(key))?;
        std::env::set_current_dir(&dir)
            .map_err(|err| anyhow::anyhow!("cannot work in {}: {err}", dir.display()))?;
        let home = crate::cron::command::user_home(&env);
        let path_var = std::env::var_os("PATH");
        if let Some(warning) = crate::tools::unattended_folder_warning(
            &dir,
            home.as_deref(),
            config.home.as_deref(),
            path_var.as_deref(),
        ) {
            eprintln!("  !! {warning}");
        }
        let gh_config = GhConfigDir::new()
            .map_err(|err| anyhow::anyhow!("cannot make the runs' GitHub CLI folder: {err}"))?;
        let mcp = Mcp::start(config.home.as_deref(), &config.tools.hidden_env, &dir).await;
        for line in mcp.report() {
            eprintln!("{line}");
        }
        Ok(Self {
            dir,
            settings,
            config,
            allow_write: options.allow_write,
            allow_bash: options.allow_bash,
            mcp,
            gh_config,
        })
    }

    pub async fn shutdown(&self) {
        self.mcp.shutdown().await;
    }

    /// The tools, for `tools/list`.
    pub fn list(&self) -> Vec<Value> {
        let mut tools_of_run = vec!["read_file"];
        if self.allow_write {
            tools_of_run.extend(crate::cron::run::MCP_WRITE_TOOLS);
        }
        if self.allow_bash {
            tools_of_run.push("bash");
        }
        vec![
            json!({
                "name": "junees_run",
                "title": "Run Junees",
                "description": format!(
                    "Hands a task to Junees, a coding agent working in {}, and returns its final \
                     answer and token usage. Its own tools here: {} (and read-only GitHub, \
                     skills and trusted MCP servers when set up). Unattended: what would need \
                     the user's approval is refused. Pass session_id to continue a conversation \
                     this tool started.",
                    self.dir.display(),
                    tools_of_run.join(", ")
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string", "minLength": 1, "maxLength": MAX_PROMPT_CHARS,
                            "description": "The task, as you would write it to a colleague."
                        },
                        "session_id": {
                            "type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$",
                            "description": "A session_id an earlier junees_run returned, to continue it."
                        },
                        "max_turns": {
                            "type": "integer", "minimum": 1, "maximum": MAX_TURNS,
                            "description": "The most model calls of this run."
                        }
                    },
                    "required": ["prompt"],
                    "additionalProperties": false
                },
                "outputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {"type": ["string", "null"]},
                        "text": {"type": "string"},
                        "input_tokens": {"type": "integer"},
                        "output_tokens": {"type": "integer"},
                        "cost_usd": {"type": ["number", "null"]},
                        "stop_reason": {
                            "type": "string",
                            "enum": ["end_turn", "max_turns", "token_limit", "time_limit"]
                        }
                    },
                    "required": ["session_id", "text", "input_tokens", "output_tokens",
                                 "cost_usd", "stop_reason"]
                }
            }),
            json!({
                "name": "junees_sessions",
                "title": "Junees sessions",
                "description": "The latest sessions of Junees in its folder, newest first: id, \
                    title (the first line of the first message, secrets hidden), source and when \
                    it was last written.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1, "maximum": SESSIONS.1,
                                  "description": format!("How many (default {}).", SESSIONS.0)}
                    },
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": true}
            }),
            json!({
                "name": "junees_session",
                "title": "Junees session",
                "description": "One session of Junees in its folder, a page of messages: text \
                    blocks, tool calls and results, cut to a length and with secrets hidden.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$"},
                        "offset": {"type": "integer", "minimum": 0,
                                   "description": "The first message (default 0)."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": MESSAGES.1,
                                  "description": format!("How many messages (default {}).", MESSAGES.0)}
                    },
                    "required": ["session_id"],
                    "additionalProperties": false
                },
                "annotations": {"readOnlyHint": true}
            }),
        ]
    }

    /// One `tools/call`.
    pub async fn call(&self, params: &Value, cancel: &Cancel, out: &Out) -> Answer {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Answer::Error(-32602, "tools/call needs a tool name".into());
        };
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(args)) => args,
            Some(_) => return failed("the arguments must be an object"),
        };
        let progress = params
            .pointer("/_meta/progressToken")
            .filter(|token| token.is_string() || token.is_number())
            .cloned();
        match name {
            "junees_run" => self.run(args, cancel, out, progress).await,
            "junees_sessions" => self.sessions(args),
            "junees_session" => self.session(args),
            other => Answer::Error(-32602, format!("unknown tool: {other}")),
        }
    }

    /// `junees_run`.
    async fn run(
        &self,
        args: &Map<String, Value>,
        client: &Cancel,
        out: &Out,
        progress: Option<Value>,
    ) -> Answer {
        if let Err(why) = only(args, &["prompt", "session_id", "max_turns"]) {
            return failed(why);
        }
        let prompt = match args.get("prompt") {
            Some(Value::String(text)) if !text.trim().is_empty() => text,
            _ => return failed("prompt must be a string that is not empty"),
        };
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return failed(format!("prompt is over {MAX_PROMPT_CHARS} characters"));
        }
        let resume = match args.get("session_id") {
            None => None,
            Some(Value::String(id)) => match self.own_session(id, true) {
                Ok(path) => Some(path),
                Err(why) => return failed(why),
            },
            Some(_) => return failed("session_id must be a string"),
        };
        let mut config = self.config.clone();
        match args.get("max_turns") {
            None => {}
            Some(value) => match value
                .as_u64()
                .filter(|turns| (1..=MAX_TURNS).contains(turns))
            {
                Some(turns) => config.max_turns = turns as usize,
                None => {
                    return failed(format!(
                        "max_turns must be a whole number from 1 to {MAX_TURNS}"
                    ));
                }
            },
        }
        let model = config.model.clone();
        let engine = ApiEngine::new(config);
        let spec = Spec {
            prompt,
            dir: &self.dir,
            source: session::MCP,
            channel: session::MCP,
            note: MCP_NOTE,
            trigger: Trigger::Mcp {
                gh_config: self.gh_config.path(),
                allow_write: self.allow_write,
                allow_bash: self.allow_bash,
            },
        };
        let stop = Cancel::new();
        let limits = Limits {
            max_time: MAX_TIME,
            max_tokens: 0,
        };
        let (mut agent, _left_out) = match engine.agent(&spec, &self.mcp, &stop, limits) {
            Ok(built) => built,
            Err(err) => return failed(self.hidden(format!("cannot start the run: {err:#}"))),
        };
        let session_id = match (&resume, self.config.home.as_deref()) {
            (Some(path), _) => match Session::resume(path) {
                Ok((session, writer)) => {
                    let id = session.meta.id.clone();
                    agent.resume(session.transcript, Some(Box::new(writer)));
                    Some(id)
                }
                Err(err) => {
                    return failed(self.hidden(format!("cannot continue the session: {err:#}")));
                }
            },
            (None, Some(home)) => match SessionWriter::create_for_channel(
                home,
                &self.dir,
                &model,
                None,
                session::MCP,
                session::MCP,
            ) {
                Ok((meta, writer)) => {
                    agent = agent.with_journal(Box::new(writer));
                    Some(meta.id)
                }
                Err(err) => {
                    eprintln!("  !! the run is not saved as a session: {err:#}");
                    None
                }
            },
            (None, None) => None,
        };
        // The client's cancel and the time limit both stop the run.
        let timed_out = std::rc::Rc::new(Cell::new(false));
        let watcher = {
            let (client, stop, timed_out) = (client.clone(), stop.clone(), timed_out.clone());
            tokio::task::spawn_local(async move {
                tokio::select! {
                    _ = client.cancelled() => {}
                    _ = tokio::time::sleep(MAX_TIME) => timed_out.set(true),
                }
                stop.cancel();
            })
        };
        eprintln!(
            "junees mcp serve: run{} started",
            session_id
                .as_deref()
                .map_or(String::new(), |id| format!(" in session {id}"))
        );
        let collected = RefCell::new(Collected::default());
        let mut steps = 0u64;
        let result = agent
            .run(prompt, |event| {
                if let Event::ToolCall { name, .. } = &event {
                    steps += 1;
                    if let Some(token) = &progress {
                        // The tool's name only: its input may hold anything.
                        send(
                            out,
                            json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": token,
                                    "progress": steps,
                                    "message": format!("tool call: {}", crate::prompt_text::escape_hidden(name)),
                                }
                            }),
                        );
                    }
                }
                collected.borrow_mut().event(event);
            })
            .await;
        watcher.abort();
        if client.is_cancelled() {
            eprintln!("junees mcp serve: run cancelled by the client");
            return Answer::Cancelled;
        }
        let usage = agent.usage();
        let cost = crate::usage::prices::Prices::from_setting(
            self.settings.get(crate::usage::PRICES_ENV).as_deref(),
        )
        .ok()
        .and_then(|prices| prices.get(&model))
        .map(|price| crate::usage::prices::cost(&price, &usage, false) as f64 / 1e9);
        let (stop_reason, text) = match result {
            Ok(text) => ("end_turn", text.trim().to_string()),
            Err(err) => {
                let partial = collected.into_inner().text();
                if err.downcast_ref::<TurnLimit>().is_some() {
                    ("max_turns", partial)
                } else if err.downcast_ref::<LimitReached>().is_some() {
                    ("token_limit", partial)
                } else if err.downcast_ref::<Cancelled>().is_some() && timed_out.get() {
                    ("time_limit", partial)
                } else {
                    return failed(self.hidden(format!(
                        "the run failed: {err:#}{}",
                        session_id
                            .as_deref()
                            .map_or(String::new(), |id| format!(" (session {id})"))
                    )));
                }
            }
        };
        let text = self.hidden(text);
        eprintln!(
            "junees mcp serve: run ended ({stop_reason}, {} in, {} out)",
            usage.input_tokens, usage.output_tokens
        );
        let shown = if stop_reason == "end_turn" {
            text.clone()
        } else {
            format!("(stopped: {stop_reason})\n\n{text}")
        };
        structured(
            shown,
            json!({
                "session_id": session_id,
                "text": text,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cost_usd": cost,
                "stop_reason": stop_reason,
            }),
        )
    }

    /// `junees_sessions`.
    fn sessions(&self, args: &Map<String, Value>) -> Answer {
        if let Err(why) = only(args, &["limit"]) {
            return failed(why);
        }
        let limit = match number(args, "limit", SESSIONS) {
            Ok(limit) => limit as usize,
            Err(why) => return failed(why),
        };
        let Some(home) = self.config.home.as_deref() else {
            return structured("[]".into(), json!({ "sessions": [] }));
        };
        let dir = self.dir.to_string_lossy();
        let list = match session::list_all(home, SCAN) {
            Ok(list) => list,
            Err(err) => return failed(self.hidden(format!("cannot list the sessions: {err:#}"))),
        };
        let sessions: Vec<Value> = list
            .iter()
            .filter(|(_, summary)| summary.cwd == dir)
            .take(limit)
            .map(|(_, summary)| {
                let first = summary
                    .first_input
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default();
                let title: String = self
                    .hidden(first.trim().to_string())
                    .chars()
                    .take(TITLE_CHARS)
                    .collect();
                json!({
                    "id": summary.id,
                    "title": title,
                    "source": summary.source.as_deref().unwrap_or(crate::search::TERMINAL),
                    "updated_at": session::format_utc_seconds(summary.updated),
                })
            })
            .collect();
        let value = json!({ "sessions": sessions });
        structured(value.to_string(), value)
    }

    /// `junees_session`.
    fn session(&self, args: &Map<String, Value>) -> Answer {
        if let Err(why) = only(args, &["session_id", "offset", "limit"]) {
            return failed(why);
        }
        let id = match args.get("session_id") {
            Some(Value::String(id)) => id,
            _ => return failed("session_id must be a string"),
        };
        if let Err(why) = self.own_session(id, false) {
            return failed(why);
        }
        let offset = match args.get("offset") {
            None => 0,
            Some(value) => match value.as_u64() {
                Some(offset) => offset as usize,
                None => return failed("offset must be a whole number, 0 or more"),
            },
        };
        let limit = match number(args, "limit", MESSAGES) {
            Ok(limit) => limit as usize,
            Err(why) => return failed(why),
        };
        match crate::dashboard::session_view::page(&self.settings, id, offset, limit) {
            Ok(value) => structured(value.to_string(), value),
            Err(why) => failed(why),
        }
    }

    /// The journal of session `id`, when it is a session of this folder
    /// (and, with `mcp_only`, one `junees_run` started): never another
    /// folder's, so an agent given one project cannot read the others.
    fn own_session(&self, id: &str, mcp_only: bool) -> Result<PathBuf, String> {
        let missing = || {
            format!(
                "there is no session `{}` of {}",
                crate::prompt_text::escape_hidden(id),
                self.dir.display()
            )
        };
        if !crate::dashboard::session_view::valid_id(id) {
            return Err("session_id is 1 to 64 letters, digits, `_` or `-`".into());
        }
        let home = self.config.home.as_deref().ok_or_else(missing)?;
        let path = session::sessions_dir(home).join(format!("{id}.jsonl"));
        // Not followed: a link in the sessions folder is not a session.
        let is_file = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
        let meta = is_file
            .then(|| session::read_meta(&path))
            .flatten()
            .ok_or_else(missing)?;
        if Path::new(&meta.cwd) != self.dir {
            return Err(missing());
        }
        if mcp_only && meta.source.as_deref() != Some(session::MCP) {
            return Err(format!(
                "session `{id}` was not started by junees_run; only those can be continued"
            ));
        }
        if meta.backend.is_some() {
            return Err(format!("session `{id}` is a claude-code session"));
        }
        Ok(path)
    }

    /// `text` with the secrets of the settings and known secret patterns
    /// hidden.
    fn hidden(&self, mut text: String) -> String {
        crate::doctor::hide_secrets_in_user_text(&self.settings, std::iter::once(&mut text));
        text
    }
}

/// Refuses arguments not in `known`.
fn only(args: &Map<String, Value>, known: &[&str]) -> Result<(), String> {
    match args.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(format!(
            "unknown argument `{}` (this tool takes {})",
            crate::prompt_text::escape_hidden(key),
            known.join(", ")
        )),
        None => Ok(()),
    }
}

/// The whole number `name` of `args`, from 1 to `most`; `default` when
/// left out.
fn number(
    args: &Map<String, Value>,
    name: &str,
    (default, most): (u64, u64),
) -> Result<u64, String> {
    match args.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|number| (1..=most).contains(number))
            .ok_or_else(|| format!("{name} must be a whole number from 1 to {most}")),
    }
}
