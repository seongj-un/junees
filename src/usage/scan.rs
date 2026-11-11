//! Reading the usage entries of the journals, line by line.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Context;
use serde::Deserialize;

use super::{Kind, Record};
use crate::search::{Line, read_line};
use crate::session::{self, Entry, Meta};

/// Bytes read from a journal at a time.
const READ_BUFFER: usize = 64 * 1024;

/// Journal lines longer than this are skipped unread. Usage lines are
/// short; only a Claude Code turn with a huge input or answer comes near.
const MAX_LINE_BYTES: usize = crate::search::MAX_LINE_BYTES;

/// The entry types that hold usage.
const USAGE_TYPES: [&[u8]; 4] = [
    b"usage",
    b"summary_usage",
    b"subagent_usage",
    b"claude_turn",
];

/// How much of the journals was read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Journals read.
    pub journals: usize,
    /// Usage lines (or lines that could not tell their type) that are not
    /// an entry this version reads: broken lines, a torn last line.
    pub broken_lines: usize,
    /// Lines over the size limit, skipped.
    pub long_lines: usize,
}

/// A journal line other than the meta line, with what Junees stamps on it.
#[derive(Deserialize)]
struct LineIn {
    #[serde(flatten)]
    entry: Entry,
    #[serde(default)]
    ts: Option<u64>,
    /// See [`session::Journal::record_usage`].
    #[serde(default)]
    model: Option<String>,
}

/// The `type` of a line as Junees writes it (`{"type":"usage",...`).
/// Lines written another way (by hand, by a JSON tool) are parsed whole.
fn written_type(line: &[u8]) -> Option<&[u8]> {
    let rest = line.strip_prefix(b"{\"type\":\"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    Some(&rest[..end])
}

/// No time limit: usage reports read every journal in their range.
fn no_deadline() -> Instant {
    Instant::now() + Duration::from_secs(365 * 24 * 3600)
}

/// Reads the journal at `path`: its meta line, then every usage entry,
/// given to `each`. `wanted` decides from the meta line whether to read on.
/// Returns `None` when the file is not a journal (or cannot be opened).
fn read_journal(
    path: &Path,
    stats: &mut Stats,
    wanted: &mut dyn FnMut(&Meta) -> bool,
    each: &mut dyn FnMut(&Meta, Record),
) -> io::Result<Option<Meta>> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let mut line = Vec::new();
    let deadline = no_deadline();
    if read_line(&mut reader, &mut line, MAX_LINE_BYTES, deadline)? != Line::Read {
        return Ok(None);
    }
    let Ok(Entry::Meta(meta)) = serde_json::from_slice::<Entry>(&line) else {
        return Ok(None);
    };
    if !wanted(&meta) {
        return Ok(Some(meta));
    }
    stats.journals += 1;
    loop {
        match read_line(&mut reader, &mut line, MAX_LINE_BYTES, deadline) {
            Ok(Line::Read) => {}
            Ok(Line::TooLong) => {
                stats.long_lines += 1;
                continue;
            }
            Ok(Line::End | Line::TimeUp) | Err(_) => break,
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        if let Some(kind) = written_type(&line)
            && !USAGE_TYPES.contains(&kind)
        {
            continue;
        }
        let Ok(parsed) = serde_json::from_slice::<LineIn>(&line) else {
            stats.broken_lines += 1;
            continue;
        };
        let (kind, usage) = match parsed.entry {
            Entry::Usage(usage) => (Kind::Call, usage),
            Entry::SummaryUsage(usage) => (Kind::Summary, usage),
            Entry::SubagentUsage(usage) => (Kind::Subagents, usage),
            Entry::ClaudeTurn(turn) => (Kind::ClaudeCode, turn.usage),
            _ => continue,
        };
        let model = match kind {
            Kind::ClaudeCode => meta.model.clone(),
            _ => parsed.model.unwrap_or_else(|| meta.model.clone()),
        };
        let record = Record {
            ts: parsed.ts.unwrap_or(meta.created_at),
            kind,
            model,
            usage,
        };
        each(&meta, record);
    }
    Ok(Some(meta))
}

/// The usage records of one session's journal (its subagents' tokens
/// included through `subagent_usage`). A journal not written yet (a new
/// session before its first entry) has none.
pub fn read_session(path: &Path) -> anyhow::Result<(Vec<Record>, Stats)> {
    let mut stats = Stats::default();
    let mut records = Vec::new();
    match read_journal(path, &mut stats, &mut |_| true, &mut |_, record| {
        records.push(record)
    }) {
        Ok(_) => Ok((records, stats)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok((records, stats)),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Where a session took place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Origin {
    pub cwd: String,
    /// `terminal` or the session's source ([`crate::search::origin`]).
    pub source: String,
}

/// Gives `each` every usage record written at or after `since` (Unix
/// seconds) in the sessions of the state folder `home`, of the working
/// folder `folder` only when one is given. Subagent journals are not read:
/// their parent's `subagent_usage` entries count their tokens.
pub(super) fn scan(
    home: &Path,
    since: u64,
    folder: Option<&str>,
    each: &mut dyn FnMut(&Origin, Record),
) -> anyhow::Result<Stats> {
    let dir = session::sessions_dir(home);
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Stats::default()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        // Every line was written by the last change of the file.
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(u64::MAX, |age| age.as_secs());
        if modified < since {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    let old_channels: HashMap<String, String> = crate::search::gateway_channels(home);
    let mut stats = Stats::default();
    for path in paths {
        let mut origin = None;
        let mut wanted = |meta: &Meta| {
            let (source, _) = crate::search::origin(meta, &old_channels);
            origin = Some(Origin {
                cwd: meta.cwd.clone(),
                source,
            });
            folder.is_none_or(|folder| folder == meta.cwd)
        };
        let mut found = Vec::new();
        // Unreadable journals are skipped, as by session search.
        let _ = read_journal(&path, &mut stats, &mut wanted, &mut |_, record| {
            if record.ts >= since {
                found.push(record);
            }
        });
        if let Some(origin) = origin {
            for record in found {
                each(&origin, record);
            }
        }
    }
    Ok(stats)
}
