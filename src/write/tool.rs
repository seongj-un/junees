//! The `draft` tool: the model asks for a long text (a report, a design
//! document, a README) made level by level ([`super::run`]) and gets the
//! whole text back; saving it is up to the model (`write_file`). The agent
//! loop runs its calls itself ([`crate::agent::Agent::with_draft`]),
//! because they need the agent's model and cancel.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Draft, Options, Stop};
use crate::tools::{Tool, ToolOutput};

/// The tool's name.
pub const NAME: &str = "draft";

/// What the system prompt says about `draft` when it is offered.
pub const PROMPT_NOTE: &str = "\n\nFor a long document (a report, a design document, a README of \
    several sections), the `draft` tool writes it coarse to fine, many sections at once, faster \
    than writing it yourself front to back. It returns the whole text; save it with write_file \
    if it should be a file, after checking it.";

/// The settings of the `draft` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Requests at once ([`super::PARALLEL_ENV`]; at most
    /// [`super::UNATTENDED_PARALLEL`] in runs nobody watches).
    pub parallel: usize,
    /// A run nobody watches, whose input may come from anyone (a webhook):
    /// at most [`UNATTENDED_LEVELS`] levels and no seam pass, so one call
    /// cannot ask for the most calls a draft can make.
    pub unattended: bool,
}

/// The most levels a `draft` call gets in a run nobody watches.
pub const UNATTENDED_LEVELS: u8 = 3;

impl Settings {
    /// The settings for a person's conversation (terminal, gateways, the
    /// dashboard).
    pub fn attended(parallel: usize) -> Self {
        Self {
            parallel,
            unattended: false,
        }
    }

    /// The settings for a run nobody watches: fewer requests at once, so
    /// an unattended run does not burst through a rate limit, at most
    /// three levels and no seam pass.
    pub fn unattended(parallel: usize) -> Self {
        Self {
            parallel: parallel.min(super::UNATTENDED_PARALLEL),
            unattended: true,
        }
    }
}

/// The `draft` tool as the model sees it.
pub struct DraftTool {
    description: String,
}

impl DraftTool {
    pub fn new(settings: Settings) -> Self {
        Self {
            description: format!(
                "Writes a long text coarse to fine: a plan of {}-{} sections, then every \
                 section's gist at once, then every section's text at once ({} requests at a \
                 time), each section conditioned on the plan and the gists only. Use it for long \
                 documents with distinct sections (reports, design documents, READMEs), not for \
                 short answers or code. Returns the whole text as Markdown with a status line; \
                 it saves nothing, so call write_file to keep it. `request` says what to write \
                 (audience, purpose, facts and constraints: the writers see only this). \
                 `levels`: 2 goes straight from the plan to the text, 4 adds a paragraph outline \
                 and writes every paragraph at once (default 3). `polish`: one more pass that \
                 fixes repetition and transitions at the seams between sections.{}",
                super::MIN_SECTIONS,
                super::MAX_SECTIONS,
                settings.parallel,
                if settings.unattended {
                    " In this run levels stop at 3 and polish is off."
                } else {
                    ""
                }
            ),
        }
    }
}

#[async_trait]
impl Tool for DraftTool {
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
                "request": {
                    "type": "string",
                    "description": "What to write, with everything the writers need."
                },
                "levels": {"type": "integer", "enum": [2, 3, 4]},
                "polish": {"type": "boolean"}
            },
            "required": ["request"]
        })
    }

    /// Only reached when no agent runs the calls (a registry used on its
    /// own).
    async fn run(&self, _input: Value) -> ToolOutput {
        ToolOutput::error("draft is not available here")
    }
}

/// The request and options of a `draft` call. Unknown keys are ignored.
pub fn parse_input(
    input: &Value,
    settings: Settings,
    max_tokens: u32,
) -> Result<(String, Options), String> {
    let request = input
        .get("request")
        .and_then(Value::as_str)
        .ok_or("`request` must be a string")?
        .to_string();
    super::check_request(&request)?;
    let mut options = Options::new(settings.parallel, max_tokens);
    match input.get("levels") {
        None | Some(Value::Null) => {}
        Some(value) => {
            options.levels = value
                .as_u64()
                .and_then(|n| super::parse_levels(&n.to_string()).ok())
                .ok_or("`levels` must be 2, 3 or 4")?;
        }
    }
    match input.get("polish") {
        None | Some(Value::Null) => {}
        Some(value) => options.polish = value.as_bool().ok_or("`polish` must be true or false")?,
    }
    if settings.unattended {
        options.levels = options.levels.min(UNATTENDED_LEVELS);
        options.polish = false;
    }
    Ok((request, options))
}

