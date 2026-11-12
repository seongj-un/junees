//! Starting and stopping the browser: the program, a private profile per
//! session, the DevTools pipe and the cleanup.
//!
//! Chrome talks the DevTools protocol over a pipe
//! (`--remote-debugging-pipe`: it reads commands from fd 3 and writes
//! answers and events to fd 4, each message followed by a NUL byte),
//! not over a debugging port: a port on 127.0.0.1 would let any local
//! program, the model's own bash commands included, drive the browser
//! past the approval questions. Chrome ends when the pipe closes, so it
//! does not outlive Junees even when Junees is killed.

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};

use super::Settings;

/// The programs looked for on PATH when no path is set, in this order.
pub const PROGRAM_NAMES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
];

/// The window size pages are laid out (and screenshots taken) at.
pub const WINDOW: (u32, u32) = (1280, 800);

/// Bytes of Chrome's stderr kept for the message when it fails to start.
const STDERR_TAIL: usize = 4096;

/// The browser program: the `browser_path` setting (a path, or a name
/// looked up on `path`), else the first of [`PROGRAM_NAMES`] on `path`.
pub fn find(settings: &Settings, path: Option<&OsStr>) -> Result<PathBuf> {
    match &settings.path {
        Some(program) if program.components().count() > 1 => {
            if program.is_file() {
                Ok(program.clone())
            } else {
                bail!(
                    "browser_path (JUNEES_BROWSER_PATH) {} is not a file",
                    program.display()
                )
            }
        }
        Some(program) => crate::claude_code::find_on_path(&program.to_string_lossy(), path)
            .with_context(|| {
                format!(
                    "browser_path (JUNEES_BROWSER_PATH) `{}` is not on PATH",
                    program.display()
                )
            }),
        None => PROGRAM_NAMES
            .iter()
            .find_map(|name| crate::claude_code::find_on_path(name, path))
            .with_context(|| {
                format!(
                    "no browser found: none of {} is on PATH; install Chromium or set \
                     browser_path (JUNEES_BROWSER_PATH)",
                    PROGRAM_NAMES.join(", ")
                )
            }),
    }
}

/// `<program> --version` (offline: Chrome prints it and exits), for
/// `junees doctor`.
pub async fn version(program: &Path) -> Result<String> {
    let mut command = Command::new(program);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    scrub(&mut command, &[]);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("`--version` did not finish in 15 s")?
        .with_context(|| format!("cannot run {}", program.display()))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || text.is_empty() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`{} --version` failed ({}): {}",
            program.display(),
            output.status,
            err.trim().lines().last().unwrap_or_default()
        );
    }
    Ok(text.lines().next().unwrap_or_default().to_string())
}

/// The longest unix socket path (`sun_path` holds 108 bytes with the NUL).
const MAX_SOCKET_PATH: usize = 107;
/// What Chromium adds to its temporary folder for its single-instance
/// socket, and room for other builds' longer names (Chrome's
/// `com.google.Chrome.XXXXXX` is shorter).
const SOCKET_SUFFIX: usize = "/org.chromium.Chromium.XXXXXX/SingletonSocket".len() + 8;
/// `/junees-browser-` and 16 hex digits.
const PROFILE_NAME: usize = "/junees-browser-".len() + 16;

/// Where a `junees-browser-*` folder Chrome takes as its temporary folder
/// goes: the temporary folder `temp`, or `/tmp` when Chrome's socket in
/// it would be too long a path (a long TMPDIR made Chrome end at once).
pub fn profile_base(temp: &Path) -> PathBuf {
    if temp.as_os_str().len() + PROFILE_NAME + SOCKET_SUFFIX <= MAX_SOCKET_PATH {
        temp.to_path_buf()
    } else {
        PathBuf::from("/tmp")
    }
}

/// Whether Chrome's socket fits in the path limit with `temp` as its
/// temporary folder.
pub fn socket_fits(temp: &Path) -> bool {
    temp.as_os_str().len() + SOCKET_SUFFIX <= MAX_SOCKET_PATH
}

/// Whether the profiles in `folder` can be Chrome's temporary folder
/// (else its socket and temporary files go in a separate folder in the
/// temporary folder, which sandboxed commands can reach).
pub fn profiles_hold_socket(folder: &Path) -> bool {
    socket_fits(&folder.join("0".repeat(PROFILE_HEX)))
}

