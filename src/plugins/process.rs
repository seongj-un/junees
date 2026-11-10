//! Running the command of a plugin tool or hook: in the plugin's folder,
//! in its own process group, without Junees's secrets, with a JSON input
//! on stdin and a time limit.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::manifest::Command;
use crate::tools::{CLAUDE_CODE_SECRET_ENV_VARS, Capture, GroupGuard, SECRET_ENV_VARS};

/// How long to keep reading after the command exits, for output still in
/// the pipes.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

/// What a command's process sees besides its input.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// Variables removed on top of [`SECRET_ENV_VARS`] and
    /// [`CLAUDE_CODE_SECRET_ENV_VARS`] (`JUNEES_HIDDEN_ENV`).
    pub hidden: Vec<String>,
    /// Variables set for it.
    pub vars: Vec<(String, String)>,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal,
    /// It ran past its time limit and was killed with its process group.
    TimedOut(Duration),
}

/// A command that ran: how it ended and the head and tail of its output.
#[derive(Debug, Clone)]
pub struct Ran {
    pub exit: Exit,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The last line it wrote on stderr, shortened, hidden characters
    /// escaped.
    pub fn stderr_line(&self) -> Option<String> {
        let line = self
            .stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        let line: String = line.chars().take(300).collect();
        Some(crate::prompt_text::escape_hidden(&line))
    }
}

/// Runs `command` in `dir` (the plugin's folder) with `input` on its stdin.
/// Its process group is killed when it times out or when the returned
/// future is dropped (a cancelled turn); what it leaves running after it
/// exited on its own stays, as with bash. `Err` says why it could not
/// start.
pub async fn run(command: &Command, dir: &Path, env: &Env, input: Vec<u8>) -> Result<Ran, String> {
    let mut process = tokio::process::Command::new(command.program(dir));
    process
        .args(&command.argv[1..])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    for name in SECRET_ENV_VARS
        .iter()
        .chain(CLAUDE_CODE_SECRET_ENV_VARS)
        .copied()
        .chain(env.hidden.iter().map(String::as_str))
    {
        process.env_remove(name);
    }
    process.envs(env.vars.iter().map(|(name, value)| (name, value)));
    let mut child = process
        .spawn()
        .map_err(|err| format!("cannot run `{}`: {err}", command.shown()))?;
    let mut group = GroupGuard(child.id());
    let mut stdout = Capture::start(child.stdout.take());
    let mut stderr = Capture::start(child.stderr.take());
    // Written on the side: a command that does not read its input must not
    // block on a full pipe while Junees waits for it. Dropping stdin then
    // closes it.
    let writer = child.stdin.take().map(|mut stdin| {
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        })
    });
    let exit = match tokio::time::timeout(command.timeout, child.wait()).await {
        Ok(Ok(status)) => status.code().map_or(Exit::Signal, Exit::Code),
        Ok(Err(err)) => return Err(format!("cannot wait for `{}`: {err}", command.shown())),
        Err(_) => {
            group.kill();
            let _ = child.wait().await;
            Exit::TimedOut(command.timeout)
        }
    };
    group.disarm();
    if let Some(writer) = writer {
        writer.abort();
    }
    let _ = tokio::time::timeout(DRAIN_GRACE, async {
        stdout.finished().await;
        stderr.finished().await;
    })
    .await;
    Ok(Ran {
        exit,
        stdout: stdout.into_text(),
        stderr: stderr.into_text(),
    })
}
