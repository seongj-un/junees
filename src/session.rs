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
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::message::{ContentBlock, Message, Role, Usage};

/// Result recorded for a tool call the process died in the middle of.
pub const INTERRUPTED: &str = "interrupted: Junees stopped before this tool finished";

/// The first line of every journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Unix seconds.
    pub created_at: u64,
    pub cwd: String,
    pub model: String,
    pub version: String,
}

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
            Entry::Usage(usage) => self.usage.add(usage),
        }
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

/// Appends entries to a journal file, one JSON line per entry, each written
/// with a single `write_all` on an unbuffered file.
pub struct SessionWriter {
    file: File,
}

impl SessionWriter {
    /// Starts a new session file under `home/sessions` and writes its meta
    /// line.
    pub fn create(home: &Path, cwd: &Path, model: &str) -> Result<(Meta, Self)> {
        let dir = sessions_dir(home);
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let now = unix_now();
        for _ in 0..16 {
            let id = new_id(now);
            let path = dir.join(format!("{id}.jsonl"));
            let file = match OpenOptions::new().append(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("cannot create {}", path.display()));
                }
            };
            let meta = Meta {
                id,
                created_at: now,
                cwd: cwd.to_string_lossy().into_owned(),
                model: model.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            };
            let mut writer = Self { file };
            writer
                .record(&Entry::Meta(meta.clone()))
                .with_context(|| format!("cannot write {}", path.display()))?;
            return Ok((meta, writer));
        }
        bail!("could not pick an unused session id in {}", dir.display())
    }
}

impl Journal for SessionWriter {
    fn record(&mut self, entry: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()
    }
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
    /// Reads and replays a journal. A last line that is cut off or does not
    /// parse is dropped with a warning (the process likely died while writing
    /// it); a broken line before that is an error.
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut entries = Vec::new();
        let mut warning = None;
        let mut valid_len = 0;
        let mut pos = 0;
        let mut line_no = 0;
        while pos < data.len() {
            line_no += 1;
            let (line, next) = match data[pos..].iter().position(|&b| b == b'\n') {
                Some(n) => (&data[pos..pos + n], pos + n + 1),
                None => (&data[pos..], data.len()),
            };
            if line.trim_ascii().is_empty() {
                pos = next;
                continue;
            }
            match serde_json::from_slice::<Entry>(line) {
                Ok(entry) => {
                    entries.push(entry);
                    valid_len = next;
                }
                Err(err) if data[next..].trim_ascii().is_empty() => {
                    warning = Some(format!(
                        "{}: dropped an unreadable last line ({err})",
                        path.display()
                    ));
                    break;
                }
                Err(err) => bail!("{}: line {line_no} is corrupt: {err}", path.display()),
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
    pub fn resume(path: &Path) -> Result<(Self, SessionWriter)> {
        let mut session = Self::load(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.set_len(session.valid_len)?;
        drop(file);

        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        if session.unterminated {
            file.write_all(b"\n")?;
        }
        let mut writer = SessionWriter { file };
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
    pub messages: usize,
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

/// The meta line of a journal, if its first line is one.
fn read_meta(path: &Path) -> Option<Meta> {
    use std::io::BufRead;
    let mut first = String::new();
    io::BufReader::new(File::open(path).ok()?)
        .read_line(&mut first)
        .ok()?;
    match serde_json::from_str(&first).ok()? {
        Entry::Meta(meta) => Some(meta),
        _ => None,
    }
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

/// Up to `limit` readable sessions, most recently written first.
pub fn list_recent(home: &Path, limit: usize) -> Result<Vec<Summary>> {
    Ok(session_files(home)?
        .into_iter()
        .filter_map(|file| {
            let session = Session::load(&file.path).ok()?;
            let first_input = session
                .transcript
                .messages
                .iter()
                .find(|message| message.role == Role::User)
                .map(Message::text)
                .unwrap_or_default();
            Some(Summary {
                id: session.meta.id,
                updated: file
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                cwd: session.meta.cwd,
                messages: session.transcript.messages.len(),
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
                output_tokens: 1
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
            }),
            Entry::Message(tool_call("toolu_1")),
        ];
        let mut expected = Transcript::default();
        for entry in entries {
            writer.record(&entry).unwrap();
            expected.apply(entry);
        }

        let path = sessions_dir(home.path()).join(format!("{}.jsonl", meta.id));
        let session = Session::load(&path).unwrap();
        assert_eq!(session.meta, meta);
        assert_eq!(session.meta.cwd, "/work");
        assert_eq!(session.transcript, expected);
        assert!(session.warning.is_none());
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

        let (session, _writer) = Session::resume(&path).unwrap();
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
        new_session(home, "/y", &[], 3_000);

        let list = list_recent(home, 2).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].messages, 0);
        assert_eq!(list[0].first_input, "");
        assert_eq!(list[1].first_input, "두 번째");
        assert_eq!(list[1].updated, 2_000);
        assert_eq!(list[1].cwd, "/y");
        let all = list_recent(home, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].messages, 2);
        assert_eq!(all[2].first_input, "first question");
    }
}
