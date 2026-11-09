//! The session journal: every change to the conversation is appended to a
//! JSONL file the moment it happens, so a conversation outlives the process
//! and can be resumed.
//!
//! A journal is `$JUNEES_HOME/sessions/<id>.jsonl`. The first line is a
//! [`Meta`] entry; every later line is one change. [`Transcript::apply`]
//! turns entries into history, and both the agent and [`Session::load`] go
//! through it, so a replayed journal rebuilds exactly what the agent had.

use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::message::{ContentBlock, Message, Role, Usage};

/// The journal format this version writes and the newest it can read.
/// Raise it when old binaries could misread new journals. Format 2 added
/// `clear_tool_results` and `compact`. Format 3 keeps the content blocks
/// Junees does not handle itself (thinking and the like) as the API sent
/// them, which older versions would read as placeholders the API rejects.
/// Format 1 and 2 journals still load.
pub const FORMAT: u32 = 3;

/// Result recorded for a tool call the process died in the middle of.
pub const INTERRUPTED: &str = "interrupted: Junees stopped before this tool finished";

/// What a tool result says once its output was cleared to save context.
pub const CLEARED: &str = "[tool output cleared to save context]";

/// Tool result messages whose output is never cleared: the most recent ones.
pub const KEEP_TOOL_RESULTS: usize = 3;

/// Starts the text block that carries a compacted conversation's summary.
pub const SUMMARY_HEADER: &str = "[Summary of the earlier conversation]\n";

/// User inputs a compaction keeps whole when it can.
const KEEP_INPUTS: usize = 2;

/// Tool rounds (a model reply and the results of its calls) a compaction
/// inside a single turn keeps whole when it can.
const KEEP_ROUNDS: usize = 2;

/// The first line of every journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Journal format version; files from before the field are format 1.
    #[serde(default = "first_format")]
    pub format: u32,
    /// Unix seconds.
    pub created_at: u64,
    pub cwd: String,
    pub model: String,
    pub version: String,
}

fn first_format() -> u32 {
    1
}

/// The `type` tags of [`Entry`]; keep in step with its variants.
const ENTRY_TYPES: [&str; 7] = [
    "meta",
    "message",
    "append_text",
    "usage",
    "clear_tool_results",
    "summary_usage",
    "compact",
];

/// One line of a journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Meta(Meta),
    /// A message was added to the history.
    Message(Message),
    /// A text block was added to the trailing user message.
    AppendText {
        text: String,
    },
    /// Token usage of one model call.
    Usage(Usage),
    /// The output of every tool result in the messages before index `upto`
    /// was replaced with [`CLEARED`], and every thinking block was dropped
    /// (see [`Transcript::apply`]).
    ClearToolResults {
        upto: usize,
    },
    /// Token usage of a call that summarized the conversation. It counts
    /// toward the session's usage but says nothing about the context size.
    SummaryUsage(Usage),
    /// The messages before index `keep_from` were replaced by `summary`.
    /// It leads the first kept message when that is a user input; when the
    /// tail starts with a model reply (a cut inside one turn), it goes into
    /// a new user message, followed by the text of the input that started
    /// the turn, word for word. Every kept thinking block was dropped.
    Compact {
        summary: String,
        keep_from: usize,
    },
}

/// An entry as a journal line stores it: stamped with when it was written.
#[derive(Serialize)]
struct LineOut<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    /// Unix seconds.
    ts: u64,
}

/// A journal line read back. Lines from before `ts` existed have none.
#[derive(Deserialize)]
struct LineIn {
    #[serde(flatten)]
    entry: Entry,
    #[serde(default)]
    #[allow(dead_code)] // Kept in the format for time budgets and status views.
    ts: Option<u64>,
}

/// Somewhere the agent records its history changes.
pub trait Journal: Send {
    fn record(&mut self, entry: &Entry) -> io::Result<()>;
}

/// A conversation history and the tokens it has used so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub messages: Vec<Message>,
    pub usage: Usage,
    /// Size of the conversation as of the last model call (see
    /// [`Usage::context_tokens`]); 0 until one is measured.
    pub context_tokens: u64,
}

impl Transcript {
    /// Applies one journal entry. `Meta` entries change nothing.
    pub fn apply(&mut self, entry: Entry) {
        match entry {
            Entry::Meta(_) => {}
            Entry::Message(message) => self.messages.push(message),
            Entry::AppendText { text } => match self.messages.last_mut() {
                Some(last) if last.role == Role::User => {
                    last.content.push(ContentBlock::Text { text })
                }
                _ => self.messages.push(Message::user_text(text)),
            },
            Entry::Usage(usage) => {
                self.usage.add(usage);
                self.context_tokens = usage.context_tokens();
            }
            Entry::ClearToolResults { upto } => {
                for message in self.messages.iter_mut().take(upto) {
                    for block in &mut message.content {
                        if let ContentBlock::ToolResult { content, .. } = block {
                            *content = CLEARED.to_string();
                        }
                    }
                }
                self.drop_thinking();
            }
            Entry::SummaryUsage(usage) => self.usage.add(usage),
            Entry::Compact { summary, keep_from } => {
                let dropped: Vec<Message> = self
                    .messages
                    .drain(..keep_from.min(self.messages.len()))
                    .collect();
                let block = ContentBlock::Text {
                    text: format!("{SUMMARY_HEADER}{summary}"),
                };
                match self.messages.first_mut() {
                    Some(first) if first.role == Role::User => first.content.insert(0, block),
                    _ => {
                        // The cut was inside the turn of the first message's
                        // input. That input is the task the turn works on,
                        // so it stays as typed rather than only summarized.
                        let mut content = vec![block];
                        if let Some(input) = dropped.first() {
                            content.extend(
                                input
                                    .content
                                    .iter()
                                    .filter(|block| {
                                        matches!(block, ContentBlock::Text { text }
                                            if !text.starts_with(SUMMARY_HEADER))
                                    })
                                    .cloned(),
                            );
                        }
                        self.messages.insert(
                            0,
                            Message {
                                role: Role::User,
                                content,
                            },
                        );
                    }
                }
                self.drop_thinking();
                // Unknown until the next model call measures it.
                self.context_tokens = 0;
            }
        }
    }