/// The pieces Junees puts around the draft's text in [`output`] and in the
/// status line ([`status_line`]), which [`read_status`] reads back.
const SEPARATOR: &str = "\n---\n";
const CANCELLED_AFTER: &str = "; the levels finished before the cancel:\n\n";
const FAILED_BEFORE: &str = "draft failed: ";
const LIMIT_BEFORE: &str = "draft stopped: ";
const LIMIT_AFTER: &str = "; the levels finished before:\n\n";
const STATUS_START: &str = "[draft] levels ";
const FAILED_NOTE: &str = " (an earlier level's text stands in, marked in the text), ";
const ESTIMATE_BEFORE: &str = "s (about ";
const ESTIMATE_AFTER: &str = "s one call at a time)";

/// The tool result: the text, then one status line with everything the
/// model needs to act on it (levels, failed sections, calls, tokens). The
/// status line is always the last line ([`read_status`] relies on it).
pub fn output(draft: &Draft) -> ToolOutput {
    let status = status_line(draft);
    match &draft.stop {
        Stop::Done => ToolOutput::ok(format!("{}{SEPARATOR}{status}", draft.text())),
        Stop::Cancelled => ToolOutput::error(format!(
            "{}{CANCELLED_AFTER}{}{SEPARATOR}{status}",
            crate::agent::CANCELLED,
            draft.text()
        )),
        Stop::Failed(reason) => ToolOutput::error(format!("{FAILED_BEFORE}{reason}\n{status}")),
        Stop::Limit(reason) => ToolOutput::error(format!(
            "{LIMIT_BEFORE}{reason}{LIMIT_AFTER}{}{SEPARATOR}{status}",
            draft.text()
        )),
    }
}

/// `[draft] levels 3/3, 6 sections, ...` in English, for the model.
pub fn status_line(draft: &Draft) -> String {
    let usage = draft.usage();
    let failed = draft.failed_sections();
    let failed = if failed.is_empty() {
        "none".to_string()
    } else {
        format!(
            "{} (an earlier level's text stands in, marked in the text)",
            failed.join(", ")
        )
    };
    format!(
        "[draft] levels {}/{}{}, {} sections, failed sections: {failed}, {} calls, {} input \
         and {} output tokens, {:.1}s{}",
        draft.levels_done,
        draft.levels,
        if draft.polished { " + polish" } else { "" },
        draft.sections.len(),
        draft.calls.len(),
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens,
        usage.output_tokens,
        draft.elapsed.as_secs_f64(),
        // Only where it compares: see `Draft::sequential_comparison`.
        draft
            .sequential_comparison()
            .map(|estimate| format!(" (about {:.1}s one call at a time)", estimate.as_secs_f64()))
            .unwrap_or_default(),
    )
}

/// How a `draft` call ended, read back from its tool result by
/// [`read_status`]. The field names follow the dashboard's `draft_done`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// [`Stop::code`]: `done`, `cancelled`, `failed` or `limit`.
    pub stop: &'static str,
    /// Why it failed or stopped (`failed`, `limit`), as Junees wrote it.
    pub error: Option<String>,
    pub levels: u8,
    pub levels_done: u8,
    pub polished: bool,
    pub sections: usize,
    pub failed_sections: Vec<String>,
    pub calls: usize,
    /// With cache writes and reads.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// To the tenth of a second the status line gives.
    pub elapsed_ms: u64,
    /// The one-at-a-time estimate, only where it compares (as
    /// [`Draft::sequential_comparison`]): the draft finished and the
    /// estimate is longer than `elapsed_ms`. Lines from before that rule
    /// always carry one; it is dropped here the same way.
    pub sequential_ms: Option<u64>,
}