/// The longest profiles folder whose profiles hold Chrome's socket.
pub const MAX_PROFILES_FOLDER: usize = MAX_SOCKET_PATH - SOCKET_SUFFIX - 1 - PROFILE_HEX;

/// Hex digits of a profile's name in the profiles folder.
const PROFILE_HEX: usize = 16;
/// The file in a profile its owner holds an exclusive `flock` on for as
/// long as the profile is in use: a profile whose lock is free belongs
/// to a process that is gone (killed, crashed) and is removed by the
/// next browser start ([`remove_stale`]).
const LOCK_FILE: &str = "junees.lock";
/// The file in a profile naming Chrome's separate temporary folder, so
/// that [`remove_stale`] removes it too.
const TEMP_FILE: &str = "junees-temp";
/// A profile without a lock file (from before the lock), or one left
/// half made ([`MAKING`]), is removed only after this long untouched.
const UNLOCKED_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The name prefix of a profile while it is made: its lock file exists
/// but is not locked yet, so [`remove_stale`] (in another process) would
/// take it for a dead one under its final name. It gets that name once
/// locked.
const MAKING: &str = ".new-";

/// A private (0700) profile folder, removed when dropped, and Chrome's
/// temporary folder (TMPDIR), so what Chrome puts there (its
/// single-instance socket folder) goes with it.
///
/// The profile goes in the profiles folder of the state folder
/// ([`super::PROFILES_FOLDER`]), out of reach of sandboxed bash commands,
/// which may write the temporary folders. Chrome's temporary folder is
/// the profile itself when the socket path fits there; else a private
/// folder of its own in the temporary folder (or `/tmp`, [`profile_base`]),
/// which then holds only the socket and Chrome's temporary files.
/// Without a state folder both are one folder in the temporary folder.
#[derive(Debug)]
pub struct Profile {
    path: PathBuf,
    /// Chrome's temporary folder, when it is not the profile.
    temp: Option<PathBuf>,
    /// The locked [`LOCK_FILE`] of a profile in the profiles folder.
    _lock: Option<std::fs::File>,
}

impl Profile {
    /// A new profile in `folder` (made 0700 if missing), or in the
    /// temporary folder when `None`. Profiles left in `folder` by a
    /// process that is gone are removed first.
    pub fn new(folder: Option<&Path>) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let Some(folder) = folder else {
            let path = private_folder(&profile_base(&std::env::temp_dir()))?;
            return Ok(Self {
                path,
                temp: None,
                _lock: None,
            });
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)?;
        remove_stale(folder);
        let name = crate::cron::store::random_hex(PROFILE_HEX);
        let making = folder.join(format!("{MAKING}{name}"));
        // Fails when the name is taken: never a folder someone else made.
        std::fs::DirBuilder::new().mode(0o700).create(&making)?;
        // From here on a failure drops `profile`, which removes it.
        let mut profile = Self {
            path: making,
            temp: None,
            _lock: None,
        };
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(profile.path.join(LOCK_FILE))?;
        if !crate::flock::try_exclusive(&lock)? {
            return Err(std::io::Error::other("cannot lock the new browser profile"));
        }
        profile._lock = Some(lock);
        // Locked: from now on it may carry a name `remove_stale` looks at
        // (the lock stays with the renamed file).
        let path = folder.join(&name);
        std::fs::rename(&profile.path, &path)?;
        profile.path = path;
        if !socket_fits(&profile.path) {
            let temp = private_folder(&profile_base(&std::env::temp_dir()))?;
            profile.temp = Some(temp.clone());
            let mut note = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(profile.path.join(TEMP_FILE))?;
            note.write_all(temp.as_os_str().as_encoded_bytes())?;
        }
        Ok(profile)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Chrome's temporary folder (TMPDIR).
    pub fn temp(&self) -> &Path {
        self.temp.as_deref().unwrap_or(&self.path)
    }
}

