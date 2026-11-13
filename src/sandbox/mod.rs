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
//!   the secrets folder, the browser profiles
//!   ([`crate::browser::PROFILES_FOLDER`]: cookies and logins of a
//!   running browser) and, in a webhook run, the git, GitHub CLI and
//!   Claude Code credentials and the saved sessions (tool output of
//!   earlier conversations);
//! - never write in Junees's data folders (memory, skills, cron, ...)
//!   or the saved sessions;
//! - in an unattended run, not read the credentials in the home folder
//!   either ([`unattended_credentials`]), nor change the files there that
//!   run later (shell startup files, user services, git's and cargo's
//!   settings, `~/.ssh` but the known hosts: [`unattended_no_write`];
//!   program folders on PATH: [`program_folders`]), even when the working
//!   folder is the home folder;
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
pub const WEBHOOK_READ_ENV: &str = "JUNEES_WEBHOOK_SANDBOX_READ";
pub const NETWORK_ENV: &str = "JUNEES_SANDBOX_NETWORK";

/// The temporary folders commands may always write in, besides `$TMPDIR`.
const TEMP_FOLDERS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];

/// Variables that lead a confined command to a process outside the
/// sandbox that runs commands for it: the user's D-Bus session bus
/// (`systemd-run --user`), the user's runtime folder where it and other
/// user services listen, the ssh agent, and tmux (`tmux send-keys` into
/// another pane). Landlock does not handle connecting to a unix socket,
/// so leaving them out of a confined command's environment only stops
/// such an escape by accident: a command can still name the sockets
/// itself (docs/safety.md, the limits of the sandbox).
pub const ESCAPE_ENV_VARS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "SSH_AUTH_SOCK",
    "TMUX",
    "TMUX_PANE",
];

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
    /// The folders a webhook run's file tools may read besides the
    /// working, temporary and `write` folders
    /// (`JUNEES_WEBHOOK_SANDBOX_READ`). They take it instead of `read`,
    /// which widens the file tools of scheduled jobs only: a webhook run
    /// starts from anyone's text.
    pub webhook_read: Vec<PathBuf>,
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
        let webhook_read = paths(get(WEBHOOK_READ_ENV), WEBHOOK_READ_ENV, home.as_deref())?;
        let network = match get(NETWORK_ENV) {
            None => true,
            raw => crate::config::parse_flag(raw, NETWORK_ENV)?,
        };
        let mut temp: Vec<PathBuf> = TEMP_FOLDERS.iter().map(PathBuf::from).collect();
        // The process's own temporary folder too: the commands inherit it.
        for dir in get("TMPDIR")
            .map(PathBuf::from)
            .into_iter()
            .chain([std::env::temp_dir()])
        {
            if dir.is_absolute() && !temp.contains(&dir) {
                temp.push(dir);
            }
        }
        Ok(Self {
            mode,
            write,
            read,
            webhook_read,
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
            Support::Abi(abi) if *abi >= TRUNCATE_ABI => {
                format!("Landlock ABI {abi} (files only; TCP needs ABI {NETWORK_ABI})")
            }
            Support::Abi(abi) => format!(
                "Landlock ABI {abi} (files without truncation, which needs ABI \
                 {TRUNCATE_ABI}; TCP needs ABI {NETWORK_ABI})"
            ),
            Support::Unavailable(why) => why.clone(),
        }
    }
}

/// The first ABI version that can restrict TCP.
pub const NETWORK_ABI: u32 = 4;

