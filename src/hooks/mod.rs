//! Hooks: commands the user sets to run at fixed points of a session
//! (`pre_tool`, `post_tool`, `turn_end`, `session_start`, `user_prompt`).
//! A hook can block a tool call or an input (exit code 2) or add a note
//! for the model (`{"message": "..."}` on stdout); it can never change a
//! tool's input, and a call it lets through still goes through the
//! approval questions and the policies. See `docs/hooks.md`.
//!
//! Where hooks come from:
//! - the user's: the `[[hooks]]` tables of `$JUNEES_HOME/config.toml`
//!   (after its flat settings), then of `$JUNEES_HOME/hooks.toml`. They
//!   run without a question; a broken one stops the run from starting,
//!   since a policy hook that silently drops out would let calls through.
//! - the project's: `.junees/hooks.toml` in the folder the project
//!   instruction files are read up to ([`crate::context_files::levels`]).
//!   Someone else may have written it, so it runs only once a person
//!   trusted its exact content ([`trust`]); unattended runs never ask and
//!   skip it with a warning.

pub mod config;
pub mod trust;

#[cfg(test)]
mod tests;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use config::{Event, OnError, Spec};

/// The user's hooks file in the state folder.
pub const FILE_NAME: &str = "hooks.toml";
/// A project's hooks file, under the folder the instruction files are
/// read up to.
pub const PROJECT_FILE: &str = ".junees/hooks.toml";
/// A larger hooks file is refused unread.
const MAX_FILE_BYTES: u64 = 256 * 1024;

/// Where a hook was set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The user's settings file or `hooks.toml`.
    User(PathBuf),
    /// A project's `.junees/hooks.toml`.
    Project(PathBuf),
}

impl Origin {
    pub fn file(&self) -> &Path {
        match self {
            Origin::User(file) | Origin::Project(file) => file,
        }
    }

    /// `user` or `project`.
    pub fn kind(&self) -> &'static str {
        match self {
            Origin::User(_) => "user",
            Origin::Project(_) => "project",
        }
    }
}

/// A hook and where it was set.
#[derive(Debug, Clone)]
pub struct Hook {
    pub spec: Spec,
    pub origin: Origin,
}

/// The user's hooks of the state folder `home`: the settings file's, then
/// `hooks.toml`'s, each in file order. An error names the file and line.
pub fn user_hooks(home: &Path) -> Result<Vec<Hook>> {
    let mut hooks = Vec::new();
    let settings = home.join(crate::config_file::FILE_NAME);
    let file = crate::config_file::ConfigFile::load(&settings)?;
    if let Some((first_line, text)) = file.hooks() {
        for spec in config::parse(text, first_line)
            .with_context(|| format!("the hooks in {}", settings.display()))?
        {
            hooks.push(Hook {
                spec,
                origin: Origin::User(settings.clone()),
            });
        }
    }
    let path = home.join(FILE_NAME);
    if let Some(bytes) =
        read_file(&path).with_context(|| format!("cannot read {}", path.display()))?
    {
        let text = String::from_utf8(bytes)
            .ok()
            .with_context(|| format!("{} is not UTF-8 text", path.display()))?;
        for spec in
            config::parse(&text, 1).with_context(|| format!("the hooks in {}", path.display()))?
        {
            hooks.push(Hook {
                spec,
                origin: Origin::User(path.clone()),
            });
        }
    }
    Ok(hooks)
}

/// A project's hooks file as read now.
#[derive(Debug, Clone)]
pub struct ProjectFile {
    /// Its path, under the canonical top folder.
    pub path: PathBuf,
    /// The SHA-256 of its bytes, in hex: what a person trusts.
    pub digest: String,
    /// Its text, for the trust question.
    pub text: String,
    /// Its hooks, or why they cannot be read.
    pub hooks: Result<Vec<Spec>, String>,
}

impl ProjectFile {
    /// Its hooks, marked as the project's; none when it is broken.
    pub fn hooks(&self) -> Vec<Hook> {
        self.hooks
            .as_ref()
            .map(|specs| {
                specs
                    .iter()
                    .cloned()
                    .map(|spec| Hook {
                        spec,
                        origin: Origin::Project(self.path.clone()),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Where the hooks file of a project worked on in `cwd` would be: under
/// the folder the instruction files are read up to (the repository's top,
/// HOME, or `cwd` itself). `user_home` is HOME.
pub fn project_path(cwd: &Path, user_home: Option<&Path>) -> PathBuf {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let user_home =
        user_home.map(|home| std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
    let levels = crate::context_files::levels(&start, user_home.as_deref());
    levels.last().cloned().unwrap_or(start).join(PROJECT_FILE)
}

/// The hooks file of the project worked on in `cwd`, if there is one.
/// `None` too when it is the user's own `hooks.toml` (the walk stopped at
/// HOME and the state folder is `~/.junees`). `Err` when it is there but
/// cannot be read (a link, not a regular file, too large).
pub fn project_file(
    cwd: &Path,
    user_home: Option<&Path>,
    junees_home: Option<&Path>,
) -> Result<Option<ProjectFile>, String> {
    let path = project_path(cwd, user_home);
    if let Some(home) = junees_home {
        let own =
            std::fs::canonicalize(home.join(FILE_NAME)).unwrap_or_else(|_| home.join(FILE_NAME));
        let found = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if own == found {
            return Ok(None);
        }
    }
    let bytes = match read_file(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let digest = digest(&bytes);
    let (text, hooks) = match String::from_utf8(bytes) {
        Ok(text) => {
            let hooks = config::parse(&text, 1).map_err(|err| format!("{err:#}"));
            (text, hooks)
        }
        Err(_) => (String::new(), Err("it is not UTF-8 text".to_string())),
    };
    Ok(Some(ProjectFile {
        path,
        digest,
        text,
        hooks,
    }))
}

/// The SHA-256 of `bytes` in hex.
pub fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The bytes of the regular file `path`, `None` when it does not exist.
/// A link (as the last part), a FIFO or a file over [`MAX_FILE_BYTES`] is
/// an error, never followed or waited on.
fn read_file(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(std::io::Error::other(
                "it is a link; hooks files must be regular files",
            ));
        }
        Err(err) => return Err(err),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(std::io::Error::other(format!(
            "it is larger than {} KiB",
            MAX_FILE_BYTES / 1024
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(std::io::Error::other("it grew while being read"));
    }
    Ok(Some(bytes))
}
