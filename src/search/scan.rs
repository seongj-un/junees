//! Reading the journals: which files, line by line, and what in them is
//! searched.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use super::query::{cut, lower, snippet};
use super::{
    Hit, MAX_LINE_BYTES, MAX_TEXT_BYTES, MAX_TOOL_RESULT_BYTES, Options, Outcome, SNIPPET_CHARS,
    SNIPPETS_PER_SESSION, Snippet, TERMINAL,
};
use crate::message::{ContentBlock, Role};
use crate::session::{CLEARED, DISCORD, Entry, INTERRUPTED, Meta, SUMMARY_HEADER};

/// Bytes read from a journal at a time.
const READ_BUFFER: usize = 64 * 1024;

struct Journal {
    path: PathBuf,
    modified: SystemTime,
}

/// The journals in `dir` and in its subfolders (subagents), most recently
/// written first.
fn journals(dir: &Path) -> anyhow::Result<Vec<Journal>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut found = Vec::new();
    let mut push = |path: PathBuf| {
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(modified) = fs::metadata(&path).and_then(|meta| meta.modified())
        {
            found.push(Journal { path, modified });
        }
    };
    for entry in read.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                for sub in fs::read_dir(&path).into_iter().flatten().flatten() {
                    if sub.file_type().is_ok_and(|kind| kind.is_file()) {
                        push(sub.path());
                    }
                }
            }
            Ok(kind) if kind.is_file() => push(path),
            _ => {}
        }
    }
    found.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.path.cmp(&a.path))
    });
    Ok(found)
}

/// Session id to channel id, from the Discord gateway's index
/// ([`crate::gateway::sessions::INDEX_FILE`]): it also names the channel
/// sessions from before meta lines recorded their source. Empty when the
/// index is missing or broken.
pub(crate) fn gateway_channels(home: &Path) -> HashMap<String, String> {
    fs::read_to_string(home.join(crate::gateway::sessions::INDEX_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<BTreeMap<String, String>>(&text).ok())
        .map(|index| {
            index
                .into_iter()
                .map(|(channel, id)| (id, channel))
                .collect()
        })
        .unwrap_or_default()
}

/// Where a session took place, and its channel. Sessions from before meta
/// lines named their [`Meta::source`] count as Discord sessions when the
/// meta names a channel or the gateway's index (`old_channels`) ties a
/// channel to the session or, for a subagent, to its parent; only the
/// rest started at a terminal.
pub(crate) fn origin(
    meta: &Meta,
    old_channels: &HashMap<String, String>,
) -> (String, Option<String>) {
    if let Some(source) = &meta.source {
        return (source.clone(), meta.channel.clone());
    }
    let parent = meta.id.split('/').next().unwrap_or_default();
    match meta
        .channel
        .clone()
        .or_else(|| old_channels.get(parent).cloned())
    {
        Some(channel) => (DISCORD.to_string(), Some(channel)),
        None => (TERMINAL.to_string(), None),
    }
}

pub(super) fn run(
    dir: &Path,
    options: &Options,
    old_channels: &HashMap<String, String>,
    deadline: Instant,
) -> anyhow::Result<Outcome> {
    let journals = journals(dir)?;
    let mut outcome = Outcome {
        files: journals.len(),
        ..Outcome::default()
    };
    for journal in &journals {
        if outcome.hits.len() >= options.limit {
            break;
        }
        if Instant::now() >= deadline {
            outcome.timed_out = true;
            break;
        }
        let (hit, finished) = read_journal(journal, options, old_channels, deadline, &mut outcome);
        outcome.hits.extend(hit);
        if !finished {
            outcome.timed_out = true;
            break;
        }
    }
    Ok(outcome)
}

/// What one line read gave.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Line {
    /// A line is in the buffer, without its `\n`.
    Read,
    /// The line was longer than the limit; it was read past, not kept.
    TooLong,
    End,
    TimeUp,
}