/// The first ABI version that handles truncating a file (Linux 6.2).
/// Before it, a confined command can empty files it may not write
/// (`truncate -s0` on the settings file or a memory file), though not
/// read them.
pub const TRUNCATE_ABI: u32 = 3;

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
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    read_only: Vec<PathBuf>,
    /// Program folders commands may read but not change, unless one is or
    /// holds the working folder: then the working folder stays writable
    /// ([`program_folders`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    programs: Vec<PathBuf>,
    /// The kernel's support; tests put a fake one here.
    support: Option<Support>,
    /// Whether a result already said the commands run unconfined.
    told: AtomicBool,
    /// Whether a result already said the working folder holds protected
    /// paths.
    told_holds: AtomicBool,
    /// Junees's state folder, made before rules are built when missing.
    state: Option<PathBuf>,
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
        let mut state = None;
        if let Some(file) = &tools.settings_file {
            hidden.extend(file.protected_paths());
            if tools.webhook {
                hidden.extend(file.resolved_credentials());
            }
            read_only.extend(file.data_folders());
            // Saved sessions too: `-c` and `--resume` read them back as
            // the user's own conversation. A webhook run (anyone's text)
            // may not read them either: they hold the tool output of
            // earlier conversations, secrets included.
            state = file.folder();
            let sessions = state.as_deref().map(crate::session::sessions_dir);
            if tools.webhook {
                hidden.extend(sessions);
                // The old content of files other runs changed.
                if let Some(state) = &state {
                    let checkpoints = state.join(crate::checkpoints::FOLDER);
                    read_only.retain(|path| *path != checkpoints);
                    hidden.push(checkpoints);
                }
            } else {
                read_only.extend(sessions);
            }
        }
        let mut programs = Vec::new();
        if settings.unattended {
            hidden.extend(unattended_credentials(tools, &settings));
            read_only.extend(unattended_no_write(tools));
            programs = program_folders(tools, std::env::var_os("PATH").as_deref());
        }
        let mut sandbox = Self::new(settings, hidden, read_only).with_programs(programs);
        sandbox.state = state;
        Some(sandbox)
    }

    /// A sandbox with `settings` that keeps commands from reading or
    /// changing `hidden` and from changing `read_only`.
    pub fn new(settings: Settings, hidden: Vec<PathBuf>, read_only: Vec<PathBuf>) -> Self {
        Self {
            settings,
            hidden,
            read_only,
            programs: Vec::new(),
            support: None,
            told: AtomicBool::new(false),
            told_holds: AtomicBool::new(false),
            state: None,
        }
    }

    /// Also keeps commands from changing the program folders `programs`,
    /// except one that is or holds the working folder (a scheduled job in
    /// `~/scripts`, or in `~/scripts/job`, with `~/scripts` on PATH): that
    /// would leave the job nothing to write.
    pub fn with_programs(mut self, programs: Vec<PathBuf>) -> Self {
        self.programs = programs;
        self
    }

    /// Uses `support` instead of asking the kernel (tests).
    pub fn with_support(mut self, support: Support) -> Self {
        self.support = Some(support);
        self
    }

    /// Also lets commands write in `paths`.
    pub fn with_write(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        self.settings.write.extend(paths);
        self
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn support(&self) -> Support {
        self.support.clone().unwrap_or_else(Support::current)
    }

    /// The confinement for a command run in `work`: what to apply before
    /// spawning it, or why it must not run. Makes a missing state folder
    /// first ([`Self::make_state_folder`]).
    pub fn prepare(&self, work: &Path) -> Prepared {
        self.prepare_in(work, true)
    }

    /// [`Self::prepare`], making the state folder only when `make_state`.
    fn prepare_in(&self, work: &Path, make_state: bool) -> Prepared {
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
        let mut gaps = Vec::new();
        if abi < TRUNCATE_ABI {
            gaps.push((
                format!(
                    "the kernel's Landlock ABI {abi} cannot stop commands from emptying \
                     (truncating) files; that needs ABI {TRUNCATE_ABI} (Linux 6.2)"
                ),
                "the settings file and the data folders can be emptied, not read",
            ));
        }
        if !self.settings.network && abi < NETWORK_ABI {
            gaps.push((
                format!(
                    "sandbox_network = 0 needs Landlock ABI {NETWORK_ABI}, the kernel has {abi}"
                ),
                "TCP is not restricted",
            ));
        }
        if let Some((why, _)) = gaps.first()
            && mode == Mode::Require
        {
            return Prepared::Refused(format!(
                "the sandbox is required (sandbox = require) but {why}"
            ));
        }
        let lines: Vec<String> = gaps
            .iter()
            .map(|(why, left)| format!("[sandbox: {why}; {left}]"))
            .collect();
        let mut note = if lines.is_empty() {
            None
        } else {
            self.tell_once(lines.join("\n"))
        };
        if make_state {
            self.make_state_folder();
        }
        match self.build(abi, work) {
            Ok((confinement, holds)) => {
                if holds && !self.told_holds.swap(true, Ordering::Relaxed) {
                    let line = "[sandbox: the working folder holds Junees's state folder or \
                                other protected paths, so commands cannot create or remove \
                                entries directly in the working folder or the folders between \
                                it and the protected files; work in a subfolder]";
                    note = Some(match note {
                        Some(note) => format!("{note}\n{line}"),
                        None => line.to_string(),
                    });
                }
                Prepared::Confined(confinement, note)
            }
            Err(err) => Prepared::Refused(format!("cannot set up the sandbox: {err}")),
        }
    }

    /// Makes the state folder (0700) when it is missing. Rules protect
    /// only the paths whose folder exists, so without it a confined
    /// command could create `mcp.json` or `config.toml` there for the next
    /// run to read.
    fn make_state_folder(&self) {
        use std::os::unix::fs::DirBuilderExt;
        if let Some(dir) = &self.state
            && std::fs::symlink_metadata(dir).is_err()
        {
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir);
        }
    }

    /// `note` the first time only.
    fn tell_once(&self, note: String) -> Option<String> {
        (!self.told.swap(true, Ordering::Relaxed)).then_some(note)
    }

    /// The ruleset for a command in `work`, and whether a writable folder
    /// holds a protected path (so that folder gets rules on its entries
    /// only, [`landlock::Ruleset::grant_except`]).
    #[cfg(target_os = "linux")]
    fn build(&self, abi: u32, work: &Path) -> io::Result<(Confinement, bool)> {
        use landlock::{READ, Ruleset, WRITE, WRITE_EXISTING, real_path};
        use std::path::Component;
        let ruleset = Ruleset::new(abi, !self.settings.network)?;
        // Paths in a folder that does not exist (no state folder yet) need
        // no exception: leaving them out keeps the folders above them
        // whole. Each path counts both resolved and as written (made
        // absolute): with a link on the way (`~/.junees -> /data/state`)
        // the resolved path lies elsewhere, but Junees reads through the
        // link, so the link entry must not be replaceable either.
        let real = |paths: &[PathBuf]| -> Vec<PathBuf> {
            let mut out = Vec::new();
            for path in paths {
                let written = std::path::absolute(path)
                    .ok()
                    .filter(|path| !path.components().any(|c| c == Component::ParentDir));
                for path in std::iter::once(real_path(path)).chain(written) {
                    if path.parent().is_some_and(Path::is_dir) && !out.contains(&path) {
                        out.push(path);
                    }
                }
            }
            out
        };
        let hidden = real(&self.hidden);
        let work = real_path(work);
        // A program folder that is or holds the working folder is left out
        // of the working folder's rules only: the other writable folders
        // (`sandbox_write`) still may not change it.
        let (around_work, programs): (Vec<PathBuf>, Vec<PathBuf>) = real(&self.programs)
            .into_iter()
            .partition(|path| work.starts_with(path));
        let work_no_write: Vec<PathBuf> = hidden
            .iter()
            .cloned()
            .chain(real(&self.read_only))
            .chain(programs)
            .collect();
        let no_write: Vec<PathBuf> = work_no_write.iter().cloned().chain(around_work).collect();
        let holds = work_no_write.iter().any(|path| path.starts_with(&work));
        ruleset.grant_except(&work, READ, &hidden)?;
        ruleset.grant_except(&work, WRITE, &work_no_write)?;
        for path in self.settings.temp.iter().chain(&self.settings.write) {
            let path = real_path(path);
            ruleset.grant_except(&path, READ, &hidden)?;
            ruleset.grant_except(&path, WRITE, &no_write)?;
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
        Ok((Confinement { ruleset }, holds))
    }

    #[cfg(not(target_os = "linux"))]
    fn build(&self, _abi: u32, _work: &Path) -> io::Result<(Confinement, bool)> {
        Err(io::Error::other("Landlock needs Linux"))
    }
}

