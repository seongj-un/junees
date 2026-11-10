//! Persistent memory: short notes that outlive a session, kept in three
//! stores under the state folder and shown to the model in the system
//! prompt of every later session.
//!
//! - `user`: facts about the user (name, preferences, language),
//!   `$JUNEES_HOME/memory/user.md`.
//! - `notes`: the environment, conventions, lessons learned,
//!   `$JUNEES_HOME/memory/notes.md`.
//! - `project`: notes on one project, kept per git root (or folder, outside
//!   git) in `$JUNEES_HOME/projects/<hash of the root path>/notes.md`, so
//!   nothing is written into the repository.
//!
//! A store holds short entries separated by lines holding just `§`. Each
//! store has a character limit; a change that would go over it is refused,
//! so the prompt section stays bounded. Sessions read the stores once when
//! they start ([`Memory::snapshot`]) and keep that text in their system
//! prompt, so a change made during a session does not break the prompt
//! cache: it shows from the next session on.

mod secret;
mod tool;

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::prompt_text::{check_plain, escape_hidden};

pub use secret::secret_kind;
pub use tool::MemoryTool;

/// The line between two entries of a store file.
pub const SEPARATOR: &str = "§";

/// Default character limits of the stores.
pub const DEFAULT_USER_CHARS: usize = 1500;
pub const DEFAULT_NOTES_CHARS: usize = 2500;
pub const DEFAULT_PROJECT_CHARS: usize = 2500;

/// The highest limit a store may be given. With all three at it, the
/// prompt section stays well below the 128 KiB that one argument (Claude
/// Code's `--append-system-prompt`) may have on Linux, even in 4-byte
/// characters.
pub const MAX_CHARS: usize = 8000;

/// Store files larger than this are not read (they were not written by
/// Junees, which keeps them under [`MAX_CHARS`]).
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// One of the three stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    User,
    Notes,
    Project,
}

impl Target {
    pub const ALL: [Target; 3] = [Target::User, Target::Notes, Target::Project];

    /// The name the tool and `/memory clear` take.
    pub fn name(self) -> &'static str {
        match self {
            Target::User => "user",
            Target::Notes => "notes",
            Target::Project => "project",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|target| target.name() == text)
    }

    /// The settings variable of the store's limit.
    pub fn limit_env(self) -> &'static str {
        match self {
            Target::User => "JUNEES_MEMORY_USER_CHARS",
            Target::Notes => "JUNEES_MEMORY_NOTES_CHARS",
            Target::Project => "JUNEES_MEMORY_PROJECT_CHARS",
        }
    }
}

/// Character limits of the stores (`memory_*_chars` in the settings file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub user: usize,
    pub notes: usize,
    pub project: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            user: DEFAULT_USER_CHARS,
            notes: DEFAULT_NOTES_CHARS,
            project: DEFAULT_PROJECT_CHARS,
        }
    }
}

impl Limits {
    /// Reads `JUNEES_MEMORY_USER_CHARS`, `JUNEES_MEMORY_NOTES_CHARS` and
    /// `JUNEES_MEMORY_PROJECT_CHARS`; each from 1 to [`MAX_CHARS`].
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut limits = Self::default();
        for target in Target::ALL {
            let name = target.limit_env();
            let Some(raw) = get(name) else { continue };
            let value = match raw.parse::<usize>() {
                Ok(value) if (1..=MAX_CHARS).contains(&value) => value,
                _ => bail!("{name} must be a whole number from 1 to {MAX_CHARS}, got `{raw}`"),
            };
            *limits.of_mut(target) = value;
        }
        Ok(limits)
    }

    pub fn of(&self, target: Target) -> usize {
        match target {
            Target::User => self.user,
            Target::Notes => self.notes,
            Target::Project => self.project,
        }
    }

    fn of_mut(&mut self, target: Target) -> &mut usize {
        match target {
            Target::User => &mut self.user,
            Target::Notes => &mut self.notes,
            Target::Project => &mut self.project,
        }
    }
}

/// Who sees the memory, which decides the guidance in the prompt section
/// and what the tool may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A conversation with the user: every store can be changed.
    Conversation,
    /// An unattended task session: only the project notes can be changed,
    /// so no session changes what Junees knows about the user unasked.
    Task,
    /// Claude Code (the `claude-code` backend): it gets the text but not
    /// the tool, which is Junees's own.
    ReadOnly,
}