/// Reads the next line into `line`, keeping at most `max` bytes of it in
/// memory: a longer line is consumed and reported as [`Line::TooLong`].
pub(crate) fn read_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max: usize,
    deadline: Instant,
) -> io::Result<Line> {
    line.clear();
    let mut started = false;
    let mut too_long = false;
    loop {
        if Instant::now() >= deadline {
            return Ok(Line::TimeUp);
        }
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        if available.is_empty() {
            return Ok(match (started, too_long) {
                (false, _) => Line::End,
                (true, true) => Line::TooLong,
                (true, false) => Line::Read,
            });
        }
        started = true;
        let (chunk, used, done) = match available.iter().position(|&b| b == b'\n') {
            Some(n) => (&available[..n], n + 1, true),
            None => (available, available.len(), false),
        };
        if !too_long {
            if line.len() + chunk.len() > max {
                too_long = true;
                line.clear();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        reader.consume(used);
        if done {
            return Ok(if too_long { Line::TooLong } else { Line::Read });
        }
    }
}

fn in_scope(meta: &Meta, source: &str, options: &Options) -> bool {
    options.sources.iter().any(|s| s == source)
        && options
            .folder
            .as_ref()
            .is_none_or(|folder| *folder == meta.cwd)
        && options
            .session
            .as_ref()
            .is_none_or(|prefix| meta.id.starts_with(prefix.as_str()))
}

/// Searches one journal. Returns its hit, if it matched, and whether it
/// was read to the end before the deadline.
fn read_journal(
    journal: &Journal,
    options: &Options,
    old_channels: &HashMap<String, String>,
    deadline: Instant,
    outcome: &mut Outcome,
) -> (Option<Hit>, bool) {
    let Ok(file) = File::open(&journal.path) else {
        return (None, true);
    };
    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let mut line = Vec::new();
    match read_line(&mut reader, &mut line, MAX_LINE_BYTES, deadline) {
        Ok(Line::Read) => {}
        Ok(Line::TimeUp) => return (None, false),
        _ => return (None, true),
    }
    let Ok(Entry::Meta(meta)) = serde_json::from_slice::<Entry>(&line) else {
        return (None, true);
    };
    let (source, channel) = origin(&meta, old_channels);
    if !in_scope(&meta, &source, options) {
        return (None, true);
    }
    outcome.read += 1;
    let mut hit = Hit {
        id: meta.id,
        created_at: meta.created_at,
        updated: journal
            .modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        cwd: meta.cwd,
        source,
        channel,
        backend: meta.backend,
        snippets: Vec::new(),
        matches: 0,
    };
    let mut tools = HashMap::new();
    let finished = loop {
        match read_line(&mut reader, &mut line, MAX_LINE_BYTES, deadline) {
            Ok(Line::Read) => {}
            Ok(Line::TooLong) => {
                outcome.long_lines += 1;
                continue;
            }
            Ok(Line::TimeUp) => break false,
            Ok(Line::End) | Err(_) => break true,
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(&line) else {
            outcome.broken_lines += 1;
            continue;
        };
        for (label, text, max) in texts(entry, &mut tools) {
            let text = cut(&text, max);
            if !options.query.matches(&lower(text)) {
                continue;
            }
            hit.matches += 1;
            if hit.snippets.len() < SNIPPETS_PER_SESSION {
                hit.snippets.push(Snippet {
                    label,
                    text: snippet(text, &options.query, SNIPPET_CHARS),
                });
            }
        }
    };
    ((hit.matches > 0).then_some(hit), finished)
}

/// The searched texts of one entry, with who wrote them and how many of
/// their bytes are searched. `tools` maps the ids of the tool calls seen
/// so far to their tool names, so a result can say which tool gave it.
/// Results of earlier searches are left out: they would match again.
fn texts(entry: Entry, tools: &mut HashMap<String, String>) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();
    match entry {
        Entry::Message(message) => {
            for block in message.content {
                match (message.role, block) {
                    (Role::User, ContentBlock::Text { text, .. }) => {
                        let label = if text.starts_with(SUMMARY_HEADER) {
                            "summary"
                        } else {
                            "user"
                        };
                        out.push((label.to_string(), text, MAX_TEXT_BYTES));
                    }
                    (Role::Assistant, ContentBlock::Text { text, .. }) => {
                        out.push(("assistant".to_string(), text, MAX_TEXT_BYTES));
                    }
                    (Role::Assistant, ContentBlock::ToolUse { id, name, .. }) => {
                        tools.insert(id, name);
                    }
                    (
                        Role::User,
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        },
                    ) => {
                        let name = tools.remove(&tool_use_id).unwrap_or_else(|| "?".into());
                        if name == super::NAME || content == CLEARED || content == INTERRUPTED {
                            continue;
                        }
                        out.push((format!("tool {name}"), content, MAX_TOOL_RESULT_BYTES));
                    }
                    _ => {}
                }
            }
        }
        Entry::AppendText { text } => out.push(("user".to_string(), text, MAX_TEXT_BYTES)),
        Entry::ClaudeTurn(turn) => {
            out.push(("user".to_string(), turn.input, MAX_TEXT_BYTES));
            out.push(("assistant".to_string(), turn.text, MAX_TEXT_BYTES));
        }
        Entry::Compact { summary, .. } => {
            out.push(("summary".to_string(), summary, MAX_TEXT_BYTES));
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn long_lines_are_read_past_without_keeping_them() {
        let data = format!("short\n{}\nnext\nlast", "x".repeat(100));
        // A small buffer, so lines span several reads.
        let mut reader = BufReader::with_capacity(8, data.as_bytes());
        let mut line = Vec::new();
        let mut seen = Vec::new();
        loop {
            let got = read_line(&mut reader, &mut line, 10, later()).unwrap();
            if got == Line::End {
                break;
            }
            seen.push((got, String::from_utf8(line.clone()).unwrap()));
            assert!(line.capacity() < 64, "{}", line.capacity());
        }
        assert_eq!(
            seen,
            [
                (Line::Read, "short".to_string()),
                (Line::TooLong, String::new()),
                (Line::Read, "next".to_string()),
                (Line::Read, "last".to_string()),
            ]
        );
        let data = "x".repeat(20);
        let mut reader = BufReader::new(data.as_bytes());
        assert_eq!(
            read_line(&mut reader, &mut line, 10, later()).unwrap(),
            Line::TooLong
        );
        assert_eq!(
            read_line(&mut reader, &mut line, 10, Instant::now()).unwrap(),
            Line::TimeUp
        );
    }
}
