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

/// The tool result: the text, then one status line with everything the
/// model needs to act on it (levels, failed sections, calls, tokens).
pub fn output(draft: &Draft) -> ToolOutput {
    let status = status_line(draft);
    match &draft.stop {
        Stop::Done => ToolOutput::ok(format!("{}\n---\n{status}", draft.text())),
        Stop::Cancelled => ToolOutput::error(format!(
            "{}; the levels finished before the cancel:\n\n{}\n---\n{status}",
            crate::agent::CANCELLED,
            draft.text()
        )),
        Stop::Failed(reason) => ToolOutput::error(format!("draft failed: {reason}\n{status}")),
        Stop::Limit(reason) => ToolOutput::error(format!(
            "draft stopped: {reason}; the levels finished before:\n\n{}\n---\n{status}",
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
         and {} output tokens, {:.1}s (about {:.1}s one call at a time)",
        draft.levels_done,
        draft.levels,
        if draft.polished { " + polish" } else { "" },
        draft.sections.len(),
        draft.calls.len(),
        usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens,
        usage.output_tokens,
        draft.elapsed.as_secs_f64(),
        draft.sequential_estimate().as_secs_f64(),
    )
}