/// Removes the profiles in the profiles folder `folder` whose owner is
/// gone: their [`LOCK_FILE`] can be locked (a live owner holds the lock;
/// a dead one's lock went with it). A profile without a lock file is
/// removed only when it has not changed for [`UNLOCKED_AGE`], and so is
/// one left half made ([`MAKING`]). Entries that are not profile folders
/// (other names, links) are left alone.
pub fn remove_stale(folder: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let (hex, making) = match name.strip_prefix(MAKING) {
            Some(hex) => (hex, true),
            None => (&*name, false),
        };
        if hex.len() != PROFILE_HEX || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > UNLOCKED_AGE);
        // Its lock file may be there but not locked yet.
        if making && !old {
            continue;
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.join(LOCK_FILE));
        // Held (when there is one) until the profile is gone.
        let _held = match lock {
            Ok(lock) => {
                // No retry: a lock that only looks taken (a fork in another
                // thread holds a copy for a moment) keeps the profile,
                // which is the safe side.
                // SAFETY: flock on a descriptor `lock` owns.
                if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    continue;
                }
                Some(lock)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !old {
                    continue;
                }
                None
            }
            Err(_) => continue,
        };
        if let Some(temp) = stale_temp(&path) {
            remove(&temp);
        }
        remove(&path);
    }
}

/// The separate temporary folder a stale profile names in [`TEMP_FILE`],
/// if it is one Junees makes: `junees-browser-<16 hex>` in the temporary
/// folder or `/tmp`, a folder (not a link) of this user.
fn stale_temp(profile: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let text = std::fs::read(profile.join(TEMP_FILE)).ok()?;
    let temp = PathBuf::from(std::ffi::OsString::from_vec(text));
    let name = temp.file_name()?.to_str()?;
    let hex = name.strip_prefix("junees-browser-")?;
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let base = temp.parent()?;
    if base != std::env::temp_dir() && base != Path::new("/tmp") {
        return None;
    }
    let meta = std::fs::symlink_metadata(&temp).ok()?;
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    (meta.is_dir() && meta.uid() == me).then_some(temp)
}

/// A new private (0700) `junees-browser-<random>` folder in `base`.
fn private_folder(base: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let path = base.join(format!(
        "junees-browser-{}",
        crate::cron::store::random_hex(16)
    ));
    // Fails when the name is taken: never a folder someone else made.
    std::fs::DirBuilder::new().mode(0o700).create(&path)?;
    Ok(path)
}

/// Removes `path`, again while a dying helper process still writes a file.
fn remove(path: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(path).is_ok() || !path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // Chrome's single-instance socket folder (`org.chromium.Chromium.*`)
        // is in its temporary folder: it goes with it.
        remove(&self.path);
        if let Some(temp) = &self.temp {
            remove(temp);
        }
    }
}

/// A running browser. Dropping it kills its process group, reaps it and
/// removes its profile.
#[derive(Debug)]
pub struct Process {
    child: Child,
    pid: i32,
    stderr: Arc<Mutex<Vec<u8>>>,
    // Dropped after the process is gone (fields drop after `drop`).
    profile: Profile,
}

/// The two pipe ends Junees keeps: Chrome's output and its input.
pub struct Pipes {
    pub from_browser: pipe::Receiver,
    pub to_browser: pipe::Sender,
}

/// Starts `program` headless with a new profile and the DevTools pipe.
/// The secret variables Junees knows of (and `hidden_env`) are not passed
/// on: a page that took over the browser must not find them.
pub fn launch(
    program: &Path,
    settings: &Settings,
    hidden_env: &[String],
) -> Result<(Process, Pipes)> {
    let profile = Profile::new(settings.profiles.as_deref())
        .context("cannot create the browser profile folder")?;
    // Junees writes commands into `to_child`, Chrome reads them on fd 3;
    // Chrome writes to fd 4, Junees reads `from_child`.
    let (child_reads, to_child) = cloexec_pipe()?;
    let (from_child, child_writes) = cloexec_pipe()?;
    let mut command = command(
        program,
        settings,
        hidden_env,
        profile.path(),
        profile.temp(),
    );
    let (read_fd, write_fd) = (child_reads.as_raw_fd(), child_writes.as_raw_fd());
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec. Both ends are first copied above fd 10 so that neither copy
    // overwrites the other when one of them already is fd 3 or 4; dup2
    // to 3 and 4 then clears close-on-exec on those two alone.
    unsafe {
        command.pre_exec(move || {
            let high_read = dup_high(read_fd)?;
            let high_write = dup_high(write_fd)?;
            if libc::dup2(high_read, 3) < 0 || libc::dup2(high_write, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start {}", program.display()))?;
    drop(child_reads);
    drop(child_writes);
    let pid = child.id().context("the browser exited at once")? as i32;
    let stderr = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut out) = child.stderr.take() {
        let tail = Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = out.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > STDERR_TAIL {
                    let extra = tail.len() - STDERR_TAIL;
                    tail.drain(..extra);
                }
            }
        });
    }
    // From here on a failure drops `process`, which kills the browser.
    let process = Process {
        child,
        pid,
        stderr,
        profile,
    };
    let pipes = Pipes {
        from_browser: pipe::Receiver::from_owned_fd(from_child)
            .context("cannot read from the browser")?,
        to_browser: pipe::Sender::from_owned_fd(to_child).context("cannot write to the browser")?,
    };
    Ok((process, pipes))
}

