//! The `[[hooks]]` tables: of the settings file (after its flat
//! settings), of `$JUNEES_HOME/hooks.toml` and of a project's
//! `.junees/hooks.toml`.
//!
//! ```toml
//! [[hooks]]
//! event = "pre_tool"          # pre_tool | post_tool | turn_end | session_start | user_prompt
//! match = "bash|write_file"   # tool names, a whole-name regex (pre_tool, post_tool only)
//! command = ["./scripts/check.sh"]   # an argv array, never a shell string
//! timeout = "10s"             # default 10s, at most 60s
//! on_error = "allow"          # allow | block (pre_tool and user_prompt only)
//! ```
//!
//! The same small TOML subset as the settings file: one `key = value` per
//! line, strings in `"..."` or `'...'`, and one-line arrays of strings.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;

/// The table header a hook starts with.
pub const HEADER: &str = "[[hooks]]";
/// A hook's time limit when it sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest time limit a hook may set.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(60);
/// Characters of a `match` pattern.
const MAX_MATCH_CHARS: usize = 1000;

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Before a tool call, before its approval question.
    PreTool,
    /// After a tool call ran (or was refused), with its result.
    PostTool,
    /// After a turn ended, with the answer or the error.
    TurnEnd,
    /// Before the first input of a new session.
    SessionStart,
    /// Before a person's input goes to the model.
    UserPrompt,
}

impl Event {
    pub const ALL: [Event; 5] = [
        Event::PreTool,
        Event::PostTool,
        Event::TurnEnd,
        Event::SessionStart,
        Event::UserPrompt,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Event::PreTool => "pre_tool",
            Event::PostTool => "post_tool",
            Event::TurnEnd => "turn_end",
            Event::SessionStart => "session_start",
            Event::UserPrompt => "user_prompt",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.name() == name)
    }

    /// Its hooks may block what comes next (exit code 2).
    pub fn can_block(self) -> bool {
        matches!(self, Event::PreTool | Event::UserPrompt)
    }

    /// It is about a tool call, so `match` applies.
    pub fn has_tool(self) -> bool {
        matches!(self, Event::PreTool | Event::PostTool)
    }
}

/// What a blocking hook's failure (another exit code, a time-out, a
/// program that does not start) does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnError {
    /// Go on, with a warning.
    #[default]
    Allow,
    /// Block, as exit code 2 would.
    Block,
}

impl OnError {
    pub fn name(self) -> &'static str {
        match self {
            OnError::Allow => "allow",
            OnError::Block => "block",
        }
    }
}

/// A `match` pattern: tool names it matches whole.
#[derive(Debug, Clone)]
pub struct Matcher {
    /// As written.
    pub source: String,
    regex: Regex,
}

impl Matcher {
    pub fn new(source: &str) -> Result<Self> {
        if source.chars().count() > MAX_MATCH_CHARS {
            bail!("`match` is longer than {MAX_MATCH_CHARS} characters");
        }
        let regex = regex::RegexBuilder::new(&format!("^(?:{source})$"))
            .size_limit(1 << 20)
            .build()
            .map_err(|err| anyhow::anyhow!("`match` is not a valid regex: {err}"))?;
        Ok(Self {
            source: source.to_string(),
            regex,
        })
    }

    /// Whether it matches the whole tool name.
    pub fn matches(&self, tool: &str) -> bool {
        self.regex.is_match(tool)
    }
}

/// One `[[hooks]]` table.
#[derive(Debug, Clone)]
pub struct Spec {
    pub event: Event,
    /// `None`: every tool.
    pub matcher: Option<Matcher>,
    /// The program and its arguments.
    pub command: Vec<String>,
    pub timeout: Duration,
    pub on_error: OnError,
    /// The line of its `[[hooks]]` header in its file.
    pub line: usize,
}

impl Spec {
    /// Whether it runs for a call of `tool` (always for events without a
    /// tool).
    pub fn applies_to(&self, tool: Option<&str>) -> bool {
        match (&self.matcher, tool) {
            (Some(matcher), Some(tool)) => matcher.matches(tool),
            _ => true,
        }
    }

    /// The command as one line, each word quoted when needed.
    pub fn shown(&self) -> String {
        shown_command(&self.command)
    }
}

/// `argv` as one line, the words with spaces or quotes quoted, hidden
/// characters escaped.
pub fn shown_command(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            let word = crate::prompt_text::escape_hidden(word);
            if word.is_empty() || word.contains([' ', '"', '\'', '\\']) {
                format!("{word:?}")
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A value of the subset.
enum Value {
    Text(String),
    List(Vec<String>),
}

/// The hooks of `text`, whose first line is line `first_line` of its file
/// (errors name the file's lines). Nothing but `[[hooks]]` tables (and
/// comments) may be in it.
pub fn parse(text: &str, first_line: usize) -> Result<Vec<Spec>> {
    let mut specs = Vec::new();
    let mut table: Option<(usize, Vec<(String, Value)>)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = first_line + index;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if !is_header(line) {
                bail!("line {number}: only `{HEADER}` tables are supported here");
            }
            if let Some((start, keys)) = table.take() {
                specs.push(spec(start, keys)?);
            }
            table = Some((number, Vec::new()));
            continue;
        }
        let Some((_, keys)) = table.as_mut() else {
            bail!("line {number}: put each hook in a table that starts with `{HEADER}`");
        };
        let Some((key, rest)) = line.split_once('=') else {
            bail!("line {number}: expected `key = value`");
        };
        let key = key.trim();
        if !KEYS.contains(&key) {
            let shown =
                if key.len() <= 32 && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    format!(" `{key}`")
                } else {
                    String::new()
                };
            bail!(
                "line {number}: unknown hook key{shown} (known: {})",
                KEYS.join(", ")
            );
        }
        if keys.iter().any(|(known, _)| known == key) {
            bail!("line {number}: `{key}` is set twice in this hook");
        }
        let value = value(rest.trim()).with_context(|| format!("line {number}: `{key}`"))?;
        keys.push((key.to_string(), value));
    }
    if let Some((start, keys)) = table {
        specs.push(spec(start, keys)?);
    }
    Ok(specs)
}

