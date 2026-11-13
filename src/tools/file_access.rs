//! Where read_file, write_file and edit_file may go, checked on what they
//! really open.
//!
//! The file tools run inside the Junees process, outside the Landlock
//! sandbox of bash ([`crate::sandbox`]), so:
//! - They never open anything under `/proc`, `/sys` or `/dev`
//!   ([`SYSTEM_FOLDERS`]), also through a link: `/proc/self/environ` holds
//!   the API key and the gateway tokens bash never sees.
//! - They check the file they opened, not the name they were given, so a
//!   symbolic link swapped between a check and the open cannot lead them
//!   elsewhere. Reading opens the path, then asks the kernel where the open
//!   file is (`/proc/self/fd`, `F_GETPATH` on macOS) and checks that.
//!   Writing opens the folder (one component at a time, creating missing
//!   ones only inside allowed folders), checks where that folder is, and
//!   works relative to it: a link in the last component is followed by
//!   hand and checked again, and the new content goes to a new temporary
//!   file (`O_CREAT|O_EXCL|O_NOFOLLOW`) renamed over the name (`renameat`).
//! - In an unattended run (a scheduled job, a webhook run or a GitHub flow
//!   step) no one is there to approve paths outside the working folder,
//!   so a [`Scope`] limits them: writes to the working folder, the
//!   temporary folders and the `sandbox_write` folders; reads to those and
//!   the `sandbox_read` folders; and neither to the credential files and
//!   folders in the home folder ([`UNATTENDED_CREDENTIALS`]), the private
//!   Junees paths or, in a webhook run, the saved sessions, even inside
//!   those folders. Nor do they change the files that run or configure
//!   programs later ([`unattended_no_write`]: shell startup files, user
//!   services, git's settings, program folders on PATH) when the working
//!   folder holds them.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{SettingsFile, ToolSettings};

/// Folders the file tools never open, after links are resolved: process
/// state (`/proc/self/environ`, `/proc/self/mem`), kernel settings and
/// devices.
pub const SYSTEM_FOLDERS: &[&str] = &["/proc", "/sys", "/dev"];

/// Credential files and folders under the home folder that the file tools
/// of an unattended run neither read nor change, even when the working
/// folder holds them (a job working in the home folder).
pub const UNATTENDED_CREDENTIALS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".config/git/credentials",
    ".git-credentials",
    ".gnupg",
    ".password-store",
    ".local/share/keyrings",
    ".claude",
    ".claude.json",
    ".docker",
    ".kube",
    ".netrc",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
];

/// Files and folders under the home folder that the file tools of an
/// unattended run do not change (they may read them), even when the
/// working folder holds them: what they hold runs later, outside the run
/// and its sandbox. Shell startup files, user services and session
/// autostart entries, git's own settings (`core.hooksPath`,
/// `core.fsmonitor`, aliases), cargo's (`build.rustc-wrapper`,
/// `target.<triple>.runner` run on the next build), what ssh and sshd run
/// or read for a login (`config` with `ProxyCommand`/`Match exec`, `rc`,
/// `environment`, the authorized keys). [`unattended_no_write`] adds
/// more at run time, and [`program_folders`] lists the program folders.
pub const UNATTENDED_NO_WRITE: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".bash_aliases",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".config/fish",
    ".pam_environment",
    ".xprofile",
    ".xsessionrc",
    ".config/systemd",
    ".local/share/systemd",
    ".config/environment.d",
    ".config/autostart",
    ".gitconfig",
    ".config/git",
    ".cargo/config.toml",
    ".cargo/config",
    ".ssh/config",
    ".ssh/rc",
    ".ssh/environment",
    ".ssh/authorized_keys",
    ".ssh/authorized_keys2",
];

/// The user's program folders that a login shell puts on PATH (Debian's
/// and Ubuntu's `~/.profile` adds both when they exist), besides the ones
/// on the run's PATH ([`program_folders`]).
const PROGRAM_FOLDERS: &[&str] = &[".local/bin", "bin"];

/// The entries of `~/.ssh` an unattended run may still change: ssh adds
/// the hosts it meets to them (`StrictHostKeyChecking accept-new`, a job
/// pushing over ssh for the first time), and they hold host keys, never
/// commands or settings.
const SSH_WRITABLE: &[&str] = &["known_hosts", "known_hosts.old", "known_hosts2"];

