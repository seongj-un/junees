//! Confinement of the commands the model runs through the API backend's
//! bash tool with Linux Landlock, on top of the command policy's deny
//! list ([`crate::tools::CommandPolicy`]).
//!
//! A confined command and everything it starts may:
//! - write only in the working folder, the temporary folders (`/tmp`,
//!   `/var/tmp`, `/dev/shm`, `$TMPDIR`) and the folders of the
//!   `sandbox_write` setting, plus write to existing files under `/dev`
//!   (`/dev/null`, terminals);
//! - read everything (or only what `sandbox_read` lists, plus the
//!   writable folders), except the settings file, the MCP server list,
//!   the secrets folder and, in a webhook run, the git and GitHub CLI
//!   credentials;
//! - never write in Junees's data folders (memory, skills, cron, ...);
//! - with `sandbox_network = 0` and a kernel of Landlock ABI 4 or later,
//!   neither connect nor bind TCP sockets.
//!
//! It also runs with `no_new_privs`: setuid programs (sudo) give no
//! privileges. `sandbox = auto|require|off` says what happens when the
//! kernel has no Landlock: `auto` runs commands without it and says so in
//! the first result, `require` refuses to run them. Unattended runs
//! (scheduled jobs, webhooks) default to `require`.
//!
//! The claude-code backend is not confined here: Claude Code runs its own
//! tools. Plugin commands and MCP servers are programs the user installed
//! and enabled, and are not confined either.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

#[cfg(target_os = "linux")]
mod landlock;
#[cfg(test)]
mod tests;

/// What to do about confinement (`sandbox`, `JUNEES_SANDBOX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Confine when the kernel can; otherwise run without and say so.
    Auto,
    /// Confine, or refuse to run the command.
    Require,
    /// Do not confine.
    Off,
}

impl Mode {
    /// Reads a setting value.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "require" => Ok(Self::Require),
            "off" => Ok(Self::Off),
            _ => bail!(
                "{MODE_ENV} must be auto, require or off, got `{}`",
                raw.trim()
            ),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Require => "require",
            Self::Off => "off",
        }
    }
}

pub const MODE_ENV: &str = "JUNEES_SANDBOX";
pub const WRITE_ENV: &str = "JUNEES_SANDBOX_WRITE";
pub const READ_ENV: &str = "JUNEES_SANDBOX_READ";
pub const NETWORK_ENV: &str = "JUNEES_SANDBOX_NETWORK";

/// The temporary folders commands may always write in, besides `$TMPDIR`.
const TEMP_FOLDERS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];

/// The sandbox settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `JUNEES_SANDBOX`; `None` when not set: `auto`, or `require` in an
    /// unattended run.
    pub mode: Option<Mode>,
    /// More folders commands may write in (`JUNEES_SANDBOX_WRITE`).
    pub write: Vec<PathBuf>,
    /// The folders commands may read (`JUNEES_SANDBOX_READ`); `None`:
    /// everything.
    pub read: Option<Vec<PathBuf>>,
    /// Whether commands may use TCP (`JUNEES_SANDBOX_NETWORK`, on by
    /// default).
    pub network: bool,
    /// The temporary folders commands may write in.
    pub temp: Vec<PathBuf>,
    /// A scheduled job or a webhook run: no one watches, so an unset mode
    /// means `require`.
    pub unattended: bool,
}

impl Settings {
    /// Reads the `JUNEES_SANDBOX*` settings and `TMPDIR` through `get`
    /// (values trimmed, empty ones absent).
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mode = get(MODE_ENV).map(|raw| Mode::parse(&raw)).transpose()?;
        let home = get("HOME").map(PathBuf::from);
        let write = paths(get(WRITE_ENV), WRITE_ENV, home.as_deref())?;
        let read = get(READ_ENV)
            .map(|raw| paths(Some(raw), READ_ENV, home.as_deref()))
            .transpose()?;
        let network = match get(NETWORK_ENV) {
            None => true,
            raw => crate::config::parse_flag(raw, NETWORK_ENV)?,
        };
        let mut temp: Vec<PathBuf> = TEMP_FOLDERS.iter().map(PathBuf::from).collect();
        if let Some(dir) = get("TMPDIR").map(PathBuf::from)
            && dir.is_absolute()
        {
            temp.push(dir);
        }
        Ok(Self {
            mode,
            write,
            read,
            network,
            temp,
            unattended: false,
        })
    }

    /// The mode in effect.
    pub fn mode(&self) -> Mode {
        self.mode.unwrap_or(if self.unattended {
            Mode::Require
        } else {
            Mode::Auto
        })
    }
}