    /// Drops every `thinking` and `redacted_thinking` block, after a change
    /// to earlier messages. Claude 5.1 and later models check that nothing
    /// before a thinking block sent back (system prompt, tools, earlier
    /// messages) changed since the block was made, and reject it, and every
    /// later one, if it did. Without thinking blocks there is nothing to
    /// check; the ones made from here on see the changed history. Replies
    /// keep their text and tool calls, so none becomes empty: a reply of
    /// nothing but thinking is never stored.
    fn drop_thinking(&mut self) {
        for message in &mut self.messages {
            message.content.retain(|block| !block.is_thinking());
        }
    }

    /// Where a compaction should cut the history: the index of the first
    /// kept message, or `None` when there is nowhere to cut.
    ///
    /// Cuts before a user input come first: the tail starts at a user
    /// message that carries typed input and no tool results, right after a
    /// message without tool calls, so no tool call is cut off from its
    /// result. It keeps the last two inputs and everything after them when
    /// it can, else the last one.
    ///
    /// When the first message is the only input (one long turn, as in a
    /// one-shot task), the cut goes inside that turn instead: at a model
    /// reply that directly follows a message holding nothing but tool
    /// results, so every earlier tool call has its result. It keeps the
    /// last two such rounds when it can, else the last one.
    pub fn compact_point(&self) -> Option<usize> {
        let inputs: Vec<usize> = (1..self.messages.len())
            .filter(|&i| {
                let message = &self.messages[i];
                message.role == Role::User
                    && message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Text { .. }))
                    && !has_tool_result(message)
                    && !self.messages[i - 1]
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
            })
            .collect();
        if !inputs.is_empty() {
            return inputs
                .iter()
                .rev()
                .nth(KEEP_INPUTS - 1)
                .or(inputs.last())
                .copied();
        }
        let rounds: Vec<usize> = (2..self.messages.len())
            .filter(|&i| {
                self.messages[i].role == Role::Assistant && only_tool_results(&self.messages[i - 1])
            })
            .collect();
        rounds
            .iter()
            .rev()
            .nth(KEEP_ROUNDS - 1)
            .or(rounds.last())
            .copied()
    }

    /// The entry that clears the output of every tool result except those
    /// in the last [`KEEP_TOOL_RESULTS`] messages that carry tool results,
    /// or `None` when there is nothing left to clear. The blocks stay, with
    /// their `tool_use_id` and `is_error`, so every tool call keeps its
    /// result.
    pub fn clear_entry(&self) -> Option<Entry> {
        self.clear_entry_keeping(KEEP_TOOL_RESULTS)
    }

    /// Like [`Transcript::clear_entry`], but keeps the output of the last
    /// `keep` messages with tool results; 0 clears every tool result.
    pub fn clear_entry_keeping(&self, keep: usize) -> Option<Entry> {
        let upto = match keep.checked_sub(1) {
            None => self.messages.len(),
            Some(skip) => self
                .messages
                .iter()
                .enumerate()
                .filter(|(_, message)| has_tool_result(message))
                .map(|(i, _)| i)
                .rev()
                .nth(skip)?,
        };
        let uncleared = self.messages[..upto].iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, ContentBlock::ToolResult { content, .. } if content != CLEARED),
            )
        });
        uncleared.then_some(Entry::ClearToolResults { upto })
    }

    /// The entry that adds user input: it joins a trailing user message (left
    /// behind by a failed turn) or starts a new one.
    pub fn user_text_entry(&self, text: &str) -> Entry {
        match self.messages.last() {
            Some(last) if last.role == Role::User => Entry::AppendText {
                text: text.to_string(),
            },
            _ => Entry::Message(Message::user_text(text)),
        }
    }

    /// If the history ends with tool calls that never got a result (the
    /// process stopped while they ran), the user message that answers each
    /// of them with an error.
    pub fn interrupted_results(&self) -> Option<Message> {
        let last = self.messages.last()?;
        if last.role != Role::Assistant {
            return None;
        }
        let results: Vec<ContentBlock> = last
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: INTERRUPTED.to_string(),
                    is_error: true,
                }),
                _ => None,
            })
            .collect();
        (!results.is_empty()).then_some(Message {
            role: Role::User,
            content: results,
        })
    }
}

fn has_tool_result(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// A user message that answers tool calls and says nothing else.
fn only_tool_results(message: &Message) -> bool {
    message.role == Role::User
        && !message.content.is_empty()
        && message
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Appends entries to a journal file, one JSON line per entry, each written
/// with a single `write_all` on an unbuffered file.
///
/// A new session's file is only created with its first entry, so starting
/// Junees and quitting right away leaves no empty session behind for
/// `--continue` to pick.
///
/// While a writer has its file open it holds an exclusive `flock` on it, so
/// a second Junees cannot resume (and repair) a session that is still live.
pub struct SessionWriter {
    path: PathBuf,
    meta: Meta,
    file: Option<File>,
}

impl SessionWriter {
    /// Picks an id for a new session under `home/sessions`. Nothing is
    /// written until the first entry.
    pub fn create(home: &Path, cwd: &Path, model: &str) -> Result<(Meta, Self)> {
        let dir = sessions_dir(home);
        let now = unix_now();
        for _ in 0..16 {
            let id = new_id(now);
            let path = dir.join(format!("{id}.jsonl"));
            if path.exists() {
                continue;
            }
            let meta = Meta {
                id,
                format: FORMAT,
                created_at: now,
                cwd: cwd.to_string_lossy().into_owned(),
                model: model.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            };
            let writer = Self {
                path,
                meta: meta.clone(),
                file: None,
            };
            return Ok((meta, writer));
        }
        bail!("could not pick an unused session id in {}", dir.display())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates the file, locks it and writes the meta line. Journals hold
    /// every tool output (files read, command output), so the directories
    /// it creates are 0700 and the file is 0600 whatever the umask.
    fn start(&self) -> io::Result<File> {
        if let Some(dir) = self.path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.path)?;
        lock(&file, &self.path)?;
        write_line(&mut file, &Entry::Meta(self.meta.clone()))?;
        Ok(file)
    }
}

impl Journal for SessionWriter {
    fn record(&mut self, entry: &Entry) -> io::Result<()> {
        let mut file = match self.file.take() {
            Some(file) => file,
            None => self.start()?,
        };
        let result = write_line(&mut file, entry);
        self.file = Some(file);
        result
    }
}

/// Takes an exclusive `flock` on a journal without waiting. The kernel drops
/// it when the file is closed or the process dies, so a lock that is held
/// means the session's writer is still running.
fn lock(file: &File, path: &Path) -> io::Result<()> {
    // SAFETY: `file` owns the descriptor and outlives the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() != io::ErrorKind::WouldBlock {
        return Err(err);
    }
    let id = path.file_stem().unwrap_or_default().to_string_lossy();
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("session {id} is in use by another Junees"),
    ))
}

