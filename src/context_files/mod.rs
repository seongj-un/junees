//! Project instruction files: `AGENTS.md`, `JUNEES.md` and `CLAUDE.md`
//! (the names are a setting, [`SETTING`]) from the working folder up to
//! the top of its git repository, or up to HOME, in the system prompt.
//!
//! The people who work on a project write them, so unlike the memory they
//! go in as instructions, ranked below the user's messages and Junees's own
//! rules. A session reads them once when it starts (the prompt cache
//! holds) and again on `/new`.
//!
//! Where the walk stops ([`levels`]): at the first folder that holds
//! `.git` (the repository's top) or is HOME. A folder in neither (`/tmp/x`
//! outside a repository) gives its own files only, so a file someone else
//! left in a shared folder above it is never read. A link is followed only
//! to a file inside the folder the walk stops at, and the same file under
//! two names (`CLAUDE.md` linked to `AGENTS.md`) goes in once.
//!
//! Limits: [`FILE_LIMIT`] per file and [`TOTAL_LIMIT`] for all of them,
//! in the order they go in (the top of the repository first, so its
//! general rules win the room); the prompt says where a file was cut.
//! Hidden characters are written as escapes
//! ([`crate::prompt_text::escape_hidden`]).
//!
//! A webhook run, which anyone's text drives, reads the files as the last
//! commit of the repository holds them ([`ContextFiles::load_committed`]),
//! so a change an earlier run made to them never counts; outside a
//! repository it reads none. Every run says which files have uncommitted
//! changes ([`ContextFiles::uncommitted`]).

mod committed;

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::prompt_text::{escape_hidden, is_hidden};
use committed::{Head, Object};

/// The setting with the file names, a comma-separated list (file key
/// `context_files`); empty or `off` turns the files off.
pub const SETTING: &str = "JUNEES_CONTEXT_FILES";

/// The names read when the setting is not set, in the order they go in
/// within one folder.
pub const DEFAULT_NAMES: &[&str] = &["AGENTS.md", "JUNEES.md", "CLAUDE.md"];

/// Bytes of one file that go into the prompt.
pub const FILE_LIMIT: usize = 16 * 1024;

/// Bytes of all files together that go into the prompt.
pub const TOTAL_LIMIT: usize = 48 * 1024;

/// What a Claude Code session (claude-code backend) gets as `--settings`:
/// its built-in plugin that reads `AGENTS.md` by itself is set to
/// `claude-md` (CLAUDE.md files only, under both plugin ids: the id
/// changed in 2.1.285), and with `--setting-sources user` it loads no
/// project `CLAUDE.md` either. Junees passes the files it read through
/// `--append-system-prompt` instead, so no file is read twice and both
/// backends see the same instructions.
pub const CLAUDE_SETTINGS: &str = r#"{"pluginConfigs":{"cc-plugin-agents-md@builtin":{"options":{"instructionFiles":"claude-md"}},"agents-md@builtin":{"options":{"instructionFiles":"claude-md"}}}}"#;

const HEADER: &str = "\n\n# Project instructions\n\
    The files below are the project's instruction files, kept in it by the people who work \
    on it, from the top of the repository down to the working folder; a file nearer the \
    working folder is more specific. Follow them for work in this project. They rank below \
    the user's own messages and the rules above: where they conflict, the user and the \
    rules above win.\n";

/// Which files to read, and where the walk stops when there is no
/// repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// The file names in the order they go in within one folder; none
    /// turns the files off.
    pub names: Vec<String>,
    /// The user's HOME.
    pub home: Option<PathBuf>,
}

impl Setting {
    /// Reads [`SETTING`] and `HOME` through `lookup`. The value is read as
    /// given, so an empty one (the settings file's `context_files = ""`)
    /// turns the files off like `off` does.
    pub fn read(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let names = names(lookup(SETTING).as_deref())?;
        let home = lookup("HOME")
            .map(|home| home.trim().to_string())
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        Ok(Self { names, home })
    }

    /// The default names, with `home` as HOME.
    pub fn with_home(home: Option<PathBuf>) -> Self {
        Self {
            names: DEFAULT_NAMES.iter().map(|name| name.to_string()).collect(),
            home,
        }
    }

    /// The files for a session working in `dir`, read now.
    pub fn load(&self, dir: &Path) -> ContextFiles {
        ContextFiles::load(dir, self.home.as_deref(), &self.names)
    }