/// The browser's command line and environment, without the pipe: the
/// profile `profile`, and `temp` as its temporary folder.
pub(super) fn command(
    program: &Path,
    settings: &Settings,
    hidden_env: &[String],
    profile: &Path,
    temp: &Path,
) -> Command {
    let (width, height) = WINDOW;
    let mut command = Command::new(program);
    command
        .arg("--headless=new")
        .arg("--remote-debugging-pipe")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-extensions",
            "--disable-sync",
            "--disable-breakpad",
            "--disable-client-side-phishing-detection",
            "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication",
            "--deny-permission-prompts",
            "--password-store=basic",
            "--use-mock-keychain",
            "--metrics-recording-only",
            "--mute-audio",
            "--hide-scrollbars",
        ])
        .arg(format!("--window-size={width},{height}"));
    if settings.no_sandbox {
        command.arg("--no-sandbox");
    }
    command
        .arg("about:blank")
        .current_dir(profile)
        .env("TMPDIR", temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // `Process`'s drop kills the whole group and reaps it.
        .process_group(0);
    scrub(&mut command, hidden_env);
    command
}

impl Process {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn profile(&self) -> &Path {
        self.profile.path()
    }

    /// The last lines Chrome wrote to stderr, for a start-up failure.
    pub fn stderr_tail(&self) -> String {
        let tail = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8_lossy(&tail);
        let lines: Vec<&str> = text.lines().rev().take(5).collect();
        let lines: Vec<&str> = lines.into_iter().rev().collect();
        crate::prompt_text::escape_hidden(&lines.join("\n"))
    }

    /// Waits up to `grace` for the browser to exit by itself (after
    /// `Browser.close` or the pipe closing), then kills what is left.
    pub async fn finish(self, grace: Duration) {
        let deadline = tokio::time::Instant::now() + grace;
        while !self.exited() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Not reaped yet: `drop` kills the group (helpers may still run),
        // reaps and removes the profile.
    }

    /// Whether the browser's main process has exited (a zombie until
    /// reaped here, or gone).
    fn exited(&self) -> bool {
        match std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
            Ok(stat) => stat
                .rfind(')')
                .and_then(|at| stat.get(at + 2..at + 3))
                .is_none_or(|state| state == "Z"),
            Err(_) => true,
        }
    }

    fn kill_and_reap(&mut self) {
        // SAFETY: signals the process group this process started. Only
        // this function reaps the child, so its pid, the group's id,
        // cannot belong to anyone else yet.
        unsafe {
            libc::killpg(self.pid, libc::SIGKILL);
        }
        for _ in 0..200 {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        // Wait until the helpers of the group are gone too, so none of
        // them writes into the profile while it is removed. Signal 0
        // sends nothing, whoever has the id by now.
        for _ in 0..200 {
            // SAFETY: kill with signal 0 only checks that the group exists.
            if unsafe { libc::kill(-self.pid, 0) } != 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Removes the secret variables from `command`'s environment.
fn scrub(command: &mut Command, hidden_env: &[String]) {
    for name in crate::tools::SECRET_ENV_VARS
        .iter()
        .chain(crate::tools::CLAUDE_CODE_SECRET_ENV_VARS)
        .copied()
        .chain(hidden_env.iter().map(String::as_str))
    {
        command.env_remove(name);
    }
}

/// A pipe whose two ends close on exec: (read end, write end).
fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd)> {
    // `std::io::pipe` sets close-on-exec on both ends (`pipe2` on Linux,
    // `pipe` + `fcntl` where there is no `pipe2`, as on macOS).
    let (read, write) = std::io::pipe().context("cannot create a pipe")?;
    Ok((OwnedFd::from(read), OwnedFd::from(write)))
}

/// A copy of `fd` numbered 10 or above, closed on exec.
fn dup_high(fd: RawFd) -> std::io::Result<RawFd> {
    // SAFETY: fcntl on a descriptor this process holds.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copy < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(copy)
    }
}