fn write_line(file: &mut File, entry: &Entry) -> io::Result<()> {
    let mut line = serde_json::to_vec(&LineOut {
        entry,
        ts: unix_now(),
    })?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.flush()
}

/// A journal read back from disk.
#[derive(Debug)]
pub struct Session {
    pub path: PathBuf,
    pub meta: Meta,
    pub transcript: Transcript,
    /// Set when a cut-off last line was dropped.
    pub warning: Option<String>,
    /// Bytes up to the end of the last good line.
    valid_len: u64,
    /// The last good line has no trailing newline.
    unterminated: bool,
}

impl Session {
    /// Reads and replays a journal. A last line that lacks its newline and
    /// does not parse is dropped with a warning: the process died while
    /// writing it. Any other line that does not parse is an error, because
    /// [`write_line`] writes a line and its newline at once, so a complete
    /// line is corrupt or from a format this version does not know.
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut entries = Vec::new();
        let mut warning = None;
        let mut valid_len = 0;
        let mut pos = 0;
        let mut line_no = 0;
        while pos < data.len() {
            line_no += 1;
            let (line, next, terminated) = match data[pos..].iter().position(|&b| b == b'\n') {
                Some(n) => (&data[pos..pos + n], pos + n + 1, true),
                None => (&data[pos..], data.len(), false),
            };
            if line.trim_ascii().is_empty() {
                pos = next;
                continue;
            }
            match serde_json::from_slice::<LineIn>(line).map(|line| line.entry) {
                Ok(entry) => {
                    if let Entry::Meta(meta) = &entry
                        && meta.format > FORMAT
                    {
                        bail!(
                            "{} was written by a newer Junees (session format {}, \
                             this version reads up to {FORMAT}); update Junees to open it",
                            path.display(),
                            meta.format
                        );
                    }
                    entries.push(entry);
                    valid_len = next;
                }
                Err(err) if !terminated => {
                    warning = Some(format!(
                        "{}: dropped an unreadable last line ({err})",
                        path.display()
                    ));
                    break;
                }
                Err(err) => match unknown_type(line) {
                    // A later Junees may have continued an older session
                    // without changing its meta line.
                    Some(kind) => bail!(
                        "{}: line {line_no} is a `{kind}` entry, which this version \
                         does not know: the session was probably continued by a newer \
                         Junees; update Junees to open it",
                        path.display()
                    ),
                    None => bail!("{}: line {line_no} is corrupt: {err}", path.display()),
                },
            }
            pos = next;
        }

        let mut entries = entries.into_iter();
        let Some(Entry::Meta(meta)) = entries.next() else {
            bail!("{} is not a Junees session (no meta line)", path.display());
        };
        let mut transcript = Transcript::default();
        for entry in entries {
            if matches!(entry, Entry::Meta(_)) {
                bail!("{}: unexpected second meta line", path.display());
            }
            transcript.apply(entry);
        }
        Ok(Self {
            path: path.to_path_buf(),
            meta,
            transcript,
            warning,
            valid_len: valid_len as u64,
            unterminated: valid_len > 0 && data[valid_len - 1] != b'\n',
        })
    }

    /// Loads a journal to continue it: drops a cut-off last line from the
    /// file, answers tool calls that never finished (and records that), and
    /// returns a writer that appends to the file.
    ///
    /// The file is locked before it is read or changed, so a session another
    /// Junees still has open is refused instead of repaired under it.
    pub fn resume(path: &Path) -> Result<(Self, SessionWriter)> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        lock(&file, path)?;
        let mut session = Self::load(path)?;
        file.set_len(session.valid_len)?;
        if session.unterminated {
            file.write_all(b"\n")?;
        }
        let mut writer = SessionWriter {
            path: path.to_path_buf(),
            meta: session.meta.clone(),
            file: Some(file),
        };
        if let Some(results) = session.transcript.interrupted_results() {
            let entry = Entry::Message(results);
            writer
                .record(&entry)
                .with_context(|| format!("cannot write {}", path.display()))?;
            session.transcript.apply(entry);
        }
        Ok((session, writer))
    }
}

/// The `type` of a JSON object line, if it is not one this version knows.
fn unknown_type(line: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let kind = value.get("type")?.as_str()?;
    (!ENTRY_TYPES.contains(&kind)).then(|| kind.to_string())
}

pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// What `/sessions` shows about one session.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub id: String,
    /// Last write, in Unix seconds.
    pub updated: u64,
    pub cwd: String,
    /// Text of the first user message.
    pub first_input: String,
}

struct SessionFile {
    path: PathBuf,
    id: String,
    modified: SystemTime,
}

/// Journal files under `home/sessions`, most recently written first.
fn session_files(home: &Path) -> Result<Vec<SessionFile>> {
    let dir = sessions_dir(home);
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in read {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let id = id.to_string();
        let Ok(modified) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
            continue;
        };
        files.push(SessionFile { path, id, modified });
    }
    files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
    Ok(files)
}

/// Reads one line of a journal as an entry.
fn read_entry(reader: &mut impl io::BufRead) -> Option<Entry> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    Some(serde_json::from_str::<LineIn>(&line).ok()?.entry)
}

/// The meta line of a journal, if its first line is one, and a reader
/// positioned after it.
fn read_head(path: &Path) -> Option<(Meta, io::BufReader<File>)> {
    let mut reader = io::BufReader::new(File::open(path).ok()?);
    match read_entry(&mut reader)? {
        Entry::Meta(meta) => Some((meta, reader)),
        _ => None,
    }
}

fn read_meta(path: &Path) -> Option<Meta> {
    read_head(path).map(|(meta, _)| meta)
}

/// The most recently written session started in `cwd`.
pub fn find_latest(home: &Path, cwd: &Path) -> Result<Option<PathBuf>> {
    let cwd = cwd.to_string_lossy();
    Ok(session_files(home)?
        .into_iter()
        .find(|file| read_meta(&file.path).is_some_and(|meta| meta.cwd == cwd))
        .map(|file| file.path))
}

