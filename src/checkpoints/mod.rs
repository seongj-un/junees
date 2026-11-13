//! File checkpoints: what `write_file` and `edit_file` changed, turn by
//! turn, so that a turn's file changes can be undone without git
//! (`/undo`, `junees rewind`, the dashboard's undo API).
//!
//! - Where: `$JUNEES_HOME/checkpoints/<session id>/<turn>/`, a
//!   `manifest.json` and one content file per recorded file; folders 0700,
//!   files 0600. `<session id>/turns` counts the session's turns, so turn
//!   numbers go on across resumes; `<session id>/notes` holds the lines an
//!   undo leaves for the conversation's next input.
//! - What: before a file tool changes a file, its content and permission
//!   bits, or that it did not exist ([`Before`]); the first change of a
//!   turn only. After each change, the SHA-256 of what is there now. A file
//!   over [`MAX_FILE_BYTES`] is noted as too large and cannot be restored.
//!   bash's changes are not recorded.
//! - Undo ([`SessionStore::undo`]): newest turn first; a file whose content
//!   is no longer what the turn left is skipped unless forced (someone or
//!   bash changed it since); a file the turn created is removed only when
//!   it is still what the turn left; links are never followed; every write
//!   goes through the file tools' [`FileAccess`] rules. An undo is a turn
//!   of its own, so it can be undone too.
//! - Limits: [`SESSION_BYTES`] per session and [`TOTAL_BYTES`] in all, the
//!   oldest turns removed first; turns older than [`MAX_AGE`] are removed
//!   when Junees starts.
//!
//! A turn is recorded when [`configure`] set a store (the binary does,
//! unless `checkpoints = false`) and the agent has a session journal: the
//! agent opens a [`Turn`] for each input ([`begin`]) and the file tools find
//! it with [`current`].

pub mod cli;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use crate::tools::{FileAccess, Refusal};
use serde::{Deserialize, Serialize};

/// A file larger than this is not recorded.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Recorded content kept per session; the oldest turns go first.
pub const SESSION_BYTES: u64 = 200 * 1024 * 1024;
/// Recorded content kept in all.
pub const TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
/// Turns older than this are removed when Junees starts.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The setting that turns recording off (`checkpoints = false`).
pub const ENV: &str = "JUNEES_CHECKPOINTS";
/// The folder under `$JUNEES_HOME`. The model may not write it (it is one
/// of the tools' data folders: a forged manifest would be written back by
/// a person's undo) and a webhook run may not read it (it holds the old
/// content of files, secrets included).
pub const FOLDER: &str = "checkpoints";

const MANIFEST: &str = "manifest.json";
const TURNS: &str = "turns";
const NOTES: &str = "notes";

/// All checkpoints, under `$JUNEES_HOME/checkpoints`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

static STORE: OnceLock<Option<Store>> = OnceLock::new();

/// Sets the process's store from the settings (`lookup`): none without a
/// state folder or with `checkpoints` off. Turns past [`MAX_AGE`] are
/// removed now. Later calls change nothing.
pub fn configure(lookup: impl Fn(&str) -> Option<String>) {
    let store = STORE.get_or_init(|| {
        if !enabled(lookup(ENV).as_deref()) {
            return None;
        }
        let home = crate::config_file::home_dir(&lookup)?;
        Some(Store::new(&home))
    });
    if let Some(store) = store
        && let Err(err) = store.remove_older_than(MAX_AGE)
    {
        eprintln!("  !! cannot clean old file checkpoints: {err}");
    }
}

/// Whether the setting's value leaves recording on: anything but `0`,
/// `false`, `off` or `no`.
pub fn enabled(value: Option<&str>) -> bool {
    !matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// The process's store, if [`configure`] set one.
pub fn store() -> Option<&'static Store> {
    STORE.get().and_then(Option::as_ref)
}

tokio::task_local! {
    static CURRENT: Arc<Turn>;
}

/// The turn the running agent records to, if any.
pub fn current() -> Option<Arc<Turn>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// Runs `future` with `turn` as [`current`] (nothing changes for `None`).
pub async fn scope<F: std::future::Future>(turn: Option<Arc<Turn>>, future: F) -> F::Output {
    match turn {
        Some(turn) => CURRENT.scope(turn, future).await,
        None => future.await,
    }
}