/// The files in `~/.ssh` a scheduled job's commands may still read: public
/// keys (`*.pub`) and what ssh reads besides the private key, so `git`
/// over ssh can check hosts and use its settings.
pub const SSH_PUBLIC: &[&str] = &[
    "known_hosts",
    "known_hosts.old",
    "known_hosts2",
    "authorized_keys",
    "authorized_keys2",
    "config",
];

/// The credentials in the home folder an unattended run's commands may
/// not read ([`crate::tools::UNATTENDED_CREDENTIALS`], the same list the
/// file tools refuse), as given; the rules cover each resolved too.
///
/// A webhook run (anyone's text) gets all of them. A scheduled job keeps
/// the public files of `~/.ssh` ([`SSH_PUBLIC`]) and loses only the rest
/// (private keys, whatever their name), since the user's own job may
/// pull or push over ssh; and the user can let a job's commands read a
/// credential by naming it, or a path in it, in `sandbox_read` (for
/// example `sandbox_read = "/, ~/.ssh"` keeps reading everything and adds
/// the ssh keys). Naming a folder above it (`/`, `~`) does not count.
fn unattended_credentials(tools: &crate::tools::ToolSettings, settings: &Settings) -> Vec<PathBuf> {
    let Some(home) = tools.settings_file.as_ref().and_then(|file| file.home()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in crate::tools::UNATTENDED_CREDENTIALS {
        let root = home.join(name);
        if tools.webhook {
            out.push(root);
            continue;
        }
        let named: Vec<&PathBuf> = settings
            .read
            .iter()
            .flatten()
            .filter(|read| within(read, &root))
            .collect();
        let paths = if *name == ".ssh" {
            ssh_private(&root)
        } else {
            vec![root]
        };
        out.extend(paths.into_iter().filter(|path| {
            !named
                .iter()
                .any(|read| within(path, read) || within(read, path))
        }));
    }
    out
}

/// The paths in the home folder an unattended run's commands may read
/// but not change ([`crate::tools::unattended_no_write`], the same list
/// the file tools refuse to write): what they hold runs later, outside
/// the run and its sandbox. Landlock rules only allow, and a path in a
/// folder that does not exist gets no exception ([`Sandbox::build`]), so
/// a path whose folder is missing counts from its first missing folder
/// below the home folder instead: with no `~/.config`, a command may not
/// make `~/.config` (and in it `systemd/user/x.service`) at all.
fn unattended_no_write(tools: &crate::tools::ToolSettings) -> Vec<PathBuf> {
    let Some(home) = tools.settings_file.as_ref().and_then(|file| file.home()) else {
        return Vec::new();
    };
    let config = std::env::var_os("XDG_CONFIG_HOME");
    from_first_missing(
        crate::tools::unattended_no_write(home, config.as_deref()),
        home,
    )
}

/// The program folders in the home folder an unattended run's commands
/// may not change ([`crate::tools::program_folders`] with `path_var` the
/// PATH), from their first missing folder like [`unattended_no_write`].
fn program_folders(
    tools: &crate::tools::ToolSettings,
    path_var: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let Some(home) = tools.settings_file.as_ref().and_then(|file| file.home()) else {
        return Vec::new();
    };
    from_first_missing(crate::tools::program_folders(home, path_var), home)
}

/// `paths` each from its first missing folder below `home`
/// ([`first_missing`]), sorted and without repeats.
fn from_first_missing(paths: Vec<PathBuf>, home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = paths
        .into_iter()
        .map(|path| first_missing(&path, home))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `path` itself when its folder exists, else the first folder on the way
/// from `home` to it that does not (never `home` itself).
fn first_missing(path: &Path, home: &Path) -> PathBuf {
    let mut path = path;
    while let Some(parent) = path.parent()
        && !parent.is_dir()
        && parent != home
        && parent.starts_with(home)
    {
        path = parent;
    }
    path.to_path_buf()
}

/// The entries of the ssh folder `dir` that are not in [`SSH_PUBLIC`] or
/// public keys; the folder itself when it cannot be listed.
fn ssh_private(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![dir.to_path_buf()];
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| {
            let name = name.to_string_lossy();
            !(name.ends_with(".pub") || SSH_PUBLIC.contains(&name.as_ref()))
        })
        .map(|name| dir.join(name))
        .collect()
}

/// Whether `path` is `folder` or inside it, as given or resolved.
fn within(path: &Path, folder: &Path) -> bool {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(folder) || real(path).starts_with(real(folder))
}

/// What a confined test command showed ([`Sandbox::try_child`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trial {
    /// It ran confined, and could not open the protected file (if any
    /// existed to try).
    Confined { tried: Option<PathBuf> },
    /// It ran without the sandbox (`auto` on a kernel without Landlock).
    Unconfined,
    /// The sandbox would refuse commands, or did not hold: why.
    Failed(String),
}