/// A comma-separated list of absolute paths; `~` and `~/...` start at
/// `home`.
fn paths(raw: Option<String>, name: &str, home: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for item in raw.iter().flat_map(|raw| raw.split(',')).map(str::trim) {
        if item.is_empty() {
            continue;
        }
        let path = match (item.strip_prefix('~'), home) {
            (Some(""), Some(home)) => home.to_path_buf(),
            (Some(rest), Some(home)) if rest.starts_with('/') => {
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(item),
        };
        if !path.is_absolute() {
            bail!("{name} takes absolute paths (or ~/...), got `{item}`");
        }
        out.push(path);
    }
    Ok(out)
}

/// Whether the kernel can confine commands: its Landlock ABI version, or
/// why it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Abi(u32),
    Unavailable(String),
}

impl Support {
    /// The running kernel's (asked once per process).
    pub fn current() -> Self {
        static CURRENT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
        CURRENT.get_or_init(probe_kernel).clone()
    }

    /// What it can restrict, for messages.
    pub fn describe(&self) -> String {
        match self {
            Support::Abi(abi) if *abi >= NETWORK_ABI => {
                format!("Landlock ABI {abi} (files and TCP)")
            }
            Support::Abi(abi) => format!("Landlock ABI {abi} (files only; TCP needs ABI 4)"),
            Support::Unavailable(why) => why.clone(),
        }
    }
}

/// The first ABI version that can restrict TCP.
pub const NETWORK_ABI: u32 = 4;