/// A new turn of the session whose journal is `journal`, when recording
/// is on and no turn is open already (a subagent records to its
/// parent's). It takes a number at its first recorded file, so turns
/// that change no file leave nothing on disk.
pub fn begin(journal: Option<&Path>) -> Option<Arc<Turn>> {
    if current().is_some() {
        return None;
    }
    let store = store()?;
    let session = journal?.file_stem()?.to_str()?;
    Some(Arc::new(Turn::new(store.session(session)?)))
}

/// The SHA-256 of `bytes`, in hex.
pub fn sha256(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn mkdir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    // An existing folder keeps its bits otherwise.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Writes `bytes` to `path` (0600) through a new temporary file next to
/// it (made with `O_EXCL | O_NOFOLLOW`, so a planted file or link there is
/// never written through) and a rename.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

fn now_text() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::session::format_utc_seconds(secs)
}

/// What a file was before a turn changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Before {
    /// It did not exist.
    Absent,
    /// Its content and permission bits.
    Content(Vec<u8>, u32),
    /// It was larger than [`MAX_FILE_BYTES`]: not recorded.
    TooLarge(u64),
}

impl Before {
    /// Reads what is at `path` now, without following a link: absent, a
    /// regular file's content, or too large. Anything else is an error.
    pub fn read(path: &Path) -> io::Result<Self> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Before::Absent),
            Err(err) => return Err(err),
        };
        if !meta.file_type().is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Ok(Before::TooLarge(meta.len()));
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Ok(Before::TooLarge(bytes.len() as u64));
        }
        Ok(Before::Content(bytes, meta.permissions().mode() & 0o7777))
    }

    /// From an opened regular file's content and metadata.
    pub fn from_content(bytes: &[u8], meta: &fs::Metadata) -> Self {
        if bytes.len() as u64 > MAX_FILE_BYTES {
            Before::TooLarge(bytes.len() as u64)
        } else {
            Before::Content(bytes.to_vec(), meta.permissions().mode() & 0o7777)
        }
    }
}

/// One file of a turn, as `manifest.json` keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recorded {
    /// The absolute path written.
    pub path: String,
    /// `absent`, `content` or `too_large`.
    pub before: String,
    /// The content file, for `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The permission bits, for `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// The size, for `too_large`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// SHA-256 of what was there before the turn, for `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha256: Option<String>,
    /// SHA-256 of what the turn left; `None`: the turn removed it.
    pub after_sha256: Option<String>,
    pub at: String,
    /// Whether undoing from this turn puts it back without `force`, as
    /// [`SessionStore::list`] works it out (not stored).
    #[serde(skip)]
    pub restorable: bool,
}

/// How a turn changed a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Created,
    Modified,
    Deleted,
}

impl Change {
    pub fn word(self) -> &'static str {
        match self {
            Change::Created => "created",
            Change::Modified => "modified",
            Change::Deleted => "deleted",
        }
    }
}

impl Recorded {
    pub fn change(&self) -> Change {
        match (self.before.as_str(), &self.after_sha256) {
            ("absent", _) => Change::Created,
            (_, None) => Change::Deleted,
            _ => Change::Modified,
        }
    }

    /// Whether an undo of this turn alone could restore it without
    /// `force`: its old state is known and the file is still what the
    /// turn left.
    fn restorable_now(&self) -> bool {
        self.before != "too_large"
            && matches!(state(Path::new(&self.path)), Ok(now) if now == self.after_sha256)
    }
}

/// What is at `path`: `None` when nothing, else the content's SHA-256.
/// A link or anything but a regular file is an error.
fn state(path: &Path) -> io::Result<Option<String>> {
    match Before::read(path)? {
        Before::Absent => Ok(None),
        Before::Content(bytes, _) => Ok(Some(sha256(&bytes))),
        Before::TooLarge(_) => {
            // Hash a large file in pieces, still without following links.
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Ok(Some(hex(hasher.finish().as_ref())))
        }
    }
}