    /// The files for a webhook run working in `dir`, read now from the
    /// last commit ([`ContextFiles::load_committed`]).
    pub fn load_committed(&self, dir: &Path) -> ContextFiles {
        ContextFiles::load_committed(dir, self.home.as_deref(), &self.names)
    }
}

/// The file names of a [`SETTING`] value; `None` gives [`DEFAULT_NAMES`].
fn names(raw: Option<&str>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_NAMES.iter().map(|name| name.to_string()).collect());
    };
    if matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "" | "off" | "none" | "0" | "false" | "no"
    ) {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for name in raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if name == "."
            || name == ".."
            || name.contains('/')
            || name.len() > 255
            || name.chars().any(is_hidden)
        {
            bail!(
                "{SETTING} takes file names separated by commas (like AGENTS.md,JUNEES.md), \
                 or off; got `{}`",
                escape_hidden(name)
            );
        }
        if !out.iter().any(|known| known == name) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

/// Why a file was not put in whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// At [`FILE_LIMIT`].
    File,
    /// At [`TOTAL_LIMIT`] (or the room a Claude Code argument has left).
    Total,
}

/// A file that goes into the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: PathBuf,
    /// Its size on disk.
    pub size: u64,
    /// What goes in: hidden characters escaped, cut to the limits.
    pub text: String,
    pub cut: Option<Cut>,
}

/// A file that was found but left out, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub why: String,
}

/// The project instruction files of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextFiles {
    /// In the order they go in: the top folder's first.
    pub files: Vec<File>,
    pub skipped: Vec<Skipped>,
    /// The folders read, the working folder first; empty when the files
    /// are off.
    pub levels: Vec<PathBuf>,
    /// Read from the last commit of the repository, not from the working
    /// tree ([`ContextFiles::load_committed`]).
    pub committed: bool,
    /// Instruction files whose content in the working tree is not what the
    /// last commit holds (or that it does not hold): files that went in
    /// like that, or for a read from the commit, files whose changes were
    /// left out.
    pub uncommitted: Vec<PathBuf>,
}

/// Why a webhook run outside a repository reads no instruction files.
const NOT_IN_A_REPOSITORY: &str = "not read: a webhook run reads instruction files only from \
    the last commit of a git repository, and this folder is not in one";

impl ContextFiles {
    /// Reads `names` in `dir` and the folders above it up to the top of
    /// its repository or `home` ([`levels`]).
    pub fn load(dir: &Path, home: Option<&Path>, names: &[String]) -> Self {
        if names.is_empty() {
            return Self::default();
        }
        let (levels, top) = walk(dir, home);
        let mut out = Self {
            levels: levels.clone(),
            ..Self::default()
        };
        let mut seen = Vec::new();
        let mut room = TOTAL_LIMIT;
        // The files that went in and the file each was read from.
        let mut read_from: Vec<(PathBuf, PathBuf)> = Vec::new();
        for dir in levels.iter().rev() {
            for name in names {
                let path = dir.join(name);
                let found = match read(&path, &top) {
                    Ok(None) => continue,
                    Ok(Some(found)) => found,
                    Err(why) => {
                        out.skipped.push(Skipped { path, why });
                        continue;
                    }
                };
                let target = found.target.clone();
                if out.add(path.clone(), found, &mut seen, &mut room)
                    && let Some(target) = target
                {
                    read_from.push((path, target));
                }
            }
        }
        if let Some(head) = repository(&top) {
            let rels: Vec<(PathBuf, String)> = read_from
                .iter()
                .filter_map(|(path, _)| Some((path.clone(), rel(path, &top)?)))
                .collect();
            let names: Vec<String> = rels.iter().map(|(_, rel)| rel.clone()).collect();
            // Without an answer from git nothing is said about changes.
            if let Ok(objects) = head.lookup(&names) {
                for ((path, _), object) in rels.into_iter().zip(objects) {
                    let target = read_from
                        .iter()
                        .find(|(read, _)| *read == path)
                        .map(|(_, target)| target);
                    if let Some(target) = target
                        && differs(&head, &object, target)
                    {
                        out.uncommitted.push(path);
                    }
                }
            }
        }
        out
    }

