//! Searching the saved sessions: the `session_search` tool, `/search` and
//! `junees sessions search`.
//!
//! The journals of `$JUNEES_HOME/sessions` (and the subagent journals in
//! their folders) are read line by line, newest file first, without an
//! index: what the user typed, the model's text, compaction summaries and
//! the start of each tool result are matched against a [`Query`]. Lines
//! that do not parse are skipped and counted, lines over
//! [`MAX_LINE_BYTES`] are skipped without being held in memory, and the
//! search stops at a time limit with what it found so far.

pub mod format;
pub mod query;
mod scan;
pub mod tool;

use std::path::Path;
use std::time::{Duration, Instant};

pub use query::Query;
pub use tool::register;

use crate::session;

/// The name of the model's tool.
pub const NAME: &str = "session_search";

/// Sessions shown when no limit is given.
pub const DEFAULT_LIMIT: usize = 5;

/// The most sessions one search shows.
pub const MAX_LIMIT: usize = 20;

/// Snippets shown per session; further matches are only counted.
pub const SNIPPETS_PER_SESSION: usize = 3;

/// Characters of one snippet, about.
pub const SNIPPET_CHARS: usize = 240;

/// Journal lines longer than this are skipped (and counted) unread.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Bytes of a user input or model text that are searched.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Bytes of a tool result that are searched: its start, which says what it
/// is about, not every line a command printed or a file held.
pub const MAX_TOOL_RESULT_BYTES: usize = 4 * 1024;

/// How long the model's search may read journals.
pub const TOOL_TIME_LIMIT: Duration = Duration::from_secs(5);

/// How long `/search` and `junees sessions search` may read journals.
pub const HUMAN_TIME_LIMIT: Duration = Duration::from_secs(15);

/// The source name of sessions started at a terminal (no
/// [`session::Meta::source`]): conversations, `-p` and task sessions.
pub const TERMINAL: &str = "terminal";

/// Every source a search can be limited to.
pub const SOURCES: [&str; 4] = [TERMINAL, session::DISCORD, session::CRON, session::WEBHOOK];

/// What to search and where.
#[derive(Debug, Clone)]
pub struct Options {
    pub query: Query,
    /// Sessions to show, newest first.
    pub limit: usize,
    /// Only sessions whose working folder is exactly this one; `None`
    /// searches every folder.
    pub folder: Option<String>,
    /// Only sessions from these sources ([`SOURCES`]).
    pub sources: Vec<String>,
    /// Only the session with this id, or whose id starts with it (its
    /// subagents' journals included).
    pub session: Option<String>,
    pub time_limit: Duration,
}

/// One matching place of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// Who wrote it: `user`, `assistant`, `summary` or `tool <name>`.
    pub label: String,
    pub text: String,
}

/// A session with at least one match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: String,
    /// Unix seconds.
    pub created_at: u64,
    /// Last write of the journal, in Unix seconds.
    pub updated: u64,
    pub cwd: String,
    /// [`TERMINAL`] or the session's [`session::Meta::source`].
    pub source: String,
    pub channel: Option<String>,
    pub backend: Option<String>,
    /// The first [`SNIPPETS_PER_SESSION`] matches.
    pub snippets: Vec<Snippet>,
    /// Every match in the session.
    pub matches: usize,
}

/// What a search found, and how much of the journals it read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub hits: Vec<Hit>,
    /// Journal files under the sessions folder.
    pub files: usize,
    /// Journal files read (the rest was out of scope or not reached).
    pub read: usize,
    /// Lines that are not a journal entry this version reads.
    pub broken_lines: usize,
    /// Lines over [`MAX_LINE_BYTES`], skipped.
    pub long_lines: usize,
    /// The time limit stopped the search before every file was read.
    pub timed_out: bool,
}

/// Searches the sessions of the state folder `home`. Only listing the
/// sessions folder can fail; unreadable journals are skipped.
pub fn search(home: &Path, options: &Options) -> anyhow::Result<Outcome> {
    let deadline = Instant::now() + options.time_limit;
    scan::run(&session::sessions_dir(home), options, deadline)
}

/// Checks source names given by the model or the user: each must be one
/// of [`SOURCES`]. `all` stands for every one.
pub fn parse_sources<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Vec<String>, String> {
    let mut sources = Vec::new();
    for name in names {
        let name = name.trim();
        if name == "all" {
            return Ok(SOURCES.iter().map(|s| s.to_string()).collect());
        }
        if !SOURCES.contains(&name) {
            return Err(format!(
                "unknown session source `{}`; use {} or all",
                crate::prompt_text::escape_hidden(name),
                SOURCES.join(", ")
            ));
        }
        if !sources.iter().any(|s: &String| s == name) {
            sources.push(name.to_string());
        }
    }
    if sources.is_empty() {
        return Err(format!("name at least one source: {}", SOURCES.join(", ")));
    }
    Ok(sources)
}

#[cfg(test)]
mod tests;