/// A turn's `manifest.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub turn: u64,
    pub created_at: String,
    /// Set when the turn was an undo of these turns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undid: Vec<u64>,
    pub files: Vec<Recorded>,
}

/// The checkpoints of one session.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
    id: String,
}

impl Store {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(FOLDER),
        }
    }

    /// Session `id`'s checkpoints; `None` for an id that is not a session
    /// id (letters, digits, `_` and `-`).
    pub fn session(&self, id: &str) -> Option<SessionStore> {
        let valid = (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        valid.then(|| SessionStore {
            dir: self.root.join(id),
            id: id.to_string(),
        })
    }

    /// Every turn folder of every session.
    fn all_turns(&self) -> io::Result<Vec<TurnDir>> {
        let mut found = Vec::new();
        let sessions = match fs::read_dir(&self.root) {
            Ok(sessions) => sessions,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(found),
            Err(err) => return Err(err),
        };
        for session in sessions.flatten() {
            let session_dir = session.path();
            if !session.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            for (_, dir) in turn_dirs(&session_dir)? {
                let (bytes, modified) = folder_size(&dir)?;
                found.push(TurnDir {
                    session: session_dir.clone(),
                    dir,
                    bytes,
                    modified,
                });
            }
        }
        Ok(found)
    }

    /// Removes turns last written more than `age` ago, and session folders
    /// left with nothing in them but their counter and notes.
    pub fn remove_older_than(&self, age: Duration) -> io::Result<()> {
        let now = SystemTime::now();
        for TurnDir {
            session,
            dir,
            modified,
            ..
        } in self.all_turns()?
        {
            if now.duration_since(modified).unwrap_or_default() > age {
                fs::remove_dir_all(&dir)?;
                if turn_dirs(&session)?.is_empty() {
                    let counter =
                        fs::metadata(session.join(TURNS)).and_then(|meta| meta.modified());
                    if counter.is_ok_and(|at| now.duration_since(at).unwrap_or_default() > age) {
                        fs::remove_dir_all(&session)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Removes the oldest turns of all sessions until the rest hold at
    /// most `limit` bytes; `keep` is not removed (the turn being written).
    pub fn limit_total(&self, limit: u64, keep: &Path) -> io::Result<()> {
        let mut turns = self.all_turns()?;
        let mut total: u64 = turns.iter().map(|turn| turn.bytes).sum();
        turns.sort_by_key(|turn| turn.modified);
        for TurnDir { dir, bytes, .. } in turns {
            if total <= limit {
                break;
            }
            if dir == keep {
                continue;
            }
            fs::remove_dir_all(&dir)?;
            total = total.saturating_sub(bytes);
        }
        Ok(())
    }
}

/// One turn folder found on disk.
struct TurnDir {
    session: PathBuf,
    dir: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

/// The numbered turn folders of a session folder.
fn turn_dirs(session: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut turns = Vec::new();
    let entries = match fs::read_dir(session) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(turns),
        Err(err) => return Err(err),
    };
    for entry in entries.flatten() {
        let Some(turn) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        else {
            continue;
        };
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            turns.push((turn, entry.path()));
        }
    }
    turns.sort_by_key(|(turn, _)| *turn);
    Ok(turns)
}

/// The bytes of the files in `dir` and when it was last written.
fn folder_size(dir: &Path) -> io::Result<(u64, SystemTime)> {
    let mut bytes = 0;
    let mut modified = fs::metadata(dir)?.modified()?;
    for entry in fs::read_dir(dir)?.flatten() {
        let meta = entry.metadata()?;
        bytes += meta.len();
        if let Ok(at) = meta.modified() {
            modified = modified.max(at);
        }
    }
    Ok((bytes, modified))
}

/// What an undo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Undone {
    /// The turns undone, newest first.
    pub turns: Vec<u64>,
    /// Every file the undo changed, once each: the ones in [`Undone::partial`]
    /// too.
    pub restored: Vec<String>,
    /// `(path, reason)`: files left as they were, once each (the first
    /// reason).
    pub skipped: Vec<(String, String)>,
    /// Files put back through some turns and then skipped at an older one:
    /// changed, but not to before every undone turn.
    pub partial: Vec<Partial>,
    /// The turn that recorded the undo, when it changed anything.
    pub checkpoint: Option<u64>,
}

/// One file's outcome at each undone turn, newest first: `Err` holds why
/// it was skipped.
type Outcomes = Vec<(u64, Result<(), String>)>;

/// A file an undo put back only part of the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    pub path: String,
    /// The oldest turn whose change to it was undone: it is now as it was
    /// before that turn.
    pub until_turn: u64,
    /// The newest turn whose change was skipped, and why.
    pub skipped_turn: u64,
    pub reason: String,
}

impl Partial {
    /// "변경 기록 4까지 되돌림, 변경 기록 3은 건너뜀: <이유>". The numbers
    /// are checkpoint numbers, not the conversation's turns, so the line
    /// does not call them turns.
    pub fn text(&self) -> String {
        format!(
            "변경 기록 {}까지 되돌림, 변경 기록 {} 건너뜀: {}",
            self.until_turn,
            crate::korean::topic(&self.skipped_turn.to_string()),
            self.reason
        )
    }
}

impl Undone {
    /// The line left for the conversation, in the words of the spec.
    pub fn note(&self) -> String {
        let turns = self
            .turns
            .iter()
            .rev()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut note = if self.restored.is_empty() {
            format!("[되돌림] 변경 기록 {turns}의 파일 변경을 되돌리지 못했습니다")
        } else {
            let restored = self
                .restored
                .iter()
                .map(
                    |path| match self.partial.iter().find(|part| part.path == *path) {
                        Some(part) => format!("{path} ({})", part.text()),
                        None => path.clone(),
                    },
                )
                .collect::<Vec<_>>()
                .join(", ");
            format!("[되돌림] 변경 기록 {turns}의 파일 변경을 되돌렸습니다: {restored}")
        };
        if !self.skipped.is_empty() {
            let skipped = self
                .skipped
                .iter()
                .map(|(path, reason)| format!("{path}, {reason}"))
                .collect::<Vec<_>>()
                .join("; ");
            note.push_str(&format!(" (건너뜀: {skipped})"));
        }
        note.push_str(". bash로 바꾼 파일은 기록되지 않아 되돌리지 않았습니다.");
        note
    }
}

/// Why an undo did not touch a file.
pub mod reason {
    pub const CHANGED: &str = "바뀐 뒤 수정됨";
    pub const TOO_LARGE: &str = "너무 커서 기록하지 않음";
    pub const LINK: &str = "심볼릭 링크로 바뀜";
    pub const NOT_FILE: &str = "일반 파일이 아님";
    pub const REFUSED: &str = "쓰기 정책이 막음";
    pub const KEPT_CREATED: &str = "턴이 만든 파일이 그 뒤 수정되어 지우지 않음";
    pub const WRITE_FAILED: &str = "쓰지 못함";
    pub const BAD_RECORD: &str = "기록이 올바르지 않음";
}

impl SessionStore {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The next turn number of the session, counted in its `turns` file.
    pub fn next_turn(&self) -> io::Result<u64> {
        mkdir(&self.dir)?;
        let path = self.dir.join(TURNS);
        let last = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0);
        // Never below a turn folder that is there (a lost counter).
        let last = turn_dirs(&self.dir)?
            .last()
            .map_or(last, |(turn, _)| last.max(*turn));
        write_private(&path, format!("{}\n", last + 1).as_bytes())?;
        Ok(last + 1)
    }

    /// The turns with recorded files, oldest first, each file marked
    /// [`Recorded::restorable`]: an undo from that turn undoes the later
    /// turns first, so a file a later turn changed again is restorable when
    /// that later turn's change is.
    pub fn list(&self) -> io::Result<Vec<Manifest>> {
        let mut manifests = self.read_manifests()?;
        // Per path, the nearest later turn's (restorable, what it found).
        let mut later: HashMap<String, (bool, Option<String>)> = HashMap::new();
        for manifest in manifests.iter_mut().rev() {
            for file in manifest.files.iter_mut().rev() {
                file.restorable = file.before != "too_large"
                    && match later.get(&file.path) {
                        // Undone first, that turn leaves what it found;
                        // this one goes back only if that is what it left
                        // (nothing, bash included, changed it between).
                        Some((chain, found)) => *chain && *found == file.after_sha256,
                        None => file.restorable_now(),
                    };
                let found = match file.before.as_str() {
                    "absent" => None,
                    // Unknown (a manifest of an older build): never equal.
                    _ => Some(file.before_sha256.clone().unwrap_or_default()),
                };
                later.insert(file.path.clone(), (file.restorable, found));
            }
        }
        Ok(manifests)
    }

    fn read_manifests(&self) -> io::Result<Vec<Manifest>> {
        let mut manifests = Vec::new();
        for (_, dir) in turn_dirs(&self.dir)? {
            if let Ok(text) = fs::read_to_string(dir.join(MANIFEST))
                && let Ok(manifest) = serde_json::from_str::<Manifest>(&text)
                && !manifest.files.is_empty()
            {
                manifests.push(manifest);
            }
        }
        Ok(manifests)
    }

    /// Leaves `line` for the conversation's next input.
    pub fn add_note(&self, line: &str) -> io::Result<()> {
        mkdir(&self.dir)?;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.dir.join(NOTES))?;
        file.write_all(format!("{}\n", line.replace('\n', " ")).as_bytes())
    }

    /// The lines left by undos, removed.
    pub fn take_notes(&self) -> Vec<String> {
        let path = self.dir.join(NOTES);
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let _ = fs::remove_file(&path);
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Undoes turn `from` and every later one, newest first, writing
    /// through `access`. Without `force` a file no longer as the turn left
    /// it is skipped.
    pub fn undo(&self, from: u64, force: bool, access: &FileAccess) -> io::Result<Undone> {
        let mut turns: Vec<Manifest> = self
            .list()?
            .into_iter()
            .filter(|manifest| manifest.turn >= from)
            .collect();
        turns.reverse();
        let mut undone = Undone {
            turns: turns.iter().map(|manifest| manifest.turn).collect(),
            ..Undone::default()
        };
        let recorder = Turn::new(self.clone());
        recorder.set_undid(undone.turns.clone());
        // Per file, in the order first met: (turn, outcome), newest first.
        let mut outcomes: Vec<(String, Outcomes)> = Vec::new();
        for manifest in &turns {
            let dir = self.dir.join(manifest.turn.to_string());
            for file in manifest.files.iter().rev() {
                let outcome = self.restore_one(&dir, file, force, access, &recorder);
                match outcomes.iter_mut().find(|(path, _)| *path == file.path) {
                    Some((_, list)) => list.push((manifest.turn, outcome)),
                    None => outcomes.push((file.path.clone(), vec![(manifest.turn, outcome)])),
                }
            }
        }
        undone.checkpoint = recorder.number();
        recorder.finish();
        // One entry per file: changed at all (restored, and partial when a
        // turn was skipped), or left as it was (skipped, first reason).
        for (path, list) in outcomes {
            let until = list
                .iter()
                .filter(|(_, outcome)| outcome.is_ok())
                .map(|(turn, _)| *turn)
                .min();
            let skip = list.iter().find_map(|(turn, outcome)| match outcome {
                Err(reason) => Some((*turn, reason.clone())),
                Ok(()) => None,
            });
            match (until, skip) {
                (Some(_), None) => undone.restored.push(path),
                (Some(until_turn), Some((skipped_turn, reason))) => {
                    undone.restored.push(path.clone());
                    undone.partial.push(Partial {
                        path,
                        until_turn,
                        skipped_turn,
                        reason,
                    });
                }
                (None, Some((_, reason))) => undone.skipped.push((path, reason)),
                (None, None) => {}
            }
        }
        Ok(undone)
    }

    /// Puts one file back as it was before the turn; `Err` says why not.
    fn restore_one(
        &self,
        dir: &Path,
        file: &Recorded,
        force: bool,
        access: &FileAccess,
        recorder: &Turn,
    ) -> Result<(), String> {
        let path = Path::new(&file.path);
        // Only the absolute, plain path a file tool wrote; it passes the
        // write rules below again, as now configured.
        let plain = path.is_absolute()
            && path.components().all(|part| {
                matches!(
                    part,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            });
        if !plain {
            return Err(reason::BAD_RECORD.into());
        }
        if file.before == "too_large" {
            return Err(reason::TOO_LARGE.into());
        }
        let link = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
        if link {
            return Err(reason::LINK.into());
        }
        let now = match state(path) {
            Ok(now) => now,
            Err(_) => return Err(reason::NOT_FILE.into()),
        };
        let unchanged = now == file.after_sha256;
        let before = Before::read(path).map_err(|_| reason::NOT_FILE.to_string())?;
        match file.before.as_str() {
            "absent" => {
                // A file the turn made goes only while it is what the turn
                // left: even forced, someone's later work is not removed.
                if now.is_none() {
                    return Ok(());
                }
                if !unchanged {
                    return Err(reason::KEPT_CREATED.into());
                }
                let target = access
                    .open_write(&file.path, false)
                    .map_err(|refusal| refused(&refusal))?;
                recorder.before(target.real(), before);
                let removed = target.remove();
                recorder.after_disk(target.real());
                removed.map_err(|_| reason::WRITE_FAILED.to_string())
            }
            _ => {
                if !unchanged && !force {
                    return Err(reason::CHANGED.into());
                }
                let name = file.content.as_deref().unwrap_or_default();
                let valid = name.strip_suffix(".bin").is_some_and(|stem| {
                    !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit())
                });
                if !valid {
                    return Err(reason::BAD_RECORD.into());
                }
                let bytes = Before::read(&dir.join(name))
                    .ok()
                    .and_then(|before| match before {
                        Before::Content(bytes, _) => Some(bytes),
                        _ => None,
                    })
                    .ok_or_else(|| reason::BAD_RECORD.to_string())?;
                let target = access
                    .open_write(&file.path, true)
                    .map_err(|refusal| refused(&refusal))?;
                recorder.before(target.real(), before);
                // The bits it had go on the temporary file before the
                // rename: the path is never opened again.
                let mode = file.mode.map(|mode| mode & 0o7777);
                if target.replace_with_mode(&bytes, mode).is_err() {
                    recorder.after_disk(target.real());
                    return Err(reason::WRITE_FAILED.into());
                }
                recorder.after(target.real(), Some(&bytes));
                Ok(())
            }
        }
    }
}

fn refused(refusal: &Refusal) -> String {
    match refusal {
        Refusal::Denied(_) => reason::REFUSED.into(),
        Refusal::NotRegular => reason::NOT_FILE.into(),
        Refusal::Io(err) if err.kind() == io::ErrorKind::NotFound => reason::NOT_FILE.into(),
        Refusal::Io(_) => reason::WRITE_FAILED.into(),
    }
}

/// One turn being recorded.
#[derive(Debug)]
pub struct Turn {
    session: SessionStore,
    state: Mutex<TurnState>,
}

#[derive(Debug, Default)]
struct TurnState {
    manifest: Manifest,
    by_path: HashMap<String, usize>,
    /// Taken at the first recorded file.
    number: Option<u64>,
    /// Set when a write failed: the turn records nothing more.
    broken: bool,
}

impl Turn {
    pub fn new(session: SessionStore) -> Self {
        Self {
            state: Mutex::new(TurnState::default()),
            session,
        }
    }

    /// The turn's number, once it recorded a file.
    pub fn number(&self) -> Option<u64> {
        self.lock().number
    }

    /// Whether any file was recorded.
    pub fn changed_files(&self) -> bool {
        !self.lock().manifest.files.is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn dir(&self, number: u64) -> PathBuf {
        self.session.dir.join(number.to_string())
    }

    fn set_undid(&self, turns: Vec<u64>) {
        self.lock().manifest.undid = turns;
    }

    /// Records `before` for `path`, unless the turn has it already.
    /// Failures are reported once on stderr; the change goes on.
    pub fn before(&self, path: &Path, before: Before) {
        let key = path.to_string_lossy().into_owned();
        let mut state = self.lock();
        if state.broken || state.by_path.contains_key(&key) {
            return;
        }
        let number = match state.number {
            Some(number) => number,
            None => match self.session.next_turn() {
                Ok(number) => {
                    state.number = Some(number);
                    state.manifest.turn = number;
                    state.manifest.created_at = now_text();
                    number
                }
                Err(err) => {
                    state.broken = true;
                    eprintln!("  !! file checkpoints are off for this turn: {err}");
                    return;
                }
            },
        };
        let dir = self.dir(number);
        let index = state.manifest.files.len();
        let recorded = Recorded {
            path: key.clone(),
            before: match &before {
                Before::Absent => "absent",
                Before::Content(..) => "content",
                Before::TooLarge(_) => "too_large",
            }
            .to_string(),
            content: matches!(before, Before::Content(..)).then(|| format!("{index}.bin")),
            mode: match &before {
                Before::Content(_, mode) => Some(*mode),
                _ => None,
            },
            size: match &before {
                Before::TooLarge(size) => Some(*size),
                _ => None,
            },
            before_sha256: match &before {
                Before::Content(bytes, _) => Some(sha256(bytes)),
                _ => None,
            },
            after_sha256: None,
            at: now_text(),
            restorable: false,
        };
        let written = mkdir(&dir).and_then(|()| match &before {
            Before::Content(bytes, _) => write_private(&dir.join(format!("{index}.bin")), bytes),
            _ => Ok(()),
        });
        if let Err(err) = written {
            state.broken = true;
            eprintln!("  !! cannot record a file checkpoint (turn {number}): {err}");
            return;
        }
        state.manifest.files.push(recorded);
        state.by_path.insert(key, index);
    }

    /// Records what is at `path` now, read from disk (after a change that
    /// failed half way, or a removal).
    pub fn after_disk(&self, path: &Path) {
        if self
            .lock()
            .by_path
            .contains_key(path.to_string_lossy().as_ref())
        {
            match state(path) {
                Ok(now) => self.set_after(path, now),
                Err(_) => self.set_after(path, Some(String::new())),
            }
        }
    }

    /// Records what is at `path` after a change: `bytes`, or nothing.
    pub fn after(&self, path: &Path, bytes: Option<&[u8]>) {
        self.set_after(path, bytes.map(sha256));
    }

    fn set_after(&self, path: &Path, sha256: Option<String>) {
        let key = path.to_string_lossy();
        let mut state = self.lock();
        let (Some(&index), Some(number)) = (state.by_path.get(key.as_ref()), state.number) else {
            return;
        };
        if state.broken {
            return;
        }
        state.manifest.files[index].after_sha256 = sha256;
        if let Err(err) = self.save(number, &state.manifest) {
            state.broken = true;
            eprintln!("  !! cannot record a file checkpoint (turn {number}): {err}");
        }
    }

    fn save(&self, number: u64, manifest: &Manifest) -> io::Result<()> {
        let dir = self.dir(number);
        mkdir(&dir)?;
        let text = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
        write_private(&dir.join(MANIFEST), &text)
    }

    /// Ends the turn: the session and the store are brought under their
    /// limits, the oldest turns first.
    pub fn finish(&self) {
        let Some(number) = self.number() else {
            return;
        };
        let keep = self.dir(number);
        let result = limit_session(&self.session.dir, SESSION_BYTES, &keep).and_then(|()| {
            let root = self
                .session
                .dir
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            Store { root }.limit_total(TOTAL_BYTES, &keep)
        });
        if let Err(err) = result {
            eprintln!("  !! cannot trim the file checkpoints: {err}");
        }
    }
}

/// Removes a session's oldest turns until it holds at most `limit` bytes.
fn limit_session(session: &Path, limit: u64, keep: &Path) -> io::Result<()> {
    let mut turns = Vec::new();
    for (_, dir) in turn_dirs(session)? {
        let (bytes, _) = folder_size(&dir)?;
        turns.push((dir, bytes));
    }
    let mut total: u64 = turns.iter().map(|(_, bytes)| bytes).sum();
    for (dir, bytes) in turns {
        if total <= limit {
            break;
        }
        if dir == keep {
            continue;
        }
        fs::remove_dir_all(&dir)?;
        total = total.saturating_sub(bytes);
    }
    Ok(())
}