    /// Like [`ContextFiles::load`], but reads the files as the last commit
    /// (`HEAD`) of the repository whose top the walk stops at holds them,
    /// for a webhook run: what an earlier run wrote to them without a
    /// commit never goes in. A link in the commit is followed only to a
    /// file in it. Without a repository there (the walk stopped at HOME,
    /// or at the working folder alone) no file is read. The files in the
    /// working tree whose content differs are listed in
    /// [`ContextFiles::uncommitted`].
    pub fn load_committed(dir: &Path, home: Option<&Path>, names: &[String]) -> Self {
        if names.is_empty() {
            return Self::default();
        }
        let (levels, top) = walk(dir, home);
        let mut out = Self {
            levels: levels.clone(),
            committed: true,
            ..Self::default()
        };
        let paths: Vec<PathBuf> = levels
            .iter()
            .rev()
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .collect();
        let Some(head) = repository(&top) else {
            for path in paths {
                if fs::symlink_metadata(&path).is_ok() {
                    let why = NOT_IN_A_REPOSITORY.to_string();
                    out.skipped.push(Skipped { path, why });
                }
            }
            return out;
        };
        let rels: Vec<(PathBuf, String)> = paths
            .into_iter()
            .filter_map(|path| {
                let rel = rel(&path, &top)?;
                Some((path, rel))
            })
            .collect();
        let names: Vec<String> = rels.iter().map(|(_, rel)| rel.clone()).collect();
        let objects = match head.lookup(&names) {
            Ok(objects) => objects,
            Err(err) => {
                let why = format!("cannot read the last commit: {err}");
                out.skipped.push(Skipped { path: top, why });
                return out;
            }
        };
        let mut seen = Vec::new();
        let mut room = TOTAL_LIMIT;
        for ((path, _), object) in rels.into_iter().zip(objects) {
            if let Some(target) = worktree_file(&path, &top)
                && differs(&head, &object, &target)
            {
                out.uncommitted.push(path.clone());
            }
            match object {
                Object::Missing => {}
                Object::Other(why) => out.skipped.push(Skipped { path, why }),
                Object::Blob { oid, size } => match head.read(&oid, FILE_LIMIT + 1) {
                    Ok(bytes) => {
                        let (text, cut) = prompt_text(&bytes);
                        let found = Found {
                            id: Id::Blob(oid),
                            size,
                            text,
                            cut,
                            target: None,
                        };
                        out.add(path, found, &mut seen, &mut room);
                    }
                    Err(why) => out.skipped.push(Skipped { path, why }),
                },
            }
        }
        out
    }

    /// Puts `found` in as the file at `path` unless the same file went in
    /// already (`seen`) or the `room` left for all files is used up;
    /// whether it went in.
    fn add(
        &mut self,
        path: PathBuf,
        found: Found,
        seen: &mut Vec<(Id, PathBuf)>,
        room: &mut usize,
    ) -> bool {
        if let Some((_, first)) = seen.iter().find(|(id, _)| *id == found.id) {
            let why = format!("the same file as {}", first.display());
            self.skipped.push(Skipped { path, why });
            return false;
        }
        seen.push((found.id, path.clone()));
        if *room == 0 {
            let why = format!(
                "left out: the {} KiB limit of all files is used up",
                TOTAL_LIMIT / 1024
            );
            self.skipped.push(Skipped { path, why });
            return false;
        }
        let mut text = found.text;
        let mut cut = found.cut.then_some(Cut::File);
        if text.len() > *room {
            text.truncate(floor(&text, *room));
            cut = Some(Cut::Total);
        }
        *room -= text.len();
        self.files.push(File {
            path,
            size: found.size,
            text,
            cut,
        });
        true
    }

    /// Whether the files are turned off.
    pub fn is_off(&self) -> bool {
        self.levels.is_empty()
    }

    /// The system prompt section; empty when there are no files.
    pub fn section(&self) -> String {
        self.render(TOTAL_LIMIT)
    }

    /// [`ContextFiles::section`] cut further to at most `max` bytes, for
    /// an argument with a size limit. Empty when not even the heading
    /// fits.
    pub fn section_within(&self, max: usize) -> String {
        let mut room = TOTAL_LIMIT;
        loop {
            let section = self.render(room);
            if section.len() <= max {
                return section;
            }
            if room == 0 {
                return String::new();
            }
            room = room.saturating_sub(section.len() - max);
        }
    }

