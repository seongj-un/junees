//! Subagents: the `delegate` tool runs tasks on fresh agents, several at
//! a time, and hands their final answers back to the agent that called it.
//!
//! A subagent is a new [`crate::agent::Agent`] with the delegating agent's
//! model, system prompt and tools (but [`WITHHELD`]), an empty history and
//! the same working folder. It shares the delegating agent's cancel, its
//! approval gate (questions name the subagent) and its token limit: the
//! tokens subagents use count toward the delegating session (a
//! `subagent_usage` journal entry), so a task's budget covers them too.
//! Only one-line summaries of what they do are shown, and each keeps its
//! own journal next to the session's (`sessions/<id>/sub-<n>.jsonl`).

mod run;
#[cfg(test)]
mod tests;

pub(crate) use run::{Parent, run};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::SharedGate;
use crate::approval::ToolGate;
use crate::tools::{Tool, ToolOutput};

/// The tool's name.
pub const NAME: &str = "delegate";

/// Tools a subagent does not get: `delegate` itself (subagents do not
/// delegate further), and the tools that change the user's memory and
/// skills, which only the conversation with the user should touch.
pub const WITHHELD: &[&str] = &[NAME, "memory", "skill_manage"];

/// Tasks one `delegate` call may hold.
pub const MAX_TASKS: usize = 10;

/// Subagents that run at once by default (`JUNEES_SUBAGENTS`).
pub const DEFAULT_PARALLEL: usize = 3;

/// Model calls one subagent may make by default
/// (`JUNEES_SUBAGENT_MAX_TURNS`).
pub const DEFAULT_MAX_TURNS: usize = 30;

/// Tokens one subagent may use by default (`JUNEES_SUBAGENT_MAX_TOKENS`),
/// counted like a task budget (cache reads and writes included).
pub const DEFAULT_MAX_TOKENS: u64 = 2_000_000;

/// Characters of subagent answers one `delegate` result holds, shared by
/// its tasks: as much as a bash result.
pub const RESULT_CHARS: usize = 30_000;

/// What the system prompt says about `delegate` when it is offered.
pub const PROMPT_NOTE: &str = "\n\nSplit large, independent pieces of work (separate areas to \
    research, unrelated modules to change) into tasks for the `delegate` tool, which runs them \
    on subagents in parallel. A subagent knows nothing of this conversation: put everything it \
    needs into each task's goal and context.";

/// Ends a subagent's system prompt.
const CHILD_NOTE: &str = "\n\nYou are a subagent: another Junees agent gave you the task in \
    the user message. Nobody reads your messages while you work and you cannot ask questions, \
    so decide for yourself within the task. Finish with a final answer that says what you did, \
    what you found and what is left undone: it is all the delegating agent gets back. \
    Subagents have no delegate, memory or skill_manage tool.";

/// Characters of a goal shown in progress lines and approval questions.
const GOAL_PREVIEW_CHARS: usize = 60;

/// Characters of a tool call shown in a progress line.
const CALL_PREVIEW_CHARS: usize = 100;

/// The settings of the `delegate` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Subagents that run at once, from 1 to [`MAX_TASKS`].
    pub parallel: usize,
    /// Model calls one subagent may make.
    pub max_turns: usize,
    /// Tokens one subagent may use.
    pub max_tokens: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            parallel: DEFAULT_PARALLEL,
            max_turns: DEFAULT_MAX_TURNS,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

impl Settings {
    /// Reads `JUNEES_SUBAGENTS` (subagents at once; 0 turns `delegate`
    /// off), `JUNEES_SUBAGENT_MAX_TURNS` and `JUNEES_SUBAGENT_MAX_TOKENS`.
    /// `None` when `delegate` is off.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let mut settings = Self::default();
        if let Some(raw) = get("JUNEES_SUBAGENTS") {
            settings.parallel = match raw.parse::<usize>() {
                Ok(0) => return Ok(None),
                Ok(value) if value <= MAX_TASKS => value,
                _ => bail!(
                    "JUNEES_SUBAGENTS must be a whole number from 0 (no subagents) to \
                     {MAX_TASKS}, got `{raw}`"
                ),
            };
        }
        if let Some(raw) = get("JUNEES_SUBAGENT_MAX_TURNS") {
            settings.max_turns = match raw.parse::<usize>() {
                Ok(value) if value > 0 => value,
                _ => bail!("JUNEES_SUBAGENT_MAX_TURNS must be a positive integer, got `{raw}`"),
            };
        }
        if let Some(raw) = get("JUNEES_SUBAGENT_MAX_TOKENS") {
            settings.max_tokens = crate::cli::positive(&raw).ok_or_else(|| {
                anyhow::anyhow!(
                    "JUNEES_SUBAGENT_MAX_TOKENS must be a positive integer, got `{raw}`"
                )
            })?;
        }
        Ok(Some(settings))
    }
}