/// The session whose id is `prefix`, or the only one whose id starts with it.
pub fn find_by_prefix(home: &Path, prefix: &str) -> Result<PathBuf> {
    if prefix.is_empty() {
        bail!("the session id is empty");
    }
    let mut matches: Vec<SessionFile> = session_files(home)?
        .into_iter()
        .filter(|file| file.id.starts_with(prefix))
        .collect();
    if let Some(exact) = matches.iter().position(|file| file.id == prefix) {
        return Ok(matches.swap_remove(exact).path);
    }
    match matches.len() {
        0 => bail!(
            "no session id starts with `{prefix}` in {}",
            sessions_dir(home).display()
        ),
        1 => Ok(matches.remove(0).path),
        n => {
            let mut ids: Vec<&str> = matches.iter().map(|file| file.id.as_str()).collect();
            ids.sort_unstable();
            bail!("`{prefix}` matches {n} sessions: {}", ids.join(", "))
        }
    }
}

/// Up to `limit` sessions with a readable meta line, most recently written
/// first. Only the meta line and the first entry (the first user input) of
/// each file are read, so long sessions do not slow the list down.
pub fn list_recent(home: &Path, limit: usize) -> Result<Vec<Summary>> {
    Ok(session_files(home)?
        .into_iter()
        .filter_map(|file| {
            let (meta, mut rest) = read_head(&file.path)?;
            let first_input = match read_entry(&mut rest) {
                Some(Entry::Message(message)) if message.role == Role::User => message.text(),
                _ => String::new(),
            };
            Some(Summary {
                id: meta.id,
                updated: file
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                cwd: meta.cwd,
                first_input,
            })
        })
        .take(limit)
        .collect())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A session id that sorts by creation time: `YYYYMMDD-HHMMSS-xxxx` (UTC,
/// then 16 random bits).
fn new_id(unix_secs: u64) -> String {
    let (date, time) = utc_parts(unix_secs);
    // RandomState is seeded randomly per process and differs per instance.
    let random = RandomState::new().hash_one((unix_secs, std::process::id())) as u16;
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{random:04x}",
        date.0, date.1, date.2, time.0, time.1, time.2
    )
}

/// `YYYY-MM-DD HH:MM` in UTC.
pub fn format_utc(unix_secs: u64) -> String {
    let ((y, mo, d), (h, mi, _)) = utc_parts(unix_secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339 in UTC).
pub fn format_utc_seconds(unix_secs: u64) -> String {
    let ((y, mo, d), (h, mi, s)) = utc_parts(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

type Ymd = (i64, u32, u32);
type Hms = (u32, u32, u32);

fn utc_parts(unix_secs: u64) -> (Ymd, Hms) {
    let days = (unix_secs / 86_400) as i64;
    let secs = (unix_secs % 86_400) as u32;
    (
        civil_from_days(days),
        (secs / 3600, secs / 60 % 60, secs % 60),
    )
}

/// Gregorian date of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> Ymd {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "running".into(),
                },
                ContentBlock::ToolUse {
                    id: id.into(),
                    name: "bash".into(),
                    input: json!({"command": "sleep 100"}),
                },
            ],
        }
    }

    fn write_lines(path: &Path, lines: &[String], tail: &str) {
        write_with_tail(path, lines, tail.as_bytes());
    }

    fn write_with_tail(path: &Path, lines: &[String], tail: &[u8]) {
        let mut data = lines.join("\n").into_bytes();
        data.push(b'\n');
        data.extend_from_slice(tail);
        fs::write(path, data).unwrap();
    }

    fn line(entry: &Entry) -> String {
        serde_json::to_string(entry).unwrap()
    }

    fn meta() -> Entry {
        Entry::Meta(Meta {
            id: "20261109-061704-3fa2".into(),
            format: FORMAT,
            created_at: 1_794_205_024,
            cwd: "/work".into(),
            model: "m".into(),
            version: "0.1.0".into(),
        })
    }

    #[test]
    fn entries_have_a_type_tag() {
        assert_eq!(
            serde_json::to_value(Entry::AppendText { text: "x".into() }).unwrap(),
            json!({"type": "append_text", "text": "x"})
        );
        assert_eq!(
            serde_json::to_value(Entry::Message(Message::user_text("hi"))).unwrap(),
            json!({"type": "message", "role": "user", "content": [{"type": "text", "text": "hi"}]})
        );
        assert_eq!(
            serde_json::to_value(Entry::Usage(Usage {
                input_tokens: 3,
                output_tokens: 1,
                ..Default::default()
            }))
            .unwrap(),
            json!({"type": "usage", "input_tokens": 3, "output_tokens": 1})
        );
        assert_eq!(serde_json::to_value(meta()).unwrap()["type"], "meta");
    }

    #[test]
    fn user_text_joins_a_trailing_user_message() {
        let mut transcript = Transcript::default();
        let entry = transcript.user_text_entry("first");
        assert!(matches!(entry, Entry::Message(_)));
        transcript.apply(entry);
        let entry = transcript.user_text_entry("second");
        assert!(matches!(entry, Entry::AppendText { .. }));
        transcript.apply(entry);
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text(), "first\nsecond");
    }

    #[test]
    fn writes_and_loads_a_session() {
        let home = tempfile::tempdir().unwrap();
        let (meta, mut writer) =
            SessionWriter::create(home.path(), Path::new("/work"), "m").unwrap();
        let entries = [
            Entry::Message(Message::user_text("안녕")),
            Entry::Usage(Usage {
                input_tokens: 5,
                output_tokens: 2,
                ..Default::default()
            }),
            Entry::Message(tool_call("toolu_1")),
        ];
        let mut expected = Transcript::default();
        for entry in entries {
            writer.record(&entry).unwrap();
            expected.apply(entry);
        }

        let path = sessions_dir(home.path()).join(format!("{}.jsonl", meta.id));
        assert_eq!(writer.path(), path);
        let session = Session::load(&path).unwrap();
        assert_eq!(session.meta, meta);
        assert_eq!(session.meta.cwd, "/work");
        assert_eq!(session.transcript, expected);
        assert!(session.warning.is_none());
    }

    #[test]
    fn written_lines_carry_a_timestamp() {
        // The other tests write their fixtures without `ts`, as journals
        // from before the field were, and those still load.
        let home = tempfile::tempdir().unwrap();
        let (meta, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        writer
            .record(&Entry::Message(Message::user_text("hi")))
            .unwrap();
        let data = fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(line["ts"].as_u64().unwrap() >= meta.created_at, "{line}");
        }
        assert_eq!(lines[1]["type"], "message");
        let session = Session::load(writer.path()).unwrap();
        assert_eq!(session.transcript.messages, [Message::user_text("hi")]);
    }

    #[test]
    fn a_new_session_file_appears_with_its_first_entry() {
        let home = tempfile::tempdir().unwrap();
        let (_, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        assert!(!sessions_dir(home.path()).exists());
        writer
            .record(&Entry::Message(Message::user_text("hi")))
            .unwrap();
        let session = Session::load(writer.path()).unwrap();
        assert_eq!(session.transcript.messages, [Message::user_text("hi")]);
    }

    #[test]
    fn session_files_and_their_folders_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("junees");
        let (_, mut writer) = SessionWriter::create(&home, Path::new("/w"), "m").unwrap();
        writer
            .record(&Entry::Message(Message::user_text("cat .env")))
            .unwrap();
        for path in [home.as_path(), &sessions_dir(&home), writer.path()] {
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{} has mode {mode:o}", path.display());
        }
    }

    #[test]
    fn drops_a_cut_off_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        // Cut in the middle of a Hangul character.
        let full = line(&Entry::Message(Message::user_text("한국어 문장")));
        let cut = &full.as_bytes()[..full.len() - 5];
        assert!(std::str::from_utf8(cut).is_err());
        write_with_tail(&path, &[line(&meta()), line(&user)], cut);

        let session = Session::load(&path).unwrap();
        assert_eq!(session.transcript.messages, [Message::user_text("hi")]);
        assert!(session.warning.unwrap().contains("unreadable last line"));
    }

    #[test]
    fn a_broken_middle_line_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = line(&Entry::Message(Message::user_text("hi")));
        write_lines(&path, &[line(&meta()), "{\"type\":".into(), user], "");
        let err = Session::load(&path).unwrap_err();
        assert!(err.to_string().contains("line 2 is corrupt"), "{err}");
    }

    #[test]
    fn a_complete_last_line_that_does_not_parse_is_kept_and_refused() {
        // Newline included, so no crash cut it short: it may be an entry
        // from a newer Junees, and resume must not delete it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = line(&Entry::Message(Message::user_text("hi")));
        let future = r#"{"type":"from_the_future","x":1}"#;
        write_lines(&path, &[line(&meta()), user, future.into()], "");
        let before = fs::read(&path).unwrap();

        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("line 3 is a `from_the_future` entry"), "{err}");
        assert!(err.contains("continued by a newer Junees"), "{err}");
        assert!(Session::resume(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);

        // A known entry type that does not parse is plain corrupt.
        let broken = r#"{"type":"usage","input_tokens":"many"}"#;
        write_lines(&path, &[line(&meta()), broken.into()], "");
        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("line 2 is corrupt"), "{err}");
    }

    #[test]
    fn every_entry_type_is_known() {
        let samples = [
            meta(),
            Entry::Message(Message::user_text("hi")),
            Entry::AppendText { text: "x".into() },
            Entry::Usage(Usage::default()),
            Entry::ClearToolResults { upto: 1 },
            Entry::SummaryUsage(Usage::default()),
            Entry::Compact {
                summary: "s".into(),
                keep_from: 1,
            },
        ];
        let kinds: Vec<String> = samples
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap()["type"].to_string())
            .collect();
        let known: Vec<String> = ENTRY_TYPES
            .iter()
            .map(|kind| format!("\"{kind}\""))
            .collect();
        assert_eq!(kinds, known);
    }

    #[test]
    fn meta_lines_carry_the_format_version() {
        let value = serde_json::to_value(meta()).unwrap();
        assert_eq!(value["format"], 3);
        // Journals written before the field existed are format 1.
        let old =
            r#"{"type":"meta","id":"a","created_at":1,"cwd":"/w","model":"m","version":"0.1.0"}"#;
        let Entry::Meta(old) = serde_json::from_str(old).unwrap() else {
            panic!("not a meta line");
        };
        assert_eq!(old.format, 1);
    }

    #[test]
    fn refuses_a_session_from_a_newer_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut newer = serde_json::to_value(meta()).unwrap();
        newer["format"] = json!(FORMAT + 1);
        let future = r#"{"type":"from_the_future","x":1}"#;
        write_lines(&path, &[newer.to_string(), future.into()], "");
        let before = fs::read(&path).unwrap();

        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("written by a newer Junees"), "{err}");
        assert!(err.contains("session format 4"), "{err}");
        assert!(Session::resume(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn loads_a_format_1_session() {
        // As L1 wrote them: format 1, no cache counts, no `ts`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"meta","id":"a","format":1,"created_at":1,"cwd":"/w","model":"m","version":"0.1.0"}"#,
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"hi"}]}"#,
            r#"{"type":"usage","input_tokens":30,"output_tokens":4}"#,
            r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"yo"}]}"#,
        ]
        .map(String::from);
        write_lines(&path, &lines, "");
        let (session, mut writer) = Session::resume(&path).unwrap();
        assert_eq!(session.meta.format, 1);
        assert_eq!(session.transcript.messages.len(), 2);
        assert_eq!(session.transcript.context_tokens, 34);
        // New entries are appended to it as they are.
        writer.record(&Entry::ClearToolResults { upto: 0 }).unwrap();
        drop(writer);
        assert_eq!(Session::load(&path).unwrap().transcript, session.transcript);
    }

    fn tool_results(id: &str, output: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: output.into(),
                is_error,
            }],
        }
    }

    /// `n` rounds of a tool call and its result after one user input.
    fn tool_rounds(n: usize) -> Transcript {
        let mut transcript = Transcript::default();
        transcript.apply(Entry::Message(Message::user_text("go")));
        for i in 0..n {
            let id = format!("toolu_{i}");
            transcript.apply(Entry::Message(tool_call(&id)));
            let output = format!("output {i}");
            transcript.apply(Entry::Message(tool_results(&id, &output, i == 0)));
        }
        transcript
    }

    #[test]
    fn clearing_keeps_the_last_three_tool_results() {
        assert_eq!(tool_rounds(3).clear_entry(), None);
        let mut transcript = tool_rounds(5);
        let before = transcript.clone();
        let entry = transcript.clear_entry().unwrap();
        // Results sit at 2, 4, 6, 8, 10; the last three start at 6.
        assert_eq!(entry, Entry::ClearToolResults { upto: 6 });
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"type": "clear_tool_results", "upto": 6})
        );
        transcript.apply(entry);

        assert_eq!(transcript.messages.len(), before.messages.len());
        assert_eq!(
            transcript.messages[2],
            tool_results("toolu_0", CLEARED, true)
        );
        assert_eq!(
            transcript.messages[4],
            tool_results("toolu_1", CLEARED, false)
        );
        assert_eq!(transcript.messages[6..], before.messages[6..]);
        // Everything but tool output is untouched.
        for (now, then) in transcript.messages.iter().zip(&before.messages).take(6) {
            if !has_tool_result(then) {
                assert_eq!(now, then);
            }
        }
        // Already cleared: nothing to do until another result comes in.
        assert_eq!(transcript.clear_entry(), None);
        transcript.apply(Entry::Message(tool_call("toolu_5")));
        transcript.apply(Entry::Message(tool_results("toolu_5", "more", false)));
        assert_eq!(
            transcript.clear_entry(),
            Some(Entry::ClearToolResults { upto: 8 })
        );
    }

    #[test]
    fn clearing_can_take_the_latest_tool_results_too() {
        let mut transcript = tool_rounds(2);
        let entry = transcript.clear_entry_keeping(0).unwrap();
        assert_eq!(entry, Entry::ClearToolResults { upto: 5 });
        transcript.apply(entry);
        assert_eq!(
            transcript.messages[4],
            tool_results("toolu_1", CLEARED, false)
        );
        assert_eq!(transcript.clear_entry_keeping(0), None);
        assert_eq!(Transcript::default().clear_entry_keeping(0), None);
    }

    /// A tool call after a signed thinking block and a redacted one.
    fn thinking_call(id: &str) -> Message {
        let mut message = tool_call(id);
        message.content.splice(
            0..0,
            [
                ContentBlock::Other(json!({"type": "thinking", "thinking": format!("why {id}"),
                                           "signature": format!("sig-{id}==")})),
                ContentBlock::Other(json!({"type": "redacted_thinking", "data": "opaque+/="})),
            ],
        );
        message
    }

    fn has_thinking(transcript: &Transcript) -> bool {
        transcript
            .messages
            .iter()
            .any(|message| message.content.iter().any(ContentBlock::is_thinking))
    }

    /// One input and `n` tool rounds whose replies all start with thinking,
    /// as Claude 5 models (thinking on by default) send them.
    fn thinking_rounds(n: usize) -> Vec<Message> {
        let mut messages = vec![Message::user_text("go")];
        for i in 0..n {
            let id = format!("toolu_{i}");
            messages.push(thinking_call(&id));
            messages.push(tool_results(&id, "out", false));
        }
        messages
    }

    #[test]
    fn clearing_drops_every_thinking_block() {
        let mut transcript = transcript_of(thinking_rounds(6));
        // The rules cut where they would without thinking blocks.
        let entry = transcript.clear_entry().unwrap();
        assert_eq!(Some(&entry), tool_rounds(6).clear_entry().as_ref());
        transcript.apply(entry);
        // Thinking after a cleared result would no longer match what came
        // before it, and dropping only that would change what came before
        // the rest: none stays, before the cut or after it.
        assert!(!has_thinking(&transcript), "{:#?}", transcript.messages);
        let mut plain = tool_rounds(6);
        let entry = plain.clear_entry().unwrap();
        plain.apply(entry);
        for (now, then) in transcript.messages.iter().zip(&plain.messages) {
            assert_eq!(now.role, then.role);
            assert_eq!(now.content.len(), then.content.len());
        }
        assert_eq!(transcript.messages.len(), 13);
        assert_eq!(transcript.messages[11], tool_call("toolu_5"));

        // Thinking that comes after the clear saw the cleared history and
        // stays until the next change.
        transcript.apply(Entry::Message(thinking_call("toolu_6")));
        assert_eq!(transcript.messages[13], thinking_call("toolu_6"));
    }

    #[test]
    fn compaction_drops_every_thinking_block_and_replays_the_same() {
        let messages = thinking_rounds(5);
        let home = tempfile::tempdir().unwrap();
        let (_, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        let mut transcript = Transcript::default();
        for message in messages.clone() {
            let entry = Entry::Message(message);
            writer.record(&entry).unwrap();
            transcript.apply(entry);
        }
        let keep_from = transcript.compact_point().unwrap();
        assert_eq!(Some(keep_from), tool_rounds(5).compact_point());
        let entry = Entry::Compact {
            summary: "s".into(),
            keep_from,
        };
        writer.record(&entry).unwrap();
        transcript.apply(entry);
        drop(writer);

        // The kept replies keep their tool calls, without their thinking:
        // it was made after the messages the summary replaced.
        assert!(!has_thinking(&transcript), "{:#?}", transcript.messages);
        assert_eq!(transcript.messages[1], tool_call("toolu_3"));
        assert_eq!(transcript.messages[3], tool_call("toolu_4"));
        assert_eq!(transcript.interrupted_results(), None);
        let path = sessions_dir(home.path());
        let path = fs::read_dir(path).unwrap().next().unwrap().unwrap().path();
        let (session, _) = Session::resume(&path).unwrap();
        assert_eq!(session.meta.format, FORMAT);
        assert_eq!(session.transcript, transcript);
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn transcript_of(messages: Vec<Message>) -> Transcript {
        let mut transcript = Transcript::default();
        for message in messages {
            transcript.apply(Entry::Message(message));
        }
        transcript
    }

    #[test]
    fn a_single_turn_is_cut_after_a_closed_tool_round() {
        assert_eq!(Transcript::default().compact_point(), None);
        // The first reply answers the input itself: nothing to cut yet.
        assert_eq!(tool_rounds(1).compact_point(), None);
        // Replies sit at 1, 3, 5, ...; each after the first follows a
        // message of tool results only. The last two rounds stay.
        assert_eq!(tool_rounds(2).compact_point(), Some(3));
        assert_eq!(tool_rounds(6).compact_point(), Some(9));

        // Results that carry typed input too (a failed turn) are no round
        // boundary.
        let mut with_input = tool_results("toolu_1", "out", false);
        with_input.content.push(ContentBlock::Text {
            text: "and now?".into(),
        });
        let transcript = transcript_of(vec![
            Message::user_text("first"),
            tool_call("toolu_1"),
            with_input,
            tool_call("toolu_2"),
            tool_results("toolu_2", "out", false),
            tool_call("toolu_3"),
            tool_results("toolu_3", "out", false),
        ]);
        assert_eq!(transcript.compact_point(), Some(5));
    }

    #[test]
    fn compacting_inside_a_turn_keeps_its_input_word_for_word() {
        let mut transcript = tool_rounds(4);
        let before = transcript.clone();
        let keep_from = transcript.compact_point().unwrap();
        assert_eq!(keep_from, 5);
        transcript.apply(Entry::Compact {
            summary: "did 0 to 1".into(),
            keep_from,
        });

        let head = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: format!("{SUMMARY_HEADER}did 0 to 1"),
                },
                ContentBlock::Text { text: "go".into() },
            ],
        };
        assert_eq!(transcript.messages[0], head);
        assert_eq!(transcript.messages[1..], before.messages[5..]);
        for (i, message) in transcript.messages.iter().enumerate() {
            let expected = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            assert_eq!(message.role, expected, "{:#?}", transcript.messages);
        }

        // A later cut in the same turn replaces the old summary and keeps
        // the input once.
        for i in 4..6 {
            let id = format!("toolu_{i}");
            transcript.apply(Entry::Message(tool_call(&id)));
            transcript.apply(Entry::Message(tool_results(&id, "more", false)));
        }
        let keep_from = transcript.compact_point().unwrap();
        assert_eq!(keep_from, 5);
        transcript.apply(Entry::Compact {
            summary: "did 0 to 3".into(),
            keep_from,
        });
        assert_eq!(
            transcript.messages[0].content,
            [
                ContentBlock::Text {
                    text: format!("{SUMMARY_HEADER}did 0 to 3"),
                },
                ContentBlock::Text { text: "go".into() },
            ]
        );
        assert_eq!(transcript.messages[1], tool_call("toolu_4"));
        assert_eq!(transcript.messages.len(), 5);
    }

    #[test]
    fn compaction_keeps_the_last_two_inputs_when_it_can() {
        let mut transcript = tool_rounds(2); // input at 0, results at 2 and 4
        transcript.apply(Entry::Message(assistant("done")));
        transcript.apply(Entry::Message(Message::user_text("second")));
        assert_eq!(transcript.compact_point(), Some(6));
        transcript.apply(Entry::Message(assistant("ok")));
        transcript.apply(Entry::Message(Message::user_text("third")));
        assert_eq!(transcript.compact_point(), Some(6));
        transcript.apply(Entry::Message(tool_call("toolu_8")));
        transcript.apply(Entry::Message(tool_results("toolu_8", "x", false)));
        transcript.apply(Entry::Message(assistant("ok")));
        transcript.apply(Entry::Message(Message::user_text("fourth")));
        assert_eq!(transcript.compact_point(), Some(8));
    }

    #[test]
    fn compaction_never_starts_the_tail_at_a_tool_result() {
        // Input typed after a failed turn joins the tool results; that
        // message cannot lead the tail, since its calls are before it.
        let mut with_results = tool_results("toolu_1", "out", false);
        with_results.content.push(ContentBlock::Text {
            text: "and now?".into(),
        });
        let transcript = transcript_of(vec![
            Message::user_text("first"),
            tool_call("toolu_1"),
            with_results,
            assistant("hm"),
            Message::user_text("second"),
            tool_call("toolu_2"),
            tool_results("toolu_2", "out", false),
        ]);
        assert_eq!(transcript.compact_point(), Some(4));

        // Nor right after a message with tool calls, should one show up.
        let odd = transcript_of(vec![
            Message::user_text("first"),
            tool_call("toolu_1"),
            Message::user_text("no result"),
        ]);
        assert_eq!(odd.compact_point(), None);
        // A message with tool results never leads, even after one without
        // calls.
        let mut orphan = tool_results("toolu_7", "out", false);
        orphan
            .content
            .push(ContentBlock::Text { text: "hi".into() });
        let odd = transcript_of(vec![Message::user_text("first"), assistant("hm"), orphan]);
        assert_eq!(odd.compact_point(), None);
    }

    #[test]
    fn compact_puts_the_summary_in_front_of_the_kept_input() {
        let mut transcript = tool_rounds(1);
        transcript.apply(Entry::Message(assistant("done")));
        transcript.apply(Entry::Message(Message::user_text("second")));
        transcript.apply(Entry::Message(tool_call("toolu_9")));
        transcript.apply(Entry::Message(tool_results("toolu_9", "x", false)));
        transcript.apply(Entry::Usage(Usage {
            input_tokens: 900,
            ..Default::default()
        }));
        let keep_from = transcript.compact_point().unwrap();
        assert_eq!(keep_from, 4);
        let tail = transcript.messages[4..].to_vec();
        let entry = Entry::Compact {
            summary: "did things".into(),
            keep_from,
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"type": "compact", "summary": "did things", "keep_from": 4})
        );
        transcript.apply(entry);

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].role, Role::User);
        assert_eq!(
            transcript.messages[0].content,
            [
                ContentBlock::Text {
                    text: "[Summary of the earlier conversation]\ndid things".into()
                },
                ContentBlock::Text {
                    text: "second".into()
                },
            ]
        );
        assert_eq!(transcript.messages[1..], tail[1..]);
        assert_eq!(transcript.context_tokens, 0);
        assert_eq!(transcript.usage.input_tokens, 900);
    }

    #[test]
    fn summary_usage_counts_but_does_not_size_the_context() {
        let mut transcript = Transcript::default();
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Default::default()
        };
        transcript.apply(Entry::Usage(usage));
        transcript.apply(Entry::SummaryUsage(Usage {
            input_tokens: 500,
            ..Default::default()
        }));
        assert_eq!(transcript.usage.input_tokens, 510);
        assert_eq!(transcript.context_tokens, 12);
        assert_eq!(
            serde_json::to_value(Entry::SummaryUsage(usage)).unwrap()["type"],
            "summary_usage"
        );
    }

    #[test]
    fn needs_a_meta_line_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write_lines(
            &path,
            &[line(&Entry::Message(Message::user_text("hi")))],
            "",
        );
        assert!(Session::load(&path).is_err());
        fs::write(&path, "").unwrap();
        assert!(Session::load(&path).is_err());
    }

    #[test]
    fn resume_cuts_the_broken_tail_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        write_lines(&path, &[line(&meta()), line(&user)], "{\"type\":\"mess");

        let (session, mut writer) = Session::resume(&path).unwrap();
        assert!(session.warning.is_some());
        writer
            .record(&Entry::AppendText {
                text: "more".into(),
            })
            .unwrap();

        let reloaded = Session::load(&path).unwrap();
        assert!(reloaded.warning.is_none());
        assert_eq!(reloaded.transcript.messages[0].text(), "hi\nmore");
    }

    #[test]
    fn resume_adds_a_newline_after_an_unterminated_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        fs::write(&path, format!("{}\n{}", line(&meta()), line(&user))).unwrap();

        let (_, mut writer) = Session::resume(&path).unwrap();
        writer
            .record(&Entry::AppendText {
                text: "more".into(),
            })
            .unwrap();
        let reloaded = Session::load(&path).unwrap();
        assert_eq!(reloaded.transcript.messages[0].text(), "hi\nmore");
    }

    #[test]
    fn resume_answers_unfinished_tool_calls_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            line(&meta()),
            line(&Entry::Message(Message::user_text("sleep"))),
            line(&Entry::Message(tool_call("toolu_9"))),
        ];
        write_lines(&path, &lines, "");

        let (session, writer) = Session::resume(&path).unwrap();
        drop(writer);
        let expected = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_9".into(),
                content: INTERRUPTED.into(),
                is_error: true,
            }],
        };
        assert_eq!(session.transcript.messages.len(), 3);
        assert_eq!(session.transcript.messages[2], expected);

        // The fix is in the file too, so it is not added twice.
        let (again, _writer) = Session::resume(&path).unwrap();
        assert_eq!(again.transcript, session.transcript);
    }

    #[test]
    fn resume_refuses_a_session_another_writer_holds() {
        let home = tempfile::tempdir().unwrap();
        let (meta, mut live) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        live.record(&Entry::Message(Message::user_text("build it")))
            .unwrap();
        live.record(&Entry::Message(tool_call("toolu_1"))).unwrap();
        let path = live.path().to_path_buf();
        let before = fs::read(&path).unwrap();

        let err = Session::resume(&path).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            format!("session {} is in use by another Junees", meta.id)
        );
        assert_eq!(fs::read(&path).unwrap(), before);

        // The lock goes with the writer; then the tool call counts as cut off.
        drop(live);
        let (session, _writer) = Session::resume(&path).unwrap();
        assert_eq!(session.transcript.messages.len(), 3);
    }

    #[test]
    fn a_resumed_session_is_locked_until_its_writer_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        write_lines(&path, &[line(&meta()), line(&user)], "");

        let (_, first) = Session::resume(&path).unwrap();
        let err = Session::resume(&path).err().unwrap();
        assert!(
            format!("{err:#}").contains("in use by another Junees"),
            "{err:#}"
        );
        drop(first);
        assert!(Session::resume(&path).is_ok());
    }

    #[test]
    fn no_fix_needed_after_answered_or_plain_turns() {
        let mut transcript = Transcript::default();
        assert!(transcript.interrupted_results().is_none());
        transcript.apply(Entry::Message(Message::user_text("hi")));
        assert!(transcript.interrupted_results().is_none());
        transcript.apply(Entry::Message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "yo".into() }],
        }));
        assert!(transcript.interrupted_results().is_none());
    }

    #[test]
    fn ids_sort_by_time() {
        let id = new_id(1_794_205_024);
        assert!(id.starts_with("20261109-061704-"), "{id}");
        assert_eq!(id.len(), "20261109-061704-3fa2".len());
        assert!(new_id(1_709_251_199) < new_id(1_794_205_024));
    }

    #[test]
    fn formats_utc_dates() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(1_709_251_199), "2024-02-29 23:59");
        assert_eq!(format_utc(951_868_800), "2000-03-01 00:00");
        assert_eq!(format_utc(1_794_205_024), "2026-11-09 06:17");
        assert_eq!(format_utc_seconds(1_794_205_024), "2026-11-09T06:17:04Z");
    }

    fn set_mtime(path: &Path, unix_secs: u64) {
        let time = UNIX_EPOCH + std::time::Duration::from_secs(unix_secs);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn new_session(home: &Path, cwd: &str, inputs: &[&str], mtime: u64) -> PathBuf {
        let (meta, mut writer) = SessionWriter::create(home, Path::new(cwd), "m").unwrap();
        for input in inputs {
            writer
                .record(&Entry::Message(Message::user_text(*input)))
                .unwrap();
        }
        let path = sessions_dir(home).join(format!("{}.jsonl", meta.id));
        set_mtime(&path, mtime);
        path
    }

    #[test]
    fn continue_picks_the_latest_session_of_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!(find_latest(home, Path::new("/x")).unwrap(), None);

        let _old = new_session(home, "/x", &["old"], 1_000);
        let newer = new_session(home, "/x", &["newer"], 2_000);
        let _other = new_session(home, "/y", &["other"], 3_000);
        let junk = sessions_dir(home).join("zzzz.jsonl");
        fs::write(&junk, "not json\n").unwrap();
        set_mtime(&junk, 4_000);
        fs::write(sessions_dir(home).join("notes.txt"), "").unwrap();

        assert_eq!(find_latest(home, Path::new("/x")).unwrap(), Some(newer));
        assert_eq!(find_latest(home, Path::new("/z")).unwrap(), None);
    }

    #[test]
    fn resume_accepts_a_unique_id_prefix() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let dir = sessions_dir(home);
        fs::create_dir_all(&dir).unwrap();
        for id in [
            "20261109-061704-3fa2",
            "20261109-061704-9bcd",
            "20261110-080000-0001",
            "20261110-080000-00012",
        ] {
            fs::write(dir.join(format!("{id}.jsonl")), "").unwrap();
        }
        let found = |prefix: &str| {
            find_by_prefix(home, prefix)
                .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        };

        assert_eq!(found("20261109-061704-3").unwrap(), "20261109-061704-3fa2");
        // A full id wins even when a longer id starts with it.
        assert_eq!(
            found("20261110-080000-0001").unwrap(),
            "20261110-080000-0001"
        );
        let err = found("20261109").unwrap_err().to_string();
        assert!(err.contains("matches 2 sessions"), "{err}");
        assert!(found("2025").is_err());
        assert!(found("").is_err());
    }

    #[test]
    fn lists_recent_sessions() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert!(list_recent(home, 10).unwrap().is_empty());
        new_session(home, "/x", &["first question", "and more"], 1_000);
        new_session(home, "/y", &["두 번째"], 2_000);
        new_session(home, "/y", &["third"], 3_000);

        let list = list_recent(home, 2).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].first_input, "third");
        assert_eq!(list[1].first_input, "두 번째");
        assert_eq!(list[1].updated, 2_000);
        assert_eq!(list[1].cwd, "/y");
        let all = list_recent(home, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].first_input, "first question");
    }

    #[test]
    fn listing_reads_only_the_head_of_each_session() {
        let home = tempfile::tempdir().unwrap();
        let path = new_session(home.path(), "/x", &["first", "second"], 1_000);
        // A broken later line would stop a replay, but the list never
        // reads that far.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"not json\n").unwrap();
        drop(file);
        assert!(Session::load(&path).is_err());

        let list = list_recent(home.path(), 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].first_input, "first");
    }
}