    /// The section with at most `room` bytes of file text.
    fn render(&self, mut room: usize) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut out = String::from(HEADER);
        let mut left_out = 0;
        for file in &self.files {
            if room == 0 {
                left_out += 1;
                continue;
            }
            let mut text = file.text.as_str();
            let mut cut = file.cut;
            if text.len() > room {
                text = &text[..floor(text, room)];
                cut = Some(Cut::Total);
            }
            room -= text.len();
            let path = escape_hidden(&file.path.display().to_string()).replace('"', "\\\"");
            out.push_str(&format!("\n<project-instructions path=\"{path}\">\n{text}"));
            if !text.ends_with('\n') {
                out.push('\n');
            }
            if let Some(cut) = cut {
                out.push_str(&format!(
                    "[cut here: the file has {} bytes; {}]\n",
                    file.size,
                    cut_reason(cut)
                ));
            }
            out.push_str("</project-instructions>\n");
        }
        if left_out > 0 {
            out.push_str(&format!(
                "\n({left_out} more files left out: the limit of all project instruction \
                 files is used up)\n"
            ));
        }
        out
    }

    /// What `/context` shows: the files, their sizes and what was cut or
    /// left out.
    pub fn overview(&self) -> String {
        if self.is_off() {
            return format!("project instructions: off ({SETTING})\n");
        }
        let mut out = String::new();
        let top = escape_hidden(
            &self
                .levels
                .last()
                .map(|top| top.display().to_string())
                .unwrap_or_default(),
        );
        if self.files.is_empty() {
            let found = if self.committed {
                "none in the last commit"
            } else {
                "none found"
            };
            out.push_str(&format!(
                "project instructions: {found} (from the working folder up to {top})\n"
            ));
        } else {
            let total: usize = self.files.iter().map(|file| file.text.len()).sum();
            if self.committed {
                out.push_str(&format!(
                    "project instructions ({total} bytes; read from the last commit of {top} \
                     when the run started, uncommitted changes left out):\n"
                ));
            } else {
                out.push_str(&format!(
                    "project instructions ({total} bytes; read when the session started, /new \
                     reads them again):\n"
                ));
            }
        }
        for file in &self.files {
            let path = escape_hidden(&file.path.display().to_string());
            let changed = if self.has_uncommitted(file) {
                " (uncommitted changes)"
            } else {
                ""
            };
            match file.cut {
                None => out.push_str(&format!("  {path}: {} bytes{changed}\n", file.size)),
                Some(cut) => out.push_str(&format!(
                    "  {path}: {} of {} bytes ({}){changed}\n",
                    file.text.len(),
                    file.size,
                    cut_reason(cut)
                )),
            }
        }
        if self.committed {
            for path in &self.uncommitted {
                out.push_str(&format!(
                    "  uncommitted changes in {}: left out, the last commit's version is used\n",
                    escape_hidden(&path.display().to_string())
                ));
            }
        }
        for skipped in &self.skipped {
            out.push_str(&format!(
                "  skipped {}: {}\n",
                escape_hidden(&skipped.path.display().to_string()),
                escape_hidden(&skipped.why)
            ));
        }
        out
    }

    /// A line for the start of a session (stderr) naming the files read
    /// and the ones skipped; `None` when there are neither.
    pub fn summary(&self) -> Option<String> {
        if self.files.is_empty() && self.skipped.is_empty() && self.uncommitted.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = self
            .files
            .iter()
            .map(|file| {
                let mut path = escape_hidden(&file.path.display().to_string());
                if file.cut.is_some() {
                    path.push_str(" (cut)");
                }
                if self.has_uncommitted(file) {
                    path.push_str(" (uncommitted changes)");
                }
                path
            })
            .collect();
        if self.committed {
            parts.extend(self.uncommitted.iter().map(|path| {
                format!(
                    "uncommitted changes in {} left out",
                    escape_hidden(&path.display().to_string())
                )
            }));
        }
        parts.extend(self.skipped.iter().map(|skipped| {
            format!(
                "skipped {} ({})",
                escape_hidden(&skipped.path.display().to_string()),
                escape_hidden(&skipped.why)
            )
        }));
        let from = if self.committed {
            " from the last commit"
        } else {
            ""
        };
        Some(format!(
            "(project instructions{from}: {})",
            parts.join(", ")
        ))
    }

    /// Whether `file` went in with uncommitted changes (only a read from
    /// the working tree puts them in).
    fn has_uncommitted(&self, file: &File) -> bool {
        !self.committed && self.uncommitted.contains(&file.path)
    }
}

fn cut_reason(cut: Cut) -> String {
    match cut {
        Cut::File => format!("only the first {} KiB go in", FILE_LIMIT / 1024),
        Cut::Total => "the rest did not fit the limit of all project instruction files".into(),
    }
}