#[cfg(target_os = "linux")]
fn probe_kernel() -> Support {
    match landlock::abi() {
        Ok(abi) => Support::Abi(abi),
        Err(why) => Support::Unavailable(why),
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_kernel() -> Support {
    Support::Unavailable("Landlock needs Linux".into())
}

/// The confinement of the commands of one bash tool.
pub struct Sandbox {
    settings: Settings,
    /// Paths commands may neither read nor change.
    hidden: Vec<PathBuf>,
    /// Paths commands may read but not change.
    read_only: Vec<PathBuf>,
    /// The kernel's support; tests put a fake one here.
    support: Option<Support>,
    /// Whether a result already said the commands run unconfined.
    told: AtomicBool,
}

impl Sandbox {
    /// The sandbox of a bash tool with `tools`, or `None` when it has no
    /// sandbox settings or the mode is `off`.
    pub fn for_tools(tools: &crate::tools::ToolSettings) -> Option<Self> {
        let settings = tools.sandbox.clone()?;
        if settings.mode() == Mode::Off {
            return None;
        }
        let mut hidden = Vec::new();
        let mut read_only = Vec::new();
        if let Some(file) = &tools.settings_file {
            hidden.extend(file.protected_paths());
            if tools.webhook {
                hidden.extend(file.resolved_credentials());
            }
            read_only.extend(file.data_folders());
        }
        Some(Self::new(settings, hidden, read_only))
    }

    /// A sandbox with `settings` that keeps commands from reading or
    /// changing `hidden` and from changing `read_only`.
    pub fn new(settings: Settings, hidden: Vec<PathBuf>, read_only: Vec<PathBuf>) -> Self {
        Self {
            settings,
            hidden,
            read_only,
            support: None,
            told: AtomicBool::new(false),
        }
    }

    /// Uses `support` instead of asking the kernel (tests).
    pub fn with_support(mut self, support: Support) -> Self {
        self.support = Some(support);
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn support(&self) -> Support {
        self.support.clone().unwrap_or_else(Support::current)
    }

    /// The confinement for a command run in `work`: what to apply before
    /// spawning it, or why it must not run.
    pub fn prepare(&self, work: &Path) -> Prepared {
        let mode = self.settings.mode();
        if mode == Mode::Off {
            return Prepared::Unconfined(None);
        }
        let abi = match self.support() {
            Support::Abi(abi) => abi,
            Support::Unavailable(why) => {
                return match mode {
                    Mode::Require => Prepared::Refused(format!(
                        "the sandbox is required (sandbox = require) but {why}"
                    )),
                    _ => Prepared::Unconfined(self.tell_once(format!(
                        "[sandbox: {why}; commands run without it (sandbox = auto)]"
                    ))),
                };
            }
        };
        let mut note = None;
        if !self.settings.network && abi < NETWORK_ABI {
            let why = format!(
                "sandbox_network = 0 needs Landlock ABI {NETWORK_ABI}, the kernel has {abi}"
            );
            if mode == Mode::Require {
                return Prepared::Refused(format!(
                    "the sandbox is required (sandbox = require) but {why}"
                ));
            }
            note = self.tell_once(format!("[sandbox: {why}; TCP is not restricted]"));
        }
        match self.build(abi, work) {
            Ok(confinement) => Prepared::Confined(confinement, note),
            Err(err) => Prepared::Refused(format!("cannot set up the sandbox: {err}")),
        }
    }

    /// `note` the first time only.
    fn tell_once(&self, note: String) -> Option<String> {
        (!self.told.swap(true, Ordering::Relaxed)).then_some(note)
    }

    #[cfg(target_os = "linux")]
    fn build(&self, abi: u32, work: &Path) -> io::Result<Confinement> {
        use landlock::{READ, Ruleset, WRITE, WRITE_EXISTING, real_path};
        let ruleset = Ruleset::new(abi, !self.settings.network)?;
        let hidden: Vec<PathBuf> = self.hidden.iter().map(|path| real_path(path)).collect();
        let no_write: Vec<PathBuf> = hidden
            .iter()
            .cloned()
            .chain(self.read_only.iter().map(|path| real_path(path)))
            .collect();
        let mut writable = vec![real_path(work)];
        for path in self.settings.temp.iter().chain(&self.settings.write) {
            writable.push(real_path(path));
        }
        for path in &writable {
            ruleset.grant_except(path, READ, &hidden)?;
            ruleset.grant_except(path, WRITE, &no_write)?;
        }
        ruleset.grant_except(Path::new("/dev"), WRITE_EXISTING | READ, &hidden)?;
        match &self.settings.read {
            None => ruleset.grant_except(Path::new("/"), READ, &hidden)?,
            Some(paths) => {
                for path in paths {
                    ruleset.grant_except(&real_path(path), READ, &hidden)?;
                }
            }
        }
        Ok(Confinement { ruleset })
    }

    #[cfg(not(target_os = "linux"))]
    fn build(&self, _abi: u32, _work: &Path) -> io::Result<Confinement> {
        Err(io::Error::other("Landlock needs Linux"))
    }
}

/// What [`Sandbox::prepare`] decided for one command.
pub enum Prepared {
    /// Apply this before spawning it; the note goes into its result.
    Confined(Confinement, Option<String>),
    /// Run it as it is; the note goes into its result.
    Unconfined(Option<String>),
    /// Do not run it, for this reason.
    Refused(String),
}

/// A ruleset ready to be applied to a child process. Keep it until the
/// child has been spawned.
pub struct Confinement {
    #[cfg(target_os = "linux")]
    ruleset: landlock::Ruleset,
}

impl Confinement {
    /// Makes `command`'s process put itself under the ruleset (with
    /// `no_new_privs`) right before it runs the program. Spawning fails
    /// when that fails.
    pub fn apply(&self, command: &mut tokio::process::Command) {
        #[cfg(target_os = "linux")]
        {
            let fd = self.ruleset.raw_fd();
            // SAFETY: the closure runs in the child between fork and exec
            // and only makes the two system calls of `restrict_self`; the
            // descriptor stays open in the parent until spawning is done
            // (the caller keeps `self`), so the child has it too.
            unsafe {
                command.pre_exec(move || landlock::restrict_self(fd));
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }
}
