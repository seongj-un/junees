//! Reading the usage entries of the journals, line by line.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Context;
use serde::Deserialize;

use super::{Kind, Prices, Record};
use crate::message::Usage;
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
            one_call: matches!(kind, Kind::Call | Kind::Summary),
        };
        each(&meta, record);
    }
    Ok(Some(meta))
}

/// The usage records of one session's journal, its subagents' tokens
/// included (see [`split_subagents`]). A journal not written yet (a new
/// session before its first entry) has none.
pub fn read_session(path: &Path, prices: &Prices) -> anyhow::Result<(Vec<Record>, Stats)> {
    let mut stats = Stats::default();
    let mut records = Vec::new();
    match read_journal(path, &mut stats, &mut |_| true, &mut |_, record| {
        records.push(record)
    }) {
        Ok(_) => Ok((split_subagents(path, records, prices), stats)),
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
/// folder `folder` only when one is given. Subagent journals are read only
/// through their parent ([`split_subagents`]).
pub(super) fn scan(
    home: &Path,
    since: u64,
    folder: Option<&str>,
    prices: &Prices,
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
            found.push(record)
        });
        if let Some(origin) = origin {
            // Before the range check: a subagent's calls come before the
            // entry that sums them.
            for record in split_subagents(&path, found, prices) {
                if record.ts >= since {
                    each(&origin, record);
                }
            }
        }
    }
    Ok(stats)
}

/// The usage `records` of the journal at `path`, with the `subagent_usage`
/// entries of models priced by the prompt length of each call (Haiku 5.5)
/// replaced by the calls they add up to, read from the subagents' own
/// journals (`sessions/<id>/sub-<n>.jsonl`).
///
/// An entry does not name its subagents' journals. A `delegate` call
/// numbers its subagents after the highest journal there is, so in number
/// order the journals run through the calls in turn: each entry (of any
/// model, to stay in step) takes the next run of journals whose usage adds
/// up to exactly its own. An entry without one (a journal that is missing
/// or was cut short) stays a sum priced at the base rates, which the
/// report says. Journals no entry takes (a run that ended before its
/// parent recorded the entry) are not counted, as the entries never
/// counted them.
fn split_subagents(path: &Path, records: Vec<Record>, prices: &Prices) -> Vec<Record> {
    let by_length = |model: &str| prices.get(model).is_some_and(|price| price.long.is_some());
    if !records
        .iter()
        .any(|record| record.kind == Kind::Subagents && by_length(&record.model))
    {
        return records;
    }
    let subs = sub_journals(path);
    let sums: Vec<Usage> = subs.iter().map(|(sum, _)| *sum).collect();
    let mut next = 0;
    let mut out = Vec::with_capacity(records.len());
    for record in records {
        if record.kind == Kind::Subagents
            && let Some((start, end)) = find_run(&sums[next..], record.usage)
        {
            let run = next + start..next + end;
            next += end;
            if by_length(&record.model) {
                out.extend(
                    subs[run]
                        .iter()
                        .flat_map(|(_, calls)| calls.iter().cloned()),
                );
                continue;
            }
        }
        out.push(record);
    }
    out
}

/// The usage records of the subagent journals of the session journal at
/// `path`, in number order, each with their sum, as subagent records.
fn sub_journals(path: &Path) -> Vec<(Usage, Vec<Record>)> {
    let Ok(read) = fs::read_dir(path.with_extension("")) else {
        return Vec::new();
    };
    let mut numbered: Vec<(usize, PathBuf)> = read
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let n = name
                .to_str()?
                .strip_prefix("sub-")?
                .strip_suffix(".jsonl")?
                .parse()
                .ok()?;
            Some((n, entry.path()))
        })
        .collect();
    numbered.sort();
    numbered
        .into_iter()
        .map(|(_, sub)| {
            let mut sum = Usage::default();
            let mut calls = Vec::new();
            // Their broken lines are not reported: an entry they leave
            // short keeps its sum, which counts those tokens.
            let _ = read_journal(
                &sub,
                &mut Stats::default(),
                &mut |_| true,
                &mut |_, record| {
                    sum.add(record.usage);
                    calls.push(Record {
                        kind: Kind::Subagents,
                        ..record
                    });
                },
            );
            (sum, calls)
        })
        .collect()
}

/// The first run `start..end` of `sums` that adds up to exactly `target`.
fn find_run(sums: &[Usage], target: Usage) -> Option<(usize, usize)> {
    (0..sums.len()).find_map(|start| {
        let mut total = Usage::default();
        for (end, sum) in sums.iter().enumerate().skip(start) {
            total.add(*sum);
            if total == target {
                return Some((start, end + 1));
            }
            if !within(&total, &target) {
                return None;
            }
        }
        None
    })
}

/// Whether no count of `part` is over that of `whole`.
fn within(part: &Usage, whole: &Usage) -> bool {
    part.input_tokens <= whole.input_tokens
        && part.output_tokens <= whole.output_tokens
        && part.cache_creation_input_tokens <= whole.cache_creation_input_tokens
        && part.cache_read_input_tokens <= whole.cache_read_input_tokens
        && part.web_searches() <= whole.web_searches()
}