/// The entries of [`UNATTENDED_NO_WRITE`] in `~/.config`, which programs
/// look for in `$XDG_CONFIG_HOME` instead when it is set.
const XDG_CONFIG_NO_WRITE: &[&str] = &["fish", "systemd", "environment.d", "autostart", "git"];

/// The paths under `home` an unattended run may not change, as written:
/// [`UNATTENDED_NO_WRITE`], the same folders of `~/.config` in
/// `xdg_config` (`$XDG_CONFIG_HOME`) when that is a plainly named folder
/// strictly inside `home` (not `home` itself, where `~/git` or `~/fish`
/// would be refused, and with them every working folder in one), and
/// every other entry of `~/.ssh` but
/// [`SSH_WRITABLE`] (ssh's `config` may `Include` a file there or run a
/// script there as `ProxyCommand`, and sshd reads any
/// `authorized_keys*` its settings name). The program folders are apart
/// ([`program_folders`]).
pub fn unattended_no_write(home: &Path, xdg_config: Option<&OsStr>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = UNATTENDED_NO_WRITE
        .iter()
        .map(|name| home.join(name))
        .collect();
    if let Some(config) = xdg_config.map(Path::new).filter(|config| {
        config.is_absolute()
            && !config.components().any(|c| c == Component::ParentDir)
            && within_home(config, home)
            && !same_folder(config, home)
    }) {
        out.extend(XDG_CONFIG_NO_WRITE.iter().map(|name| config.join(name)));
    }
    if let Ok(entries) = std::fs::read_dir(home.join(".ssh")) {
        out.extend(
            entries
                .filter_map(Result::ok)
                .filter(|entry| !SSH_WRITABLE.iter().any(|name| entry.file_name() == *name))
                .map(|entry| entry.path()),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// The program folders under `home` an unattended run may not change, as
/// written: [`PROGRAM_FOLDERS`] and the folders of `path_var` (a PATH
/// value) inside `home`, since the next shell runs what lands there
/// instead of the system's program. The home folder itself is left out
/// when PATH names it: protecting it would leave a run working anywhere
/// in the home folder unable to write. A run whose working folder is (or
/// is inside) one of them still writes in its working folder: the sandbox
/// and the file tools leave that program folder out there, and
/// [`unattended_folder_warning`] tells when the working folder is a
/// program folder itself.
pub fn program_folders(home: &Path, path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = PROGRAM_FOLDERS.iter().map(|name| home.join(name)).collect();
    let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    for dir in path_var.into_iter().flat_map(std::env::split_paths) {
        if !dir.is_absolute() || dir.components().any(|c| c == Component::ParentDir) {
            continue;
        }
        let real = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let is_home = |home: &Path| dir == home || real == home;
        if within_home(&dir, home) && !is_home(home) && !is_home(&real_home) {
            out.push(dir);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Whether `path` is `home` or inside it, as written or with links
/// resolved (either side).
fn within_home(path: &Path, home: &Path) -> bool {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (real_path, real_home) = (real(path), real(home));
    [path, real_path.as_path()]
        .iter()
        .any(|path| path.starts_with(home) || path.starts_with(&real_home))
}

/// Whether `a` and `b` are the same folder, as written or with links
/// resolved.
fn same_folder(a: &Path, b: &Path) -> bool {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    a == b || real(a) == real(b)
}

/// Whether `dir` is the home folder `home` or a folder holding it (`/`,
/// `/home`), as written or with links resolved; `/` also when the home
/// folder is not known. An unattended run working there may change
/// everything in the home folder but the protected paths, and the files
/// that run code later cannot all be listed (python's `.pth` files,
/// editor settings, other repositories' git hooks), so scheduled jobs and
/// webhook routes refuse such a folder unless added with `--allow-home`.
pub fn holds_home(dir: &Path, home: Option<&Path>) -> bool {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let real_dir = real(dir);
    if dir.parent().is_none() || real_dir.parent().is_none() {
        return true;
    }
    home.is_some_and(|home| {
        let real_home = real(home);
        [dir, real_dir.as_path()]
            .iter()
            .any(|dir| home.starts_with(dir) || real_home.starts_with(dir))
    })
}

/// Why the working folder `dir` of an unattended run is a risky choice,
/// if it is: the home folder itself, a folder holding Junees's state
/// folder `state` or the home folder ([`holds_home`]; refused unless the
/// job or route allows it), or a program folder ([`program_folders`]
/// with `path_var` the PATH). The run's tools may write anywhere in the
/// working folder; the protected paths stay protected, but everything
/// else there (other projects, dotfiles not listed) does not, and in a
/// program folder that includes the programs the next shell runs.
pub fn unattended_folder_warning(
    dir: &Path,
    home: Option<&Path>,
    state: Option<&Path>,
    path_var: Option<&OsStr>,
) -> Option<String> {
    let real = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let written = dir;
    let dir = real(dir);
    let what = if home.is_some_and(|home| real(home) == dir) {
        "the home folder"
    } else if state.is_some_and(|state| real(state).starts_with(&dir)) {
        "a folder holding Junees's state folder"
    } else if holds_home(written, home) {
        "a folder holding the home folder"
    } else if home.is_some_and(|home| {
        program_folders(home, path_var)
            .iter()
            .any(|folder| real(folder) == dir)
    }) {
        "a program folder on PATH, so the programs it writes there run in later shells"
    } else {
        return None;
    };
    Some(format!(
        "this unattended run works in {} ({what}): its tools may change everything there \
         except the protected paths (credentials, Junees's settings and data, shell startup \
         files); give the job or route a project folder instead",
        dir.display()
    ))
}

/// How many symbolic links a write follows before giving up, like the
/// kernel.
const MAX_LINKS: usize = 40;

/// Folders are opened only to work in them: on Linux `O_PATH`, which needs
/// no read permission (a 0711 `/home`).
#[cfg(target_os = "linux")]
const FOLDER_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY;
#[cfg(not(target_os = "linux"))]
const FOLDER_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;

/// What a file is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    Read,
    Write,
}

/// Why a file tool cannot use a path.
#[derive(Debug)]
pub enum Refusal {
    /// A rule refuses it; the whole message for the model.
    Denied(String),
    /// It is a folder, a device or another kind of file.
    NotRegular,
    /// The filesystem said no.
    Io(io::Error),
}

impl From<io::Error> for Refusal {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl Refusal {
    /// The error for the model about `path`; `verb` says what failed
    /// (`read`, `write`, `edit`).
    pub fn message(&self, path: &str, verb: &str) -> String {
        match self {
            Self::Denied(message) => message.clone(),
            Self::NotRegular => format!("{path} is not a regular file"),
            Self::Io(err) => format!("cannot {verb} {path}: {err}"),
        }
    }
}

/// The rules of one file tool.
#[derive(Debug, Clone, Default)]
pub struct FileAccess {
    settings_file: Option<SettingsFile>,
    scope: Option<Scope>,
}

impl FileAccess {
    /// The rules for tools with `settings`: the settings file they refuse,
    /// and in an unattended run its [`Scope`].
    pub fn new(settings: &ToolSettings) -> Self {
        Self {
            settings_file: settings.settings_file.clone(),
            scope: Scope::unattended(settings),
        }
    }

    /// Limits the tools to `scope` (tests).
    #[cfg(test)]
    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn settings_file(&self) -> Option<&SettingsFile> {
        self.settings_file.as_ref()
    }

    /// Whether `real`, a resolved absolute path, may be used: never in
    /// [`SYSTEM_FOLDERS`], never the settings file and the paths protected
    /// with it, for writes never a Junees data folder, and within the
    /// unattended [`Scope`]. `shown` is the path as the model gave it.
    fn check(&self, real: &Path, shown: &str, use_: Use) -> Result<(), Refusal> {
        system_check(real, shown)?;
        if let Some(file) = &self.settings_file {
            if file.is(real) {
                return Err(Refusal::Denied(file.refusal(shown)));
            }
            if use_ == Use::Write
                && let Some(folder) = file.data_folder(real)
            {
                return Err(Refusal::Denied(folder.refusal(shown)));
            }
        }
        if let Some(scope) = &self.scope {
            scope.check(real, shown, use_)?;
        }
        Ok(())
    }

    /// Opens the regular file at `path` for reading and checks where the
    /// opened file is.
    pub fn open_read(&self, path: &str) -> Result<Opened, Refusal> {
        let given = std::path::absolute(path)?;
        // Before opening anything: opening some devices does something.
        system_check(&given, path)?;
        if let Ok(real) = std::fs::canonicalize(&given) {
            system_check(&real, path)?;
        }
        let file = open_path(
            &given,
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY,
            0,
        )?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(Refusal::NotRegular);
        }
        let real = fd_path(&file)?;
        if meta.nlink() == 0 {
            return Err(changed(path));
        }
        self.check(&real, path, Use::Read)?;
        Ok(Opened { file, meta })
    }

    /// The place to write the file at `path`: its folder, opened and
    /// checked (with missing folders created when `create`), and its name
    /// there, links in the last component followed and checked.
    pub fn open_write(&self, path: &str, create: bool) -> Result<Target, Refusal> {
        // `name/` and `name/.` name a folder; the path components below
        // drop a trailing `.`, which would write a file `name`.
        if path.ends_with('/') || path == "." || path.ends_with("/.") {
            return Err(Refusal::NotRegular);
        }
        let given = std::path::absolute(path)?;
        system_check(&given, path)?;
        let (mut folder, mut name) = split(&given)?;
        let mut followed = 0;
        loop {
            let (dir, dir_path) = self.open_folder(&folder, create && followed == 0, path)?;
            match readlink_at(&dir, &name) {
                Ok(target) => {
                    followed += 1;
                    if followed > MAX_LINKS {
                        return Err(io::Error::from_raw_os_error(libc::ELOOP).into());
                    }
                    let next = dir_path.join(target);
                    system_check(&next, path)?;
                    (folder, name) = split(&next)?;
                }
                // Not a link (EINVAL), or nothing there yet.
                Err(err) if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOENT)) => {
                    let real = dir_path.join(&name);
                    self.check(&real, path, Use::Write)?;
                    let existing = match stat_at(&dir, &name) {
                        Ok(stat) if stat.regular => Some(stat),
                        Ok(stat) if stat.symlink => return Err(changed(path)),
                        Ok(_) => return Err(Refusal::NotRegular),
                        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
                        Err(err) => return Err(err.into()),
                    };
                    return Ok(Target {
                        dir,
                        dir_path,
                        name,
                        real,
                        existing,
                        // Another name of the same file may lie outside
                        // an unattended run's scope: there the link is
                        // broken instead.
                        links_in_place: self.scope.is_none(),
                    });
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Opens the absolute folder `folder` one component at a time, as the
    /// kernel resolves it (links followed, `..` from where a link led),
    /// and returns it with where it really is. Missing folders are made
    /// when `create`, each only after checking where it would be.
    fn open_folder(
        &self,
        folder: &Path,
        create: bool,
        shown: &str,
    ) -> Result<(File, PathBuf), Refusal> {
        let mut dir = open_path(Path::new("/"), FOLDER_FLAGS, 0)?;
        for component in folder.components() {
            match component {
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
                Component::ParentDir => dir = open_at(&dir, OsStr::new(".."), FOLDER_FLAGS, 0)?,
                Component::Normal(name) => match open_at(&dir, name, FOLDER_FLAGS, 0) {
                    Ok(next) => dir = next,
                    Err(err) if create && err.kind() == io::ErrorKind::NotFound => {
                        let here = fd_path(&dir)?;
                        self.check(&here.join(name), shown, Use::Write)?;
                        if let Err(err) = mkdir_at(&dir, name)
                            && err.kind() != io::ErrorKind::AlreadyExists
                        {
                            return Err(err.into());
                        }
                        dir = open_at(&dir, name, FOLDER_FLAGS | libc::O_NOFOLLOW, 0)?;
                    }
                    Err(err) => return Err(err.into()),
                },
            }
        }
        let real = fd_path(&dir)?;
        system_check(&real, shown)?;
        Ok((dir, real))
    }
}

/// A file opened for reading.
pub struct Opened {
    pub file: File,
    pub meta: Metadata,
}

/// Where a write goes: the name in an opened, checked folder.
pub struct Target {
    dir: File,
    dir_path: PathBuf,
    name: OsString,
    real: PathBuf,
    existing: Option<Stat>,
    /// Whether a file with other names (hard links) is rewritten in place
    /// by `write_file`, so every name sees the new content.
    links_in_place: bool,
}

impl Target {
    /// The file's resolved path.
    pub fn real(&self) -> &Path {
        &self.real
    }

    /// Opens the file there for reading, without following a link.
    pub fn open_existing(&self) -> Result<Opened, Refusal> {
        let file = open_at(
            &self.dir,
            &self.name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY,
            0,
        )?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(Refusal::NotRegular);
        }
        Ok(Opened { file, meta })
    }

    /// Puts `bytes` there: written to a new temporary file in the folder
    /// (with the old file's permissions and, when allowed, its group) and
    /// renamed over the name, so a crash or a full disk never leaves a
    /// half-written file. An existing file this process may not write
    /// (0444) is refused, as writing it directly would be.
    ///
    /// With `in_place` (write_file), an existing file is rewritten in
    /// place instead when the temporary file cannot be made (a folder
    /// without write permission) or renamed (a file bind-mounted on its
    /// own: `EBUSY`; a sticky folder: `EPERM`), when it has a single
    /// name, and when it has other names (hard links) outside an
    /// unattended run, so they all keep seeing the same file.
    ///
    /// Blocking: every error comes back from the call that hit it, and a
    /// caller on one blocking thread cannot be stopped between creating
    /// the temporary file and renaming or removing it.
    pub fn replace(&self, bytes: &[u8], in_place: bool) -> io::Result<()> {
        if let Some(stat) = &self.existing {
            writable_at(&self.dir, &self.name)?;
            if in_place && stat.nlink > 1 && self.links_in_place {
                return self.write_in_place(bytes);
            }
        }
        let single = self.existing.as_ref().is_some_and(|stat| stat.nlink == 1);
        let temp = temp_name(&self.name);
        let mode = if self.existing.is_some() {
            0o600
        } else {
            0o666
        };
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        let mut file = match open_at(&self.dir, &temp, flags, mode) {
            Ok(file) => file,
            Err(err) if in_place && err.kind() == io::ErrorKind::PermissionDenied && single => {
                return self.write_in_place(bytes);
            }
            Err(err) => {
                // The file itself may well be writable: name the folder.
                let hint = if !in_place && err.kind() == io::ErrorKind::PermissionDenied {
                    " (edit_file replaces the file through a temporary file in its \
                     directory; write_file can rewrite the file in place)"
                } else {
                    ""
                };
                return Err(io::Error::new(
                    err.kind(),
                    format!(
                        "cannot create a temporary file in {}: {err}{hint}",
                        self.dir_path.display()
                    ),
                ));
            }
        };
        // Created by this call, so it is ours to remove on any error below.
        let mut guard = RemoveOnDrop {
            dir: &self.dir,
            name: &temp,
            armed: true,
        };
        file.write_all(bytes)?;
        if let Some(stat) = &self.existing {
            // Keep the group where the owner may give it (a member of it),
            // before the mode: changing the group can clear setgid.
            let _ = chgrp(&file, stat.gid);
            // The creation mode went through the umask; set the original
            // exactly.
            file.set_permissions(std::fs::Permissions::from_mode(stat.mode & 0o7777))?;
        }
        file.sync_all()?;
        drop(file);
        if let Err(err) = rename_at(&self.dir, &temp, &self.name) {
            if in_place
                && single
                && matches!(
                    err.raw_os_error(),
                    Some(libc::EBUSY | libc::EXDEV | libc::EPERM)
                )
            {
                drop(guard);
                return self.write_in_place(bytes);
            }
            return Err(err);
        }
        guard.armed = false;
        Ok(())
    }

    /// Rewrites the existing file in place, when it is still the file
    /// checked (same inode, as many names as then).
    fn write_in_place(&self, bytes: &[u8]) -> io::Result<()> {
        let mut file = open_at(
            &self.dir,
            &self.name,
            libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY,
            0,
        )?;
        let meta = file.metadata()?;
        let same = self.existing.as_ref().is_some_and(|stat| {
            stat.dev == meta.dev() && stat.ino == meta.ino() && stat.nlink == meta.nlink()
        });
        if !meta.is_file() || !same {
            return Err(io::Error::other(
                "the file changed while it was being written; try again",
            ));
        }
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.sync_all()
    }
}

/// Where an unattended run's file tools may go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The working folder; `None`: the current folder at each call (the
    /// run moves there before its tools run).
    work: Option<PathBuf>,
    /// Folders writable (and readable) besides the working folder: the
    /// temporary folders and `sandbox_write`.
    write: Vec<PathBuf>,
    /// Folders readable besides those (`sandbox_read`; a webhook run's
    /// `webhook_sandbox_read`).
    read: Vec<PathBuf>,
    /// The setting that names `read`, for refusals.
    read_setting: &'static str,
    /// Never read nor written ([`UNATTENDED_CREDENTIALS`] in the home
    /// folder, the protected Junees paths, a webhook run's sessions).
    denied: Vec<PathBuf>,
    /// Never written ([`unattended_no_write`]).
    no_write: Vec<PathBuf>,
    /// Never written ([`program_folders`]), except a folder that is or
    /// holds the working folder, for paths in the working folder.
    programs: Vec<PathBuf>,
}

impl Scope {
    /// The scope of tools with `settings` when they belong to an
    /// unattended run (their sandbox settings say so,
    /// [`crate::cron::run::Trigger::tool_settings`]); `None` otherwise.
    pub fn unattended(settings: &ToolSettings) -> Option<Self> {
        let sandbox = settings
            .sandbox
            .as_ref()
            .filter(|sandbox| sandbox.unattended)?;
        let home = settings
            .settings_file
            .as_ref()
            .and_then(|file| file.home().map(Path::to_path_buf))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        // A webhook run (anyone's text) does not take `sandbox_read`: the
        // user widens it for scheduled jobs (`/` beside `~/.ssh` for a job
        // that pushes over ssh), and the same value would let a webhook
        // run's `read_file` read the whole disk. It reads its own list.
        let read = if settings.webhook {
            sandbox.webhook_read.clone()
        } else {
            sandbox.read.clone().unwrap_or_default()
        };
        let mut scope = Self::new(
            None,
            sandbox.temp.iter().chain(&sandbox.write).cloned().collect(),
            read,
            home.as_deref(),
            settings.settings_file.as_ref(),
        );
        if settings.webhook {
            scope.read_setting = "webhook_sandbox_read";
            // Nor does it read earlier conversations' tool output.
            if let Some(state) = settings
                .settings_file
                .as_ref()
                .and_then(SettingsFile::folder)
            {
                scope.denied.push(crate::session::sessions_dir(&state));
            }
        }
        Some(scope)
    }

    /// A scope with the working folder `work` (`None`: the current one),
    /// more writable and readable folders, the credentials in `home` and
    /// the paths protected with `settings_file` denied, and the startup
    /// files in `home` not writable.
    pub fn new(
        work: Option<PathBuf>,
        write: Vec<PathBuf>,
        read: Vec<PathBuf>,
        home: Option<&Path>,
        settings_file: Option<&SettingsFile>,
    ) -> Self {
        let under_home = |names: &[&str]| -> Vec<PathBuf> {
            home.iter()
                .flat_map(|home| names.iter().map(|name| home.join(name)))
                .collect()
        };
        let mut denied = under_home(UNATTENDED_CREDENTIALS);
        denied.extend(settings_file.iter().flat_map(|file| file.protected_paths()));
        Self {
            work,
            write,
            read,
            read_setting: "sandbox_read",
            denied,
            no_write: home
                .map(|home| {
                    unattended_no_write(home, std::env::var_os("XDG_CONFIG_HOME").as_deref())
                })
                .unwrap_or_default(),
            programs: home
                .map(|home| program_folders(home, std::env::var_os("PATH").as_deref()))
                .unwrap_or_default(),
        }
    }

    /// Uses `programs` as the program folders instead of the ones of the
    /// process's PATH (tests).
    #[cfg(test)]
    fn with_programs(mut self, programs: Vec<PathBuf>) -> Self {
        self.programs = programs;
        self
    }

    fn check(&self, real: &Path, shown: &str, use_: Use) -> Result<(), Refusal> {
        let within = |paths: &[PathBuf]| {
            paths
                .iter()
                .find(|path| forms(path).iter().any(|form| real.starts_with(form)))
                .cloned()
        };
        if let Some(denied) = within(&self.denied) {
            return Err(Refusal::Denied(format!(
                "{shown} is a credential file or folder, a private Junees path or saved sessions \
                 ({}). This run is unattended (a scheduled job or a webhook), and its file tools \
                 neither read nor change it.",
                denied.display()
            )));
        }
        let work = match &self.work {
            Some(work) => work.clone(),
            None => std::env::current_dir()?,
        };
        let mut allowed: Vec<&Path> = vec![&work];
        allowed.extend(self.write.iter().map(PathBuf::as_path));
        if use_ == Use::Read {
            allowed.extend(self.read.iter().map(PathBuf::as_path));
        }
        if allowed
            .iter()
            .flat_map(|path| forms(path))
            .any(|form| real.starts_with(form))
        {
            // Within the allowed folders (a job working in the home
            // folder), still not the files that run later. A program
            // folder that is or holds the working folder does not count
            // in the working folder, like in the sandbox: refusing would
            // leave the run nothing to write, and below the program folder
            // itself nothing is on PATH.
            let in_work = forms(&work).iter().any(|form| real.starts_with(form));
            let holds_work = |folder: &PathBuf| {
                forms(folder)
                    .iter()
                    .any(|folder| forms(&work).iter().any(|work| work.starts_with(folder)))
            };
            let programs: Vec<PathBuf> = self
                .programs
                .iter()
                .filter(|folder| !(in_work && holds_work(folder)))
                .cloned()
                .collect();
            if use_ == Use::Write
                && let Some(startup) = within(&self.no_write).or_else(|| within(&programs))
            {
                return Err(Refusal::Denied(format!(
                    "{shown} is a shell startup file, user service, autostart entry, git or cargo \
                     setting, ssh file, or program folder on PATH ({}): what it holds runs later, \
                     outside this run. This run is unattended (a scheduled job or a webhook), and \
                     its file tools do not change it.",
                    startup.display()
                )));
            }
            return Ok(());
        }
        let (what, folders, setting) = match use_ {
            Use::Read => (
                "read",
                format!(
                    "the temporary folders and the `sandbox_write` and `{}` folders",
                    self.read_setting
                ),
                self.read_setting,
            ),
            Use::Write => (
                "write",
                "the temporary folders and the `sandbox_write` folders".to_string(),
                "sandbox_write",
            ),
        };
        Err(Refusal::Denied(format!(
            "{shown} is outside the folders this unattended run (a scheduled job or a webhook) \
             may {what} with the file tools: the working folder ({}), {folders}. No one is there \
             to approve it; the user can add a folder to the `{setting}` setting.",
            work.display()
        )))
    }
}

/// `path` as given (when absolute) and resolved, for prefix checks against
/// resolved paths.
fn forms(path: &Path) -> Vec<PathBuf> {
    let mut forms = Vec::new();
    if path.is_absolute() {
        forms.push(path.to_path_buf());
    }
    if let Ok(real) = std::fs::canonicalize(path)
        && !forms.contains(&real)
    {
        forms.push(real);
    }
    forms
}

/// Refuses paths in [`SYSTEM_FOLDERS`].
fn system_check(path: &Path, shown: &str) -> Result<(), Refusal> {
    match SYSTEM_FOLDERS
        .iter()
        .find(|folder| path.starts_with(folder))
    {
        Some(folder) => Err(Refusal::Denied(format!(
            "{shown} leads into {folder}, where process, kernel and device files live (such as \
             the environment of Junees itself). The file tools do not open anything there."
        ))),
        None => Ok(()),
    }
}

fn changed(path: &str) -> Refusal {
    Refusal::Denied(format!(
        "{path} changed while it was being opened (a link or file was replaced); try again"
    ))
}

/// The folder and the name of the file at the absolute `path`.
fn split(path: &Path) -> Result<(PathBuf, OsString), Refusal> {
    match (path.parent(), path.components().next_back()) {
        (Some(folder), Some(Component::Normal(name))) => {
            Ok((folder.to_path_buf(), name.to_os_string()))
        }
        _ => Err(Refusal::NotRegular),
    }
}

/// What `fstatat` says about a name, without following a link.
#[derive(Debug, Clone, Copy)]
struct Stat {
    regular: bool,
    symlink: bool,
    mode: u32,
    gid: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the path holds a NUL byte"))
}

fn open_path(path: &Path, flags: libc::c_int, mode: libc::c_uint) -> io::Result<File> {
    let path = c_name(path.as_os_str())?;
    // SAFETY: a valid C string; the returned descriptor is new and ours.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is an open descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_at(dir: &File, name: &OsStr, flags: libc::c_int, mode: libc::c_uint) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: `dir` is an open folder and `name` a valid C string; the
    // returned descriptor is new and ours.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is an open descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn mkdir_at(dir: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: an open folder and a valid C string.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename_at(dir: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let (from, to) = (c_name(from)?, c_name(to)?);
    // SAFETY: an open folder and valid C strings.
    if unsafe { libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_at(dir: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: an open folder and a valid C string.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether this process may write the file `name` in `dir`, by its
/// effective ids (permission bits, ACLs, a read-only mount); `Permission
/// denied` when not.
fn writable_at(dir: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: an open folder and a valid C string.
    if unsafe { libc::faccessat(dir.as_raw_fd(), name.as_ptr(), libc::W_OK, libc::AT_EACCESS) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Gives the open `file` the group `gid`, keeping its owner.
fn chgrp(file: &File, gid: libc::gid_t) -> io::Result<()> {
    // SAFETY: an open descriptor; `uid_t::MAX` (-1) leaves the owner.
    if unsafe { libc::fchown(file.as_raw_fd(), libc::uid_t::MAX, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn readlink_at(dir: &File, name: &OsStr) -> io::Result<PathBuf> {
    let name = c_name(name)?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: an open folder, a valid C string and a buffer of `buf.len()`
    // bytes.
    let len = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len < 0 {
        return Err(io::Error::last_os_error());
    }
    let len = len as usize;
    if len >= buf.len() {
        return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
    }
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

// The field types of `struct stat` differ between systems.
#[allow(clippy::unnecessary_cast)]
fn stat_at(dir: &File, name: &OsStr) -> io::Result<Stat> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: an open folder, a valid C string and room for a `stat`.
    let result = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat filled it.
    let stat = unsafe { stat.assume_init() };
    let kind = stat.st_mode & libc::S_IFMT;
    Ok(Stat {
        regular: kind == libc::S_IFREG,
        symlink: kind == libc::S_IFLNK,
        mode: stat.st_mode as u32,
        gid: stat.st_gid as u32,
        nlink: stat.st_nlink as u64,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

/// Where the open `file` is, as the kernel knows it.
#[cfg(target_os = "linux")]
fn fd_path(file: &File) -> io::Result<PathBuf> {
    let path =
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("cannot tell where the opened file is (/proc/self/fd): {err}"),
            )
        })?;
    if !path.is_absolute() {
        return Err(io::Error::other(format!(
            "the opened file is not in a folder ({})",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(target_os = "macos")]
fn fd_path(file: &File) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes and a NUL.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn fd_path(_file: &File) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "cannot tell where an opened file is on this system",
    ))
}

/// Removes the temporary file unless disarmed, so no error path (or
/// panic) leaves it in the working tree, where a task checkpoint would
/// commit it.
struct RemoveOnDrop<'a> {
    dir: &'a File,
    name: &'a OsStr,
    armed: bool,
}

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = unlink_at(self.dir, self.name);
        }
    }
}

/// A hidden, recognisable name for the temporary file of `name`,
/// `.<name>.junees-edit-<pid>-<n>.tmp`, with `<name>` cut short (on a char
/// boundary) so the whole stays within the 255-byte limit on file names.
pub fn temp_name(name: &OsStr) -> OsString {
    const NAME_MAX: usize = 255;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = name.to_string_lossy();
    let suffix = format!(
        ".junees-edit-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let room = NAME_MAX - 1 - suffix.len();
    let cut = name
        .char_indices()
        .map(|(i, ch)| i + ch.len_utf8())
        .take_while(|&end| end <= room)
        .last()
        .unwrap_or(0);
    OsString::from(format!(".{}{suffix}", &name[..cut]))
}

#[cfg(test)]
mod tests;