/// Whether `line` is a `[[hooks]]` header (spaces inside the brackets and
/// a comment after it allowed).
pub fn is_header(line: &str) -> bool {
    let line = line.trim();
    let Some(rest) = line.strip_prefix("[[") else {
        return false;
    };
    let Some((name, after)) = rest.split_once("]]") else {
        return false;
    };
    let after = after.trim();
    name.trim() == "hooks" && (after.is_empty() || after.starts_with('#'))
}

const KEYS: &[&str] = &["event", "match", "command", "timeout", "on_error"];

fn spec(line: usize, keys: Vec<(String, Value)>) -> Result<Spec> {
    let at = |key: &str| keys.iter().find(|(name, _)| name == key).map(|(_, v)| v);
    let text = |key: &str| -> Result<Option<&str>> {
        match at(key) {
            None => Ok(None),
            Some(Value::Text(text)) => Ok(Some(text)),
            Some(Value::List(_)) => bail!("hook at line {line}: `{key}` must be a string"),
        }
    };
    let Some(event) = text("event")? else {
        bail!(
            "hook at line {line}: `event` is missing (one of {})",
            event_names()
        );
    };
    let Some(event) = Event::parse(event) else {
        bail!(
            "hook at line {line}: unknown event (one of {})",
            event_names()
        );
    };
    let command = match at("command") {
        None => bail!("hook at line {line}: `command` is missing"),
        Some(Value::Text(_)) => bail!(
            "hook at line {line}: `command` must be an array of words, like \
             [\"./scripts/check.sh\", \"--fast\"]; shell strings are not accepted"
        ),
        Some(Value::List(words)) => words.clone(),
    };
    if command
        .first()
        .is_none_or(|program| program.trim().is_empty())
    {
        bail!("hook at line {line}: `command` needs a program as its first word");
    }
    if command.iter().any(|word| word.contains('\0')) {
        bail!("hook at line {line}: `command` holds a NUL character");
    }
    let matcher = match text("match")? {
        None => None,
        Some(_) if !event.has_tool() => {
            bail!("hook at line {line}: `match` is only for pre_tool and post_tool hooks")
        }
        Some(source) => Some(Matcher::new(source).with_context(|| format!("hook at line {line}"))?),
    };
    let timeout = match text("timeout")? {
        None => DEFAULT_TIMEOUT,
        Some(raw) => parse_duration(raw).with_context(|| format!("hook at line {line}"))?,
    };
    let on_error = match text("on_error")? {
        None => OnError::Allow,
        Some("allow") => OnError::Allow,
        Some("block") if event.can_block() => OnError::Block,
        Some("block") => bail!(
            "hook at line {line}: `on_error = \"block\"` is only for pre_tool and user_prompt \
             hooks; {} hooks never block",
            event.name()
        ),
        Some(_) => bail!("hook at line {line}: `on_error` is \"allow\" or \"block\""),
    };
    Ok(Spec {
        event,
        matcher,
        command,
        timeout,
        on_error,
        line,
    })
}

fn event_names() -> String {
    Event::ALL.map(Event::name).join(", ")
}

/// `\"10s\"`, `\"500ms\"` or `\"1m\"`: more than nothing, at most
/// [`MAX_TIMEOUT`].
pub fn parse_duration(raw: &str) -> Result<Duration> {
    let raw = raw.trim();
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (digits, unit) = raw.split_at(split);
    let number: u64 = digits
        .parse()
        .ok()
        .context("`timeout` is a number with a unit, like \"10s\" or \"500ms\"")?;
    let duration = match unit {
        "ms" => Duration::from_millis(number),
        "s" => Duration::from_secs(number),
        "m" => Duration::from_secs(number.saturating_mul(60)),
        _ => bail!("`timeout` needs a unit: ms, s or m (like \"10s\")"),
    };
    if duration.is_zero() {
        bail!("`timeout` must be more than 0");
    }
    if duration > MAX_TIMEOUT {
        bail!("`timeout` is at most 60s");
    }
    Ok(duration)
}

fn value(text: &str) -> Result<Value> {
    let (value, rest) = if let Some(body) = text.strip_prefix('[') {
        let mut words = Vec::new();
        let mut rest = body.trim_start();
        loop {
            if let Some(after) = rest.strip_prefix(']') {
                break (Value::List(words), after);
            }
            let (word, after) = string(rest).context(
                "arrays hold strings and close on the same line, like [\"./check.sh\", \"-q\"]",
            )?;
            words.push(word);
            let after = after.trim_start();
            rest = match after.strip_prefix(',') {
                Some(after) => after.trim_start(),
                None if after.starts_with(']') => after,
                None => bail!("expected `,` or `]` in the array"),
            };
        }
    } else {
        let (text, rest) = string(text).context("the value needs quotes")?;
        (Value::Text(text), rest)
    };
    let rest = rest.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        bail!("unexpected text after the value");
    }
    Ok(value)
}

/// A string at the start of `text`, and what follows it.
fn string(text: &str) -> Result<(String, &str)> {
    if let Some(body) = text.strip_prefix('"') {
        crate::config_file::basic_string(body)
    } else if let Some(body) = text.strip_prefix('\'') {
        let end = body.find('\'').context("the string has no closing '")?;
        Ok((body[..end].to_string(), &body[end + 1..]))
    } else {
        bail!("expected a string in quotes")
    }
}
