//! Plugin hooks: `pre_tool` commands may refuse a tool call, `post_turn`
//! commands are told how each turn ended.
//!
//! A `pre_tool` hook gets `{"event": "pre_tool", "tool", "input", "cwd"}`
//! on stdin before a tool runs (after an approval question, if one is
//! asked). Exit code 0 lets the call run, 2 refuses it with the hook's
//! stdout as the reason the model reads; anything else, a time-out or a
//! program that does not start is a warning, and the call runs. The first
//! hook that refuses decides.
//!
//! A `post_turn` hook gets `{"event": "post_turn", "input", "outcome",
//! "answer", "error", "cwd"}` once a turn ended (`outcome` is `answered`,
//! `cancelled` or `failed`); what it prints is ignored, a failure is a
//! warning.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::manifest::Command;
use super::process::{self, Exit, Ran};
use super::{Plugin, Shared};
use crate::agent::{AfterTurn, Cancelled};
use crate::prompt_text::escape_hidden;
use crate::tools::{PreTool, ToolOutput, truncate_middle};

/// Characters of a refusing hook's reason passed on to the model.
pub const MAX_REASON_CHARS: usize = 2000;
/// The exit code with which a `pre_tool` hook refuses the call.
pub const REFUSE: i32 = 2;

/// Where hook warnings go: stderr, or a test's list.
pub type Warn = Arc<dyn Fn(&str) + Send + Sync>;

/// One hook command of a plugin.
pub struct Hook {
    pub plugin: Arc<Plugin>,
    pub command: Command,
}

/// The hooks of the enabled plugins, in plugin name order and then in
/// the order of each manifest.
pub struct Hooks {
    pub(super) pre_tool: Vec<Hook>,
    pub(super) post_turn: Vec<Hook>,
    pub(super) shared: Arc<Shared>,
    pub(super) warn: Warn,
}

/// Prints a warning on stderr.
pub fn stderr_warn() -> Warn {
    Arc::new(|line: &str| eprintln!("  !! {line}"))
}

impl Hooks {
    async fn run(&self, hook: &Hook, payload: &Value) -> Result<Ran, String> {
        let env = self.shared.env(&hook.plugin);
        process::run(
            &hook.command,
            &hook.plugin.dir,
            &env,
            payload.to_string().into_bytes(),
        )
        .await
    }

    fn workdir(&self) -> &PathBuf {
        &self.shared.workdir
    }

    /// Warns that `hook` (of kind `kind`) failed, and what happens instead.
    fn failed(&self, kind: &str, hook: &Hook, result: &Result<Ran, String>, then: &str) {
        let what = match result {
            Err(err) => err.clone(),
            Ok(ran) => {
                let mut what = match &ran.exit {
                    Exit::Code(code) => format!("exit code {code}"),
                    Exit::Signal => "terminated by a signal".into(),
                    Exit::TimedOut(limit) => format!(
                        "it did not finish within {} and was killed",
                        crate::mcp::server::seconds(*limit)
                    ),
                };
                if let Some(line) = ran.stderr_line() {
                    what.push_str(&format!("; its stderr: {line}"));
                }
                what
            }
        };
        (self.warn)(&format!(
            "plugin {}: {kind} hook `{}` failed ({what}){then}",
            hook.plugin.name,
            hook.command.shown()
        ));
    }
}

#[async_trait]
impl PreTool for Hooks {
    async fn check(&self, tool: &str, input: &Value) -> Option<ToolOutput> {
        let payload = json!({
            "event": "pre_tool",
            "tool": tool,
            "input": input,
            "cwd": self.workdir(),
        });
        for hook in &self.pre_tool {
            let result = self.run(hook, &payload).await;
            match &result {
                Ok(Ran {
                    exit: Exit::Code(0),
                    ..
                }) => {}
                Ok(Ran {
                    exit: Exit::Code(REFUSE),
                    stdout,
                    ..
                }) => {
                    let reason = escape_hidden(stdout.trim());
                    let reason = if reason.is_empty() {
                        "no reason given".to_string()
                    } else {
                        truncate_middle(&reason, MAX_REASON_CHARS)
                    };
                    return Some(ToolOutput::error(format!(
                        "not run: the pre_tool hook of the plugin `{}` refused this call: {reason}",
                        hook.plugin.name
                    )));
                }
                _ => self.failed("pre_tool", hook, &result, "; the call runs anyway"),
            }
        }
        None
    }
}

#[async_trait]
impl AfterTurn for Hooks {
    async fn after_turn(&self, input: &str, result: &Result<String>) {
        if self.post_turn.is_empty() {
            return;
        }
        let (outcome, answer, error) = match result {
            Ok(answer) => ("answered", Some(answer.clone()), None),
            Err(err) if err.downcast_ref::<Cancelled>().is_some() => ("cancelled", None, None),
            Err(err) => ("failed", None, Some(format!("{err:#}"))),
        };
        let payload = json!({
            "event": "post_turn",
            "input": input,
            "outcome": outcome,
            "answer": answer,
            "error": error,
            "cwd": self.workdir(),
        });
        for hook in &self.post_turn {
            let result = self.run(hook, &payload).await;
            if !matches!(
                result,
                Ok(Ran {
                    exit: Exit::Code(0),
                    ..
                })
            ) {
                self.failed("post_turn", hook, &result, "");
            }
        }
    }
}
