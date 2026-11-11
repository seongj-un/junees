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

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::prompt_text::{escape_hidden, is_hidden};

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
}

impl ContextFiles {
    /// Reads `names` in `dir` and the folders above it up to the top of
    /// its repository or `home` ([`levels`]).
    pub fn load(dir: &Path, home: Option<&Path>, names: &[String]) -> Self {
        if names.is_empty() {
            return Self::default();
        }
        let start = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let home = home.map(|home| fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
        let levels = levels(&start, home.as_deref());
        let top = levels.last().cloned().unwrap_or_else(|| start.clone());
        let mut out = Self {
            levels: levels.clone(),
            ..Self::default()
        };
        let mut seen: Vec<((u64, u64), PathBuf)> = Vec::new();
        let mut room = TOTAL_LIMIT;
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
                if let Some((_, first)) = seen.iter().find(|(id, _)| *id == found.id) {
                    let why = format!("the same file as {}", first.display());
                    out.skipped.push(Skipped { path, why });
                    continue;
                }
                seen.push((found.id, path.clone()));
                if room == 0 {
                    let why = format!(
                        "left out: the {} KiB limit of all files is used up",
                        TOTAL_LIMIT / 1024
                    );
                    out.skipped.push(Skipped { path, why });
                    continue;
                }
                let mut text = found.text;
                let mut cut = found.cut.then_some(Cut::File);
                if text.len() > room {
                    text.truncate(floor(&text, room));
                    cut = Some(Cut::Total);
                }
                room -= text.len();
                out.files.push(File {
                    path,
                    size: found.size,
                    text,
                    cut,
                });
            }
        }
        out
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
        if self.files.is_empty() {
            let top = self.levels.last().map(|top| top.display().to_string());
            out.push_str(&format!(
                "project instructions: none found (from the working folder up to {})\n",
                escape_hidden(&top.unwrap_or_default())
            ));
        } else {
            let total: usize = self.files.iter().map(|file| file.text.len()).sum();
            out.push_str(&format!(
                "project instructions ({total} bytes; read when the session started, /new \
                 reads them again):\n"
            ));
        }
        for file in &self.files {
            let path = escape_hidden(&file.path.display().to_string());
            match file.cut {
                None => out.push_str(&format!("  {path}: {} bytes\n", file.size)),
                Some(cut) => out.push_str(&format!(
                    "  {path}: {} of {} bytes ({})\n",
                    file.text.len(),
                    file.size,
                    cut_reason(cut)
                )),
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
        if self.files.is_empty() && self.skipped.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = self
            .files
            .iter()
            .map(|file| {
                let path = escape_hidden(&file.path.display().to_string());
                match file.cut {
                    None => path,
                    Some(_) => format!("{path} (cut)"),
                }
            })
            .collect();
        parts.extend(self.skipped.iter().map(|skipped| {
            format!(
                "skipped {} ({})",
                escape_hidden(&skipped.path.display().to_string()),
                escape_hidden(&skipped.why)
            )
        }));
        Some(format!("(project instructions: {})", parts.join(", ")))
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

/// A file read for the prompt.
struct Found {
    /// Device and inode, to notice one file under two names.
    id: (u64, u64),
    size: u64,
    text: String,
    /// Cut at [`FILE_LIMIT`].
    cut: bool,
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
    let mut text = escape_hidden(&String::from_utf8_lossy(&bytes));
    let cut = bytes.len() > FILE_LIMIT || text.len() > FILE_LIMIT;
    text.truncate(floor(&text, FILE_LIMIT));
    Ok(Some(Found {
        id: (meta.dev(), meta.ino()),
        size: meta.len(),
        text,
        cut,
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
