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

pub mod cli;
pub mod config;
pub mod run;
pub mod trust;

#[cfg(test)]
mod tests;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use config::{Event, OnError, Spec};
pub use run::Hooks;

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

/// The hooks a run finds for a project in `project_dir`: the user's, the
/// project's when trusted, and an untrusted project file to ask about or
/// skip.
#[derive(Debug, Default)]
pub struct Found {
    pub hooks: Vec<Hook>,
    /// A project hooks file that is not trusted with its current content.
    pub untrusted: Option<ProjectFile>,
    /// What the person should know (a project file that cannot be read).
    pub warnings: Vec<String>,
}

/// The hooks of a run of the state folder `home` on the project in
/// `project_dir`. Fails when a user hook is broken: a policy hook that
/// silently dropped out would let calls through.
pub fn find(home: Option<&Path>, project_dir: &Path) -> Result<Found> {
    let mut found = Found::default();
    if let Some(home) = home {
        found.hooks = user_hooks(home)?;
    }
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    match project_file(project_dir, user_home.as_deref(), home) {
        Ok(None) => {}
        Ok(Some(file)) => match &file.hooks {
            Err(err) => found.warnings.push(format!(
                "the project hooks file {} cannot be read, so its hooks do not run: {err}",
                file.path.display()
            )),
            Ok(_) if home.is_some_and(|home| trust::is_trusted(home, &file)) => {
                found.hooks.extend(file.hooks());
            }
            Ok(_) => found.untrusted = Some(file),
        },
        Err(err) => found
            .warnings
            .push(format!("{err}; the project's hooks do not run")),
    }
    Ok(found)
}

impl Found {
    /// Leaves the untrusted project file out, with a warning: runs that
    /// cannot ask (unattended, gateways, `-p`).
    pub fn skip_untrusted(&mut self) {
        if let Some(file) = self.untrusted.take() {
            self.warnings.push(format!(
                "the project hooks file {} is not trusted (new, or changed since it was \
                 trusted), so its hooks do not run; read it, then run `junees hooks trust` \
                 in the project",
                file.path.display()
            ));
        }
    }

    /// Trusts the untrusted project file (a person said yes) and adds its
    /// hooks.
    pub fn trust_untrusted(&mut self, home: &Path) -> Result<()> {
        if let Some(file) = self.untrusted.take() {
            trust::trust(home, &file)?;
            self.hooks.extend(file.hooks());
        }
        Ok(())
    }

    /// The hooks to give the agent of a session working in `workdir`
    /// under `tools` (its bash sandbox and hidden variables); `None`
    /// without any.
    pub fn build(
        self,
        workdir: &Path,
        tools: &crate::tools::ToolSettings,
        hide: run::Hide,
    ) -> Option<std::sync::Arc<Hooks>> {
        if self.hooks.is_empty() {
            return None;
        }
        Some(std::sync::Arc::new(Hooks::new(
            self.hooks,
            workdir,
            &tools.hidden_env,
            crate::sandbox::Sandbox::for_tools(tools),
            hide,
        )))
    }
}

/// The question a person answers before a project's hooks file first
/// runs, whole: they approve what they read.
pub fn trust_question(file: &ProjectFile) -> String {
    format!(
        "  이 프로젝트의 훅 파일을 처음 보거나, 승인한 뒤 내용이 바뀌었습니다: {}\n  \
         승인하면 이 내용의 훅이 이 세션부터 실행됩니다(내용이 바뀌면 다시 묻습니다).\n{}",
        file.path.display(),
        cli::quoted(&file.text)
    )
}

/// Hides the values of the secret settings and variables in what hooks
/// get ([`crate::doctor::hide_known_secrets`]).
pub fn hider() -> run::Hide {
    match crate::config_file::Settings::from_env() {
        Ok(settings) => crate::doctor::known_secrets_hider(&settings),
        Err(_) => crate::doctor::env_secrets_hider(),
    }
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