impl Sandbox {
    /// Runs a tiny command (`/bin/sh`, empty environment) the way bash's
    /// commands in `work` would run, then one that tries to open the
    /// first protected file that exists for reading, which must fail.
    /// Writes nothing, a missing state folder included.
    pub fn try_child(&self, work: &Path) -> Trial {
        let confinement = match self.prepare_in(work, false) {
            Prepared::Confined(confinement, _) => confinement,
            Prepared::Unconfined(_) => return Trial::Unconfined,
            Prepared::Refused(reason) => return Trial::Failed(reason),
        };
        let run = |script: &str, arg: Option<&Path>| {
            let mut command = std::process::Command::new("/bin/sh");
            command
                .args(["-c", script, "sh"])
                .args(arg)
                .env_clear()
                .current_dir(work)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            confinement.apply_std(&mut command);
            command.status()
        };
        match run("exit 0", None) {
            Ok(status) if status.success() => {}
            Ok(status) => {
                return Trial::Failed(format!("a confined `sh -c 'exit 0'` ended with {status}"));
            }
            Err(err) => return Trial::Failed(format!("cannot start a confined `sh`: {err}")),
        }
        let Some(file) = self.hidden.iter().find(|path| path.is_file()) else {
            return Trial::Confined { tried: None };
        };
        match run("exec < \"$1\"", Some(file)) {
            Ok(status) if status.success() => {
                Trial::Failed(format!("a confined `sh` could read {}", file.display()))
            }
            Ok(_) => Trial::Confined {
                tried: Some(file.clone()),
            },
            Err(err) => Trial::Failed(format!("cannot start a confined `sh`: {err}")),
        }
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

/// Makes the calling process the leader of a session of its own
/// (`setsid`), unless it leads one already. A new session is also a
/// process group of its own whose id is the pid (so killing the group
/// still works), and has no controlling terminal: the process cannot
/// open Junees's terminal through `/dev/tty`, to push keys into it
/// (`TIOCSTI`, on kernels that still allow it) or to draw over its
/// approval questions. For a child between fork and exec: it only makes
/// system calls. Fails when the process leads a process group but no
/// session (`process_group(0)`): never combine the two.
pub(crate) fn start_session() -> io::Result<()> {
    // SAFETY: getsid, getpid and setsid are plain system calls.
    unsafe {
        if libc::getsid(0) == libc::getpid() {
            return Ok(());
        }
        if libc::setsid() == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl Confinement {
    /// Makes `command`'s process start a session of its own
    /// ([`start_session`]) and put itself under the ruleset (with
    /// `no_new_privs`) right before it runs the program, and takes the
    /// [`ESCAPE_ENV_VARS`] out of its environment. Spawning fails when
    /// either cannot be done. Every confined child (bash, the flow's test
    /// command, the harness's git, `doctor`'s trial run) goes through
    /// here, so none of them keeps Junees's terminal.
    pub fn apply(&self, command: &mut tokio::process::Command) {
        for name in ESCAPE_ENV_VARS {
            command.env_remove(name);
        }
        #[cfg(target_os = "linux")]
        {
            let fd = self.ruleset.raw_fd();
            // SAFETY: the closure runs in the child between fork and exec
            // and only makes the system calls of `start_session` and
            // `restrict_self`; the descriptor stays open in the parent
            // until spawning is done (the caller keeps `self`), so the
            // child has it too.
            unsafe {
                command.pre_exec(move || {
                    start_session()?;
                    landlock::restrict_self(fd)
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }

    /// [`Self::apply`] for a std command.
    pub fn apply_std(&self, command: &mut std::process::Command) {
        for name in ESCAPE_ENV_VARS {
            command.env_remove(name);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            let fd = self.ruleset.raw_fd();
            // SAFETY: as in `apply`.
            unsafe {
                command.pre_exec(move || {
                    start_session()?;
                    landlock::restrict_self(fd)
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }
}