/// The folders whose files a session in `start` reads, `start` first:
/// up to and with the first one that holds `.git` or is `home`. When
/// there is neither above it, `start` alone.
pub fn levels(start: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in start.ancestors() {
        out.push(dir.to_path_buf());
        if Some(dir) == home || fs::symlink_metadata(dir.join(".git")).is_ok() {
            return out;
        }
    }
    vec![start.to_path_buf()]
}

/// What tells one file under two names apart from two files.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Id {
    /// Device and inode, in the working tree.
    Inode(u64, u64),
    /// The blob, in the last commit.
    Blob(String),
}

/// A file read for the prompt.
struct Found {
    id: Id,
    size: u64,
    text: String,
    /// Cut at [`FILE_LIMIT`].
    cut: bool,
    /// The file in the working tree it was read from (a link's target).
    target: Option<PathBuf>,
}

/// The folders a session in `dir` reads ([`levels`]), and the one the
/// walk stops at, all canonical.
fn walk(dir: &Path, home: Option<&Path>) -> (Vec<PathBuf>, PathBuf) {
    let start = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let home = home.map(|home| fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
    let levels = levels(&start, home.as_deref());
    let top = levels.last().cloned().unwrap_or(start);
    (levels, top)
}

/// The repository of a walk that stopped at `top`: only when `top` holds
/// `.git`.
fn repository(top: &Path) -> Option<Head<'_>> {
    fs::symlink_metadata(top.join(".git"))
        .is_ok()
        .then_some(Head { top })
}

/// `path` as git names it in a commit of the repository at `top`; `None`
/// when it is outside or cannot be named (not UTF-8, a line break).
fn rel(path: &Path, top: &Path) -> Option<String> {
    let rel = path.strip_prefix(top).ok()?;
    let parts: Option<Vec<&str>> = rel
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect();
    let rel = parts?.join("/");
    (!rel.is_empty() && !rel.contains('\n')).then_some(rel)
}

/// The regular file in the working tree that `path` is or links to,
/// when it is inside `top`.
fn worktree_file(path: &Path, top: &Path) -> Option<PathBuf> {
    fs::symlink_metadata(path).ok()?;
    let target = fs::canonicalize(path).ok()?;
    (target.starts_with(top) && fs::metadata(&target).ok()?.is_file()).then_some(target)
}

/// Whether the working tree's file `target` is not what the commit holds
/// (`object`). Unknown (an error) counts as the same.
fn differs(head: &Head<'_>, object: &Object, target: &Path) -> bool {
    match object {
        Object::Blob { oid, size } => head.same(oid, *size, target).is_ok_and(|same| !same),
        Object::Missing | Object::Other(_) => true,
    }
}

/// What goes into the prompt of a file's first bytes: hidden characters
/// escaped, cut at [`FILE_LIMIT`], and whether it was cut.
fn prompt_text(bytes: &[u8]) -> (String, bool) {
    let mut text = escape_hidden(&String::from_utf8_lossy(bytes));
    let cut = bytes.len() > FILE_LIMIT || text.len() > FILE_LIMIT;
    text.truncate(floor(&text, FILE_LIMIT));
    (text, cut)
}

/// Reads instruction file `path`: `None` when there is none, and why it is
/// skipped when it is a link out of `top`, a link to nothing, not a
/// regular file or unreadable.
fn read(path: &Path, top: &Path) -> Result<Option<Found>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot read it: {err}")),
    };
    // The file opened is the link's target, never a link: one put in its
    // place after this check makes the open fail.
    let target = if meta.file_type().is_symlink() {
        match fs::canonicalize(path) {
            Ok(target) if target.starts_with(top) => target,
            Ok(target) => {
                return Err(format!(
                    "a link to {}, outside {}",
                    target.display(),
                    top.display()
                ));
            }
            Err(_) => return Err("a link to nothing".into()),
        }
    } else {
        path.to_path_buf()
    };
    // Non-blocking, so a FIFO under the name cannot hold the session.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&target)
        .map_err(|err| format!("cannot read it: {err}"))?;
    let meta = file
        .metadata()
        .map_err(|err| format!("cannot read it: {err}"))?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("cannot read it: {err}"))?;
    let (text, cut) = prompt_text(&bytes);
    Ok(Some(Found {
        id: Id::Inode(meta.dev(), meta.ino()),
        size: meta.len(),
        text,
        cut,
        target: Some(target),
    }))
}

/// The largest char boundary of `text` at or below `at`.
fn floor(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    (0..=at)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