/// Reads how a `draft` call ended from its tool result (`content`,
/// `is_error`) as [`output`] made it, or `None` when it is not such a
/// result (a refusal, a bad input, a cut or replaced result).
///
/// Only what Junees wrote is read: the words before the text, the
/// separator and the status line, which is always the last line. The
/// draft's text in between is the model's and is never searched, so a line
/// in it that looks like a status line changes nothing.
///
/// `content` must be the whole result as the journal keeps it: a cut one
/// can end in the model's text, right after a line it made look like this.
pub fn read_status(content: &str, is_error: bool) -> Option<Status> {
    let (before, line) = content.rsplit_once('\n')?;
    let mut status = status_from_line(line)?;
    // What came before the status line: the text and its separator, or a
    // failure's reason.
    let text_before = || format!("{before}\n").ends_with(SEPARATOR).then_some(());
    let (stop, error) = if !is_error {
        text_before()?;
        (Stop::Done, None)
    } else if let Some(rest) = content.strip_prefix(crate::agent::CANCELLED) {
        rest.starts_with(CANCELLED_AFTER).then_some(())?;
        text_before()?;
        (Stop::Cancelled, None)
    } else if let Some(rest) = content.strip_prefix(LIMIT_BEFORE) {
        text_before()?;
        // The reason is Junees's and comes first; the text after it is the
        // model's, so the first match is the end of the reason.
        let (reason, _) = rest.split_once(LIMIT_AFTER)?;
        (Stop::Limit(reason.to_string()), Some(reason.to_string()))
    } else if let Some(reason) = before.strip_prefix(FAILED_BEFORE) {
        (Stop::Failed(reason.to_string()), Some(reason.to_string()))
    } else {
        return None;
    };
    status.stop = stop.code();
    status.error = error;
    if stop != Stop::Done
        || status
            .sequential_ms
            .is_some_and(|estimate| estimate <= status.elapsed_ms)
    {
        status.sequential_ms = None;
    }
    Some(status)
}

/// The fields of a status line ([`status_line`]); `stop` and `error` are
/// left for [`read_status`].
fn status_from_line(line: &str) -> Option<Status> {
    let rest = line.strip_prefix(STATUS_START)?;
    let (levels_done, rest) = rest.split_once('/')?;
    let (levels, rest) = rest.split_once(", ")?;
    let (levels, polished) = match levels.strip_suffix(" + polish") {
        Some(levels) => (levels, true),
        None => (levels, false),
    };
    let (sections, rest) = rest.split_once(" sections, failed sections: ")?;
    let (failed_sections, rest) = match rest.strip_prefix("none, ") {
        Some(rest) => (Vec::new(), rest),
        None => {
            // Section ids are plain (`[A-Za-z0-9_-]`), so `, ` splits them.
            let (ids, rest) = rest.split_once(FAILED_NOTE)?;
            let ids: Vec<String> = ids.split(", ").map(str::to_string).collect();
            if !ids.iter().all(|id| super::plan::plain_id(id)) {
                return None;
            }
            (ids, rest)
        }
    };
    let (calls, rest) = rest.split_once(" calls, ")?;
    let (input_tokens, rest) = rest.split_once(" input and ")?;
    let (output_tokens, rest) = rest.split_once(" output tokens, ")?;
    let (elapsed, estimate) = match rest.split_once(ESTIMATE_BEFORE) {
        Some((elapsed, estimate)) => (elapsed, Some(estimate.strip_suffix(ESTIMATE_AFTER)?)),
        None => (rest.strip_suffix('s')?, None),
    };
    Some(Status {
        stop: Stop::Done.code(),
        error: None,
        levels: whole(levels)?,
        levels_done: whole(levels_done)?,
        polished,
        sections: whole(sections)?,
        failed_sections,
        calls: whole(calls)?,
        input_tokens: whole(input_tokens)?,
        output_tokens: whole(output_tokens)?,
        elapsed_ms: tenths_as_ms(elapsed)?,
        sequential_ms: match estimate {
            Some(estimate) => Some(tenths_as_ms(estimate)?),
            None => None,
        },
    })
}

/// A whole number written as digits only.
fn whole<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Seconds with one decimal (`12.3`) as milliseconds.
fn tenths_as_ms(text: &str) -> Option<u64> {
    let (seconds, tenth) = text.split_once('.')?;
    if tenth.len() != 1 {
        return None;
    }
    let seconds: u64 = whole(seconds)?;
    let tenth: u64 = whole(tenth)?;
    seconds.checked_mul(1_000)?.checked_add(tenth * 100)
}