/// A change the memory tool makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Add(String),
    /// Replaces the one entry that contains `old` with `new`.
    Replace {
        old: String,
        new: String,
    },
    /// Removes the one entry that contains `old`.
    Remove(String),
}

/// A store after a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// What happened: `added`, `replaced`, `removed`, or `already saved`
    /// (an add of an entry that is there, word for word).
    pub what: &'static str,
    pub entries: usize,
    pub chars: usize,
    pub limit: usize,
}

/// The memory of one project folder: where the stores are and their limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// `$JUNEES_HOME/memory`, holding `user.md` and `notes.md`.
    dir: PathBuf,
    /// The project the `project` store belongs to.
    project_root: PathBuf,
    /// `$JUNEES_HOME/projects/<hash>`, holding its `notes.md`.
    project_dir: PathBuf,
    limits: Limits,
}

impl Memory {
    /// The memory under the state folder `home` for the project at
    /// `project_root`.
    pub fn new(home: &Path, project_root: &Path, limits: Limits) -> Self {
        Self {
            dir: home.join("memory"),
            project_root: project_root.to_path_buf(),
            project_dir: home.join("projects").join(project_key(project_root)),
            limits,
        }
    }

    /// The memory for work in `cwd`: its project is the git repository
    /// `cwd` is in, or `cwd` itself outside git.
    pub fn for_folder(home: &Path, cwd: &Path, limits: Limits) -> Self {
        let root = crate::task::git::toplevel(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        Self::new(home, &root, limits)
    }

    pub fn path(&self, target: Target) -> PathBuf {
        match target {
            Target::User => self.dir.join("user.md"),
            Target::Notes => self.dir.join("notes.md"),
            Target::Project => self.project_dir.join("notes.md"),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The entries of a store; none when its file does not exist.
    pub fn entries(&self, target: Target) -> Result<Vec<String>> {
        let path = self.path(target);
        let text = match read_store(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        Ok(parse(&text))
    }

    /// The stores as they are now, for a session's system prompt.
    pub fn snapshot(&self) -> Snapshot {
        let stores = Target::ALL
            .into_iter()
            .map(|target| {
                let limit = self.limits.of(target);
                match self.entries(target) {
                    Ok(entries) => Store {
                        target,
                        text: render(&entries),
                        entries: entries.len(),
                        limit,
                        error: None,
                    },
                    Err(err) => Store {
                        target,
                        text: String::new(),
                        entries: 0,
                        limit,
                        error: Some(format!("{err:#}")),
                    },
                }
            })
            .collect();
        Snapshot {
            stores,
            project_root: self.project_root.clone(),
        }
    }

    /// Makes `change` to a store, holding a lock on its folder so two
    /// Junees processes cannot lose each other's changes. The error is a
    /// message for the model.
    pub fn change(&self, target: Target, change: Change) -> Result<Changed, String> {
        let limit = self.limits.of(target);
        let new_text = match &change {
            Change::Add(text) | Change::Replace { new: text, .. } => Some(check_entry(text)?),
            Change::Remove(_) => None,
        };
        let path = self.path(target);
        let dir = path.parent().expect("store files are in a folder");
        let _lock = lock(dir).map_err(|err| format!("{err:#}"))?;
        let mut entries = self.entries(target).map_err(|err| format!("{err:#}"))?;
        let before = chars(&entries);
        let what = match change {
            Change::Add(_) => {
                let entry = new_text.expect("checked above");
                if entries.contains(&entry) {
                    return Ok(Changed {
                        what: "already saved",
                        entries: entries.len(),
                        chars: chars(&entries),
                        limit,
                    });
                }
                entries.push(entry);
                "added"
            }
            Change::Replace { old, .. } => {
                let index = find_one(&entries, &old, target)?;
                entries[index] = new_text.expect("checked above");
                "replaced"
            }
            Change::Remove(old) => {
                let index = find_one(&entries, &old, target)?;
                entries.remove(index);
                "removed"
            }
        };
        let total = chars(&entries);
        // A store edited by hand may be over its limit already; changes
        // that shrink it are still welcome.
        if total > limit && total > before {
            let current = self.entries(target).unwrap_or_default();
            return Err(format!(
                "not saved: {} memory would hold {total} of its {limit} characters. \
                 Make room first: remove entries that matter less, or replace long ones \
                 with shorter text (action replace or remove with old_text). \
                 Current entries ({} characters):\n{}",
                target.name(),
                chars(&current),
                listing(&current)
            ));
        }
        write_store(&path, &entries).map_err(|err| format!("{err:#}"))?;
        Ok(Changed {
            what,
            entries: entries.len(),
            chars: total,
            limit,
        })
    }

    /// What `/memory` shows: each store's file, size and entries.
    pub fn overview(&self) -> String {
        let snapshot = self.snapshot();
        let mut out = String::new();
        for store in &snapshot.stores {
            let path = self.path(store.target);
            out.push_str(&format!("{} ({}): ", store.target.name(), path.display()));
            if let Some(error) = &store.error {
                out.push_str(&format!("cannot read: {error}\n"));
                continue;
            }
            out.push_str(&format!(
                "{} entries, {}/{} characters\n",
                store.entries,
                store.chars(),
                store.limit
            ));
            for line in escape_hidden(&store.text).lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out.push_str(&format!(
            "project = {}. A session sees memory as it was when it started; /new starts \
             one with the memory as it is now.\n",
            self.project_root.display()
        ));
        out
    }

    /// Empties a store; returns how many entries it had.
    pub fn clear(&self, target: Target) -> Result<usize> {
        let path = self.path(target);
        let dir = path.parent().expect("store files are in a folder");
        if !dir.exists() {
            return Ok(0);
        }
        let _lock = lock(dir)?;
        let count = self.entries(target)?.len();
        match fs::remove_file(&path) {
            Err(err) if err.kind() != ErrorKind::NotFound => {
                Err(err).with_context(|| format!("cannot delete {}", path.display()))
            }
            _ => Ok(count),
        }
    }
}

/// The stores of one moment, as [`Memory::snapshot`] read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub stores: Vec<Store>,
    pub project_root: PathBuf,
}

/// One store in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub target: Target,
    /// The entries, separated by `§` lines.
    pub text: String,
    pub entries: usize,
    pub limit: usize,
    /// Why the store could not be read.
    pub error: Option<String>,
}

impl Store {
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

impl Snapshot {
    /// The `MEMORY (persistent notes)` section of a system prompt, starting
    /// with an empty line. For Claude Code, which cannot change memory,
    /// there is no section while every store is empty.
    pub fn section(&self, mode: Mode) -> String {
        if mode == Mode::ReadOnly
            && self
                .stores
                .iter()
                .all(|store| store.entries == 0 && store.error.is_none())
        {
            return String::new();
        }
        let mut out = String::from("\nMEMORY (persistent notes)\n");
        out.push_str(match mode {
            Mode::Conversation => CONVERSATION_GUIDE,
            Mode::Task => TASK_GUIDE,
            Mode::ReadOnly => READ_ONLY_GUIDE,
        });
        for store in &self.stores {
            let title = match store.target {
                Target::User => "About the user".to_string(),
                Target::Notes => "Notes on the environment and conventions".to_string(),
                Target::Project => format!(
                    "Notes on this project ({})",
                    escape_hidden(&self.project_root.display().to_string())
                ),
            };
            out.push_str(&format!(
                "\n[{title}; {}: {}/{} characters]\n",
                store.target.name(),
                store.chars(),
                store.limit
            ));
            if let Some(error) = &store.error {
                out.push_str(&format!("(cannot read this memory: {error})\n"));
            } else if store.text.is_empty() {
                out.push_str("(empty)\n");
            } else {
                // A store edited by hand may hold what `check_entry` refuses.
                out.push_str(&cut(&escape_hidden(&store.text), store.limit));
                out.push('\n');
            }
        }
        out
    }
}

const CONVERSATION_GUIDE: &str = "\
What earlier sessions saved with the memory tool, as it was when this session started. \
A change you make now is saved at once but shows here only from the next session.\n\
- Save, without being asked, when the user tells you a preference or a fact about \
themselves or corrects you (target user), when you learn a fact about the environment \
or a convention that will matter again (target notes), or one that only concerns this \
project (target project).\n\
- Keep each entry to a short fact. Do not save task progress, temporary state, or \
what is quick to look up again; never save secrets.\n\
- When a memory is full, replace or remove entries that matter less.\n";

const TASK_GUIDE: &str = "\
What earlier sessions saved with the memory tool, as it was when this session started; \
a change you make now shows here from the next session.\n\
- This is an unattended task session: you can only change the notes on this project \
(target project). Save conventions and facts about this repository that later sessions \
need; keep the task's progress in .junees/progress.md, not in memory. Never save \
secrets.\n";

const READ_ONLY_GUIDE: &str = "\
Notes Junees saved in earlier sessions: facts about the user and notes on the \
environment and this project. Take them into account; they cannot be changed in this \
session.\n";

/// The folder name of a project's memory: the FNV-1a hash of its root
/// path, in hex. Stable across Junees versions and platforms, unlike
/// `std`'s hasher.
pub fn project_key(root: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The entries of a store file.
fn parse(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut push = |current: &mut String| {
        let entry = current.trim();
        if !entry.is_empty() {
            entries.push(entry.to_string());
        }
        current.clear();
    };
    for line in text.lines() {
        if line.trim() == SEPARATOR {
            push(&mut current);
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    push(&mut current);
    entries
}

fn render(entries: &[String]) -> String {
    entries.join(&format!("\n{SEPARATOR}\n"))
}

fn chars(entries: &[String]) -> usize {
    render(entries).chars().count()
}

/// The entries numbered, for a message to the model.
fn listing(entries: &[String]) -> String {
    if entries.is_empty() {
        return "(none)".into();
    }
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| format!("{}. {entry} ({} characters)", i + 1, entry.chars().count()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first `limit` characters of `text`, marked when cut. A store file
/// edited by hand may be over its limit.
fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        None => text.to_string(),
        Some((end, _)) => format!(
            "{}\n[... cut at the {limit}-character limit of this memory: shorten it]",
            &text[..end]
        ),
    }
}

/// A new entry as stored, or why it cannot be.
fn check_entry(text: &str) -> Result<String, String> {
    let entry = text.trim();
    if entry.is_empty() {
        return Err("`content` is empty".into());
    }
    // It goes into every later system prompt and onto the terminal.
    check_plain("`content`", entry)?;
    if entry.lines().any(|line| line.trim() == SEPARATOR) {
        return Err(format!(
            "`content` has a line holding just `{SEPARATOR}`, which separates entries; \
             save one entry per call"
        ));
    }
    if let Some(kind) = secret_kind(entry) {
        return Err(format!(
            "not saved: the content looks like a secret ({kind}). Memory is plain text \
             shown to the model in every later session, so it never holds credentials; \
             save where to find it instead (for example the name of the variable)"
        ));
    }
    Ok(entry.to_string())
}

/// The index of the one entry that contains `old`.
fn find_one(entries: &[String], old: &str, target: Target) -> Result<usize, String> {
    if old.trim().is_empty() {
        return Err("`old_text` is empty: give a part of the entry to change".into());
    }
    let found: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.contains(old))
        .map(|(i, _)| i)
        .collect();
    match found.as_slice() {
        [index] => Ok(*index),
        [] => Err(format!(
            "no entry of {} memory contains `old_text`. Current entries:\n{}",
            target.name(),
            listing(entries)
        )),
        _ => {
            let matching: Vec<String> = found.iter().map(|&i| entries[i].clone()).collect();
            Err(format!(
                "`old_text` is in {} entries of {} memory; give a longer part that only \
                 one of them contains. Matching entries:\n{}",
                found.len(),
                target.name(),
                listing(&matching)
            ))
        }
    }
}

fn read_store(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(std::io::Error::other(format!(
            "larger than {MAX_FILE_BYTES} bytes"
        )));
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut &file, &mut text)?;
    Ok(text)
}

fn create_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// An exclusive lock on the folder of a store, created if missing (0700),
/// held until the returned file is dropped.
fn lock(dir: &Path) -> Result<File> {
    create_dir(dir)?;
    let file = File::open(dir).with_context(|| format!("cannot open {}", dir.display()))?;
    match crate::flock::try_exclusive(&file) {
        Ok(true) => Ok(file),
        Ok(false) => bail!("another Junees is changing this memory right now; try again"),
        Err(err) => Err(err).with_context(|| format!("cannot lock {}", dir.display())),
    }
}

/// Writes a store file whole: a temporary file (0600), synced, renamed over
/// the old one, so a crash leaves the old or the new file, never half of one.
fn write_store(path: &Path, entries: &[String]) -> Result<()> {
    let dir = path.parent().expect("store files are in a folder");
    create_dir(dir)?;
    let name = path.file_name().expect("store files have names");
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut text = render(entries);
    if !text.is_empty() {
        text.push('\n');
    }
    let write = || -> std::io::Result<()> {
        match fs::remove_file(&tmp) {
            Err(err) if err.kind() != ErrorKind::NotFound => return Err(err),
            _ => {}
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(|err| {
        let _ = fs::remove_file(&tmp);
        anyhow::Error::new(err).context(format!("cannot write {}", path.display()))
    })
}

#[cfg(test)]
mod tests;
