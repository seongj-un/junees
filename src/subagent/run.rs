//! Running the tasks of a `delegate` call.

use std::cell::RefCell;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use super::{
    CALL_PREVIEW_CHARS, CHILD_NOTE, ChildGate, GOAL_PREVIEW_CHARS, PROMPT_NOTE, RESULT_CHARS,
    Settings, Task, WITHHELD, parse, preview,
};
use crate::agent::{
    Agent, AgentOptions, Cancelled, Event, Limit, LimitReached, SharedGate, SharedLimit, TurnLimit,
};
use crate::cancel::Cancel;
use crate::message::{Role, Usage};
use crate::provider::Provider;
use crate::session::SessionWriter;
use crate::task::budget::tokens;
use crate::tools::{ToolOutput, ToolRegistry, truncate_middle};

/// What subagents take from the agent that delegates to them.
pub(crate) struct Parent<'a> {
    pub provider: &'a Arc<dyn Provider>,
    pub tools: &'a ToolRegistry,
    pub options: &'a AgentOptions,
    pub cancel: &'a Cancel,
    pub gate: Option<&'a SharedGate>,
    pub limit: Option<&'a SharedLimit>,
    /// The tokens the delegating session used before the call.
    pub usage: Usage,
    /// The delegating session's journal file, if it is saved.
    pub journal: Option<&'a Path>,
}

/// How a subagent ended.
#[derive(Debug)]
enum Ended {
    Answered(String),
    /// Stopped by its turn or token limit, or by the delegating session's
    /// budget, with the last text it wrote.
    Stopped {
        reason: String,
        last: String,
    },
    Cancelled,
    Failed(String),
}

impl Ended {
    fn word(&self) -> &'static str {
        match self {
            Self::Answered(_) => "done",
            Self::Stopped { .. } => "stopped",
            Self::Cancelled => "cancelled",
            Self::Failed(_) => "failed",
        }
    }
}

struct Outcome {
    ended: Ended,
    usage: Usage,
}

/// Runs the tasks of a `delegate` call's `input`, at most
/// `settings.parallel` at a time, and returns the call's result (each
/// task's answer, in task order) and the tokens the subagents used.
/// Progress goes to `on_event` as [`Event::Subagent`] lines.
pub(crate) async fn run(
    parent: Parent<'_>,
    settings: Settings,
    input: &Value,
    on_event: &mut dyn FnMut(Event<'_>),
) -> (ToolOutput, Usage) {
    let tasks = match parse(input) {
        Ok(tasks) => tasks,
        Err(output) => return (output, Usage::default()),
    };
    let first_journal = parent.journal.map(next_sub_number);
    // The tokens each subagent used as of its last model call.
    let spent = Arc::new(Mutex::new(vec![Usage::default(); tasks.len()]));
    let on_event = RefCell::new(on_event);
    let say = |index: usize, line: &str| (*on_event.borrow_mut())(Event::Subagent { index, line });
    let children = tasks.iter().enumerate().map(|(i, task)| {
        let journal = first_journal.map(|n| n + i);
        child(&parent, settings, i, task, journal, &spent, &say)
    });
    let outcomes: Vec<Outcome> = stream::iter(children)
        .buffered(settings.parallel.max(1))
        .collect()
        .await;
    let mut usage = Usage::default();
    for outcome in &outcomes {
        usage.add(outcome.usage);
    }
    (result(&tasks, &outcomes), usage)
}

/// Runs task `i` on a new subagent.
async fn child(
    parent: &Parent<'_>,
    settings: Settings,
    i: usize,
    task: &Task,
    journal: Option<usize>,
    spent: &Arc<Mutex<Vec<Usage>>>,
    say: &dyn Fn(usize, &str),
) -> Outcome {
    let index = i + 1;
    if parent.cancel.is_cancelled() {
        say(index, "cancelled before it started");
        return Outcome {
            ended: Ended::Cancelled,
            usage: Usage::default(),
        };
    }
    let goal = preview(&task.goal, GOAL_PREVIEW_CHARS);
    say(index, &format!("started: {goal}"));
    let options = AgentOptions {
        system: parent.options.system.replacen(PROMPT_NOTE, "", 1) + CHILD_NOTE,
        max_turns: settings.max_turns,
        ..parent.options.clone()
    };
    let mut agent = Agent::sharing(
        Arc::clone(parent.provider),
        parent.tools.without(WITHHELD),
        options,
    )
    .with_cancel(parent.cancel.clone())
    .with_limit(limit(parent, settings, i, spent));
    if let Some(gate) = parent.gate {
        agent = agent.with_gate(Box::new(ChildGate {
            shared: Arc::clone(gate),
            label: format!("[{index}] {goal}"),
        }));
    }
    if let (Some(path), Some(n)) = (parent.journal, journal) {
        match SessionWriter::create_sub(path, &format!("sub-{n}")) {
            Ok(writer) => agent = agent.with_journal(Box::new(writer)),
            Err(err) => say(index, &format!("!! its session is not saved: {err:#}")),
        }
    }

    let result = agent
        .run(&task.input(), |event| {
            if let Some(line) = summary(&event) {
                say(index, &line);
            }
        })
        .await;
    let usage = agent.usage();
    // The limits of the subagents still running count all of it.
    spent.lock().unwrap_or_else(PoisonError::into_inner)[i] = usage;
    let ended = match result {
        Ok(answer) => Ended::Answered(answer),
        Err(err) if err.downcast_ref::<Cancelled>().is_some() => Ended::Cancelled,
        Err(err)
            if err.downcast_ref::<TurnLimit>().is_some()
                || err.downcast_ref::<LimitReached>().is_some() =>
        {
            Ended::Stopped {
                reason: err.to_string(),
                last: last_text(&agent),
            }
        }
        Err(err) => Ended::Failed(format!("{err:#}")),
    };
    let status = match &ended {
        Ended::Answered(_) => format!("done ({} tokens)", tokens(usage)),
        Ended::Stopped { reason, .. } => format!("stopped: {reason}"),
        Ended::Cancelled => "cancelled".to_string(),
        Ended::Failed(err) => format!("failed: {}", preview(err, CALL_PREVIEW_CHARS)),
    };
    say(index, &status);
    Outcome { ended, usage }
}

/// The limit of subagent `i`: its own token limit, then the delegating
/// agent's limit, asked with the tokens the session and all its subagents
/// used so far.
fn limit(
    parent: &Parent<'_>,
    settings: Settings,
    i: usize,
    spent: &Arc<Mutex<Vec<Usage>>>,
) -> Limit {
    let spent = Arc::clone(spent);
    let outer = parent.limit.cloned();
    let before = parent.usage;
    let max = settings.max_tokens;
    Box::new(move |usage| {
        let mut total = before;
        {
            let mut spent = spent.lock().unwrap_or_else(PoisonError::into_inner);
            spent[i] = usage;
            for used in spent.iter() {
                total.add(*used);
            }
        }
        let used = tokens(usage);
        if used >= max {
            return Some(format!(
                "subagent token limit used up: {used} of {max} tokens"
            ));
        }
        let outer = outer.as_ref()?;
        let mut outer = outer.lock().unwrap_or_else(PoisonError::into_inner);
        (*outer)(total)
    })
}

/// The progress line of a subagent's event, for those worth one.
fn summary(event: &Event<'_>) -> Option<String> {
    match event {
        Event::ToolCall { name, input } => Some(format!(
            "-> {name} {}",
            preview(&input.to_string(), CALL_PREVIEW_CHARS)
        )),
        Event::ServerToolUse { name, input } => Some(format!(
            "-> {name} {} (run by the API)",
            preview(&input.to_string(), CALL_PREVIEW_CHARS)
        )),
        Event::ToolResult { name, output } if output.is_error => Some(format!(
            "<- {name} error: {}",
            preview(&output.content, CALL_PREVIEW_CHARS)
        )),
        Event::Warning(warning) => Some(format!("!! {}", preview(warning, 200))),
        Event::Compacted {
            before_messages,
            after_messages,
        } => Some(format!(
            "(compacted its conversation: {before_messages} messages -> {after_messages})"
        )),
        _ => None,
    }
}

/// The text of the subagent's last reply.
fn last_text(agent: &Agent) -> String {
    agent
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.text())
        .unwrap_or_default()
}

