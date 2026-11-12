//! Starting and stopping the browser: the program, a private temporary
//! profile per session, the DevTools pipe and the cleanup.
//!
//! Chrome talks the DevTools protocol over a pipe
//! (`--remote-debugging-pipe`: it reads commands from fd 3 and writes
//! answers and events to fd 4, each message followed by a NUL byte),
//! not over a debugging port: a port on 127.0.0.1 would let any local
//! program, the model's own bash commands included, drive the browser
//! past the approval questions. Chrome ends when the pipe closes, so it
//! does not outlive Junees even when Junees is killed.

use std::ffi::OsStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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

/// A private (0700) temporary profile folder, removed when dropped.
#[derive(Debug)]
pub struct Profile(PathBuf);

impl Profile {
    pub fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!(
            "junees-browser-{}",
            crate::cron::store::random_hex(16)
        ));
        // Fails when the name is taken: never a folder someone else made.
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // Chrome keeps its single-instance socket in a folder of its own in
        // the temporary folder (`org.chromium.Chromium.*` or
        // `com.google.Chrome.*`; a socket path inside the profile could be
        // too long), linked from the profile, and leaves it there when
        // killed.
        if let Ok(socket) = std::fs::read_link(self.0.join("SingletonSocket"))
            && let Some(folder) = socket.parent()
            && folder.parent() == Some(std::env::temp_dir().as_path())
            && folder.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("org.chromium.") || name.starts_with("com.google.")
            })
        {
            let _ = std::fs::remove_dir_all(folder);
        }
        // A dying helper process may still write a file: try again once.
        if std::fs::remove_dir_all(&self.0).is_err() {
            std::thread::sleep(Duration::from_millis(100));
            let _ = std::fs::remove_dir_all(&self.0);
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
    let profile = Profile::new().context("cannot create the browser profile folder")?;
    // Junees writes commands into `to_child`, Chrome reads them on fd 3;
    // Chrome writes to fd 4, Junees reads `from_child`.
    let (child_reads, to_child) = cloexec_pipe()?;
    let (from_child, child_writes) = cloexec_pipe()?;
    let (width, height) = WINDOW;
    let mut command = Command::new(program);
    command
        .arg("--headless=new")
        .arg("--remote-debugging-pipe")
        .arg(format!("--user-data-dir={}", profile.path().display()))
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
        .current_dir(profile.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    scrub(&mut command, hidden_env);
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
    let pipes = Pipes {
        from_browser: pipe::Receiver::from_owned_fd(from_child)
            .context("cannot read from the browser")?,
        to_browser: pipe::Sender::from_owned_fd(to_child).context("cannot write to the browser")?,
    };
    Ok((
        Process {
            child,
            pid,
            stderr,
            profile,
        },
        pipes,
    ))
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
    pub async fn finish(mut self, grace: Duration) {
        let _ = tokio::time::timeout(grace, self.child.wait()).await;
        // `drop` kills the group (helpers may still run) and reaps.
    }

    fn kill_and_reap(&mut self) {
        // SAFETY: signals the process group this process started; the
        // child is not reaped yet, so its pid (the group id) is still ours.
        unsafe {
            libc::killpg(self.pid, libc::SIGKILL);
        }
        for _ in 0..200 {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
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
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2 writes two new descriptors into `fds`.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot create a pipe");
    }
    // SAFETY: both descriptors are new and owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
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