/// [`PROMPT_NOTE`] when `delegate` is offered, else nothing.
pub fn prompt_note(settings: Option<Settings>) -> &'static str {
    if settings.is_some() { PROMPT_NOTE } else { "" }
}

/// The `delegate` tool as the model sees it. The agent loop runs its
/// calls itself ([`crate::agent::Agent::with_delegate`]), because they
/// need the agent: its model, tools, cancel, gate and budget.
pub struct Delegate {
    description: String,
}

impl Delegate {
    pub fn new(settings: Settings) -> Self {
        let Settings {
            parallel,
            max_turns,
            max_tokens,
        } = settings;
        Self {
            description: format!(
                "Runs independent tasks on fresh subagents, {parallel} at a time, and returns \
                 each one's final answer, in task order. A subagent works in the same folder \
                 with your tools except delegate, memory and skill_manage, and knows nothing \
                 of this conversation: put everything it needs into `goal` and `context` \
                 (paths, what you found so far, constraints, what to report back). Each may \
                 make up to {max_turns} model calls and use up to {max_tokens} tokens, which \
                 count toward this session. Use it for large, independent work, not for small \
                 or dependent steps; subagents changing the same files at once can overwrite \
                 each other's edits."
            ),
        }
    }
}

#[async_trait]
impl Tool for Delegate {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_TASKS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "goal": {
                                "type": "string",
                                "description": "What the subagent should do and report back."
                            },
                            "context": {
                                "type": "string",
                                "description": "Everything else it needs to know: it has not \
                                                seen this conversation."
                            }
                        },
                        "required": ["goal"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    async fn run(&self, _input: Value) -> ToolOutput {
        ToolOutput::error("delegate is not available here: only an agent's own loop runs it")
    }
}

/// One task of a `delegate` call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Task {
    goal: String,
    context: Option<String>,
}

impl Task {
    /// The subagent's input.
    fn input(&self) -> String {
        match &self.context {
            Some(context) => format!(
                "{}\n\nContext from the agent that delegated this task:\n{context}",
                self.goal
            ),
            None => self.goal.clone(),
        }
    }
}

/// The tasks of a `delegate` call's input.
fn parse(input: &Value) -> Result<Vec<Task>, ToolOutput> {
    let Some(tasks) = input.get("tasks").and_then(Value::as_array) else {
        return Err(ToolOutput::error("missing required array argument `tasks`"));
    };
    if tasks.is_empty() || tasks.len() > MAX_TASKS {
        return Err(ToolOutput::error(format!(
            "`tasks` must hold 1 to {MAX_TASKS} tasks, got {}",
            tasks.len()
        )));
    }
    let mut parsed = Vec::with_capacity(tasks.len());
    for (i, task) in tasks.iter().enumerate() {
        let n = i + 1;
        let goal = match task.get("goal").and_then(Value::as_str).map(str::trim) {
            Some(goal) if !goal.is_empty() => goal.to_string(),
            _ => {
                return Err(ToolOutput::error(format!(
                    "task {n} needs a non-empty string `goal`"
                )));
            }
        };
        let context = match task.get("context") {
            None | Some(Value::Null) => None,
            Some(Value::String(context)) if context.trim().is_empty() => None,
            Some(Value::String(context)) => Some(context.trim().to_string()),
            Some(_) => {
                return Err(ToolOutput::error(format!(
                    "task {n}: `context` must be a string"
                )));
            }
        };
        parsed.push(Task { goal, context });
    }
    Ok(parsed)
}

/// A shortened line of model-written text, safe to show on a terminal.
fn preview(text: &str, max_chars: usize) -> String {
    crate::prompt_text::escape_hidden(&crate::cli::preview(text, max_chars))
}

/// The gate of a subagent: the delegating agent's, shared by all its
/// subagents, with questions that say which subagent asks.
struct ChildGate {
    shared: SharedGate,
    /// `[n] <goal>`.
    label: String,
}

#[async_trait]
impl ToolGate for ChildGate {
    async fn check(&self, tool: &str, input: &Value) -> Option<ToolOutput> {
        self.shared.check_for(&self.label, tool, input).await
    }
}