/// The number of the next subagent journal of the session recorded at
/// `parent`: one past the highest `sub-<n>.jsonl` next to it.
fn next_sub_number(parent: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(parent.with_extension("")) else {
        return 1;
    };
    entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            let name = name.to_str()?;
            name.strip_prefix("sub-")?
                .strip_suffix(".jsonl")?
                .parse::<usize>()
                .ok()
        })
        .max()
        .map_or(1, |n| n + 1)
}

/// The `delegate` call's result: every task's answer (or how it ended),
/// in task order, sharing [`RESULT_CHARS`]. It is an error when no task
/// gave an answer.
fn result(tasks: &[Task], outcomes: &[Outcome]) -> ToolOutput {
    let room = RESULT_CHARS / tasks.len().max(1);
    let mut text = String::new();
    for (i, (task, outcome)) in tasks.iter().zip(outcomes).enumerate() {
        if i > 0 {
            text.push_str("\n\n");
        }
        text.push_str(&format!(
            "[{}] {}: {}\n",
            i + 1,
            outcome.ended.word(),
            crate::cli::preview(&task.goal, GOAL_PREVIEW_CHARS)
        ));
        match &outcome.ended {
            Ended::Answered(answer) if answer.trim().is_empty() => {
                text.push_str("(it gave no answer text)");
            }
            Ended::Answered(answer) => text.push_str(&truncate_middle(answer.trim(), room)),
            Ended::Stopped { reason, last } => {
                text.push_str(reason);
                if !last.trim().is_empty() {
                    text.push_str("\nits last message:\n");
                    text.push_str(&truncate_middle(last.trim(), room));
                }
            }
            Ended::Cancelled => text.push_str(crate::agent::CANCELLED),
            Ended::Failed(err) => text.push_str(&format!("error: {err}")),
        }
    }
    let answered = outcomes
        .iter()
        .any(|outcome| matches!(outcome.ended, Ended::Answered(_)));
    if answered {
        ToolOutput::ok(text)
    } else {
        ToolOutput::error(text)
    }
}
