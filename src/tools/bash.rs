use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use super::{
    CommandPolicy, SECRET_ENV_VARS, Tool, ToolOutput, ToolSettings, Verdict, opt_u64_arg, str_arg,
    truncate_middle,
};

const MAX_OUTPUT_CHARS: usize = 30_000;
/// Bytes kept from each end of a stream while it is read. Everything in
/// between is counted and dropped, so a huge output never sits in memory.
const CAPTURE_BYTES: usize = 64 * 1024;
const MAX_TIMEOUT_SECS: u64 = 600;
/// How long to keep reading after bash exits, for output still in the pipes.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

/// Runs a command with `bash -c` in the current working directory.
///
/// The command gets its own process group, so a timeout kills everything it
/// started. bash exiting ends the call even if background processes it
/// started still hold the output pipes. Commands the [`CommandPolicy`]
/// denies are not run at all; the default policy has the built-in rules.
pub struct Bash {
    default_timeout: Duration,
    hidden_env: Vec<String>,
    policy: CommandPolicy,
}

impl Default for Bash {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(120),
            hidden_env: Vec::new(),
            policy: CommandPolicy::default(),
        }
    }
}

impl Bash {
    /// Removes these environment variables from the commands' environment,
    /// typically API keys the model must not be able to read.
    pub fn hide_env<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hidden_env.extend(names.into_iter().map(Into::into));
        self
    }

    /// The bash of the default tools: it never sees [`SECRET_ENV_VARS`] or
    /// the user's extra hidden variables, and runs only the commands the
    /// user's [`CommandPolicy`] allows.
    pub fn with_settings(settings: &ToolSettings) -> Self {
        let policy = CommandPolicy::default()
            .deny_prefixes(&settings.deny_commands)
            .allow_dangerous(settings.allow_dangerous);
        Self::default()
            .hide_env(SECRET_ENV_VARS.iter().copied())
            .hide_env(settings.hidden_env.iter().cloned())
            .policy(policy)
    }

    /// Replaces the policy that decides which commands may run.
    pub fn policy(mut self, policy: CommandPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a shell command with `bash -c` in the current working directory. \
         Returns stdout, stderr and the exit code. Stdin is closed, so interactive \
         commands will not work. To leave a process running in the background, \
         redirect its output to a file, e.g. `server > server.log 2>&1 &`. \
         Destructive commands (for example `rm -rf /`, `sudo`, `shutdown`, force push) \
         are blocked by policy and not run."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command to run"},
                "timeout_secs": {
                    "type": "integer",
                    "description": "Kill the command after this many seconds (default 120, max 600)"
                }
            },
            "required": ["command"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let command = match str_arg(&input, "command") {
            Ok(command) => command,
            Err(err) => return err,
        };
        let timeout = match opt_u64_arg(&input, "timeout_secs") {
            Ok(Some(secs)) => Duration::from_secs(secs.clamp(1, MAX_TIMEOUT_SECS)),
            Ok(None) => self.default_timeout,
            Err(err) => return err,
        };
        if let Verdict::Deny(reason) = self.policy.check(command) {
            return ToolOutput::error(format!("blocked by policy: {reason}"));
        }

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for name in &self.hidden_env {
            cmd.env_remove(name);
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => return ToolOutput::error(format!("failed to start bash: {err}")),
        };
        let group = child.id();
        let mut stdout = Capture::start(child.stdout.take());
        let mut stderr = Capture::start(child.stderr.take());

        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(err)) => {
                kill_group(group);
                return ToolOutput::error(format!("failed to wait for command: {err}"));
            }
            Err(_) => {
                kill_group(group);
                let _ = child.wait().await;
                None
            }
        };
        let drained = tokio::time::timeout(DRAIN_GRACE, async {
            stdout.finished().await;
            stderr.finished().await;
        })
        .await
        .is_ok();

        let mut text = stdout.into_text();
        let stderr = stderr.into_text();
        if !stderr.is_empty() {
            ensure_newline(&mut text);
            text.push_str("[stderr]\n");
            text.push_str(&stderr);
        }
        ensure_newline(&mut text);
        if !drained {
            text.push_str(
                "[background processes still hold the output pipes; their later output \
                 is not captured]\n",
            );
        }
        match status {
            Some(status) => match status.code() {
                Some(code) => text.push_str(&format!("[exit code {code}]")),
                None => text.push_str("[terminated by signal]"),
            },
            None => text.push_str(&format!(
                "[timed out after {}s; the command and its child processes were killed]",
                timeout.as_secs()
            )),
        }

        ToolOutput {
            content: truncate_middle(&text, MAX_OUTPUT_CHARS),
            is_error: !status.is_some_and(|status| status.success()),
        }
    }
}

fn kill_group(group: Option<u32>) {
    if let Some(pid) = group.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: killpg only sends a signal. The group was created for this
        // command with process_group(0), so its id equals the child's pid.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

fn ensure_newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

/// Reads a pipe in the background, keeping only its head and tail.
struct Capture {
    kept: Arc<Mutex<HeadTail>>,
    task: Option<JoinHandle<()>>,
}

impl Capture {
    fn start<R>(reader: Option<R>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let kept = Arc::new(Mutex::new(HeadTail::new(CAPTURE_BYTES)));
        let task = reader.map(|mut reader| {
            let kept = Arc::clone(&kept);
            tokio::spawn(async move {
                let mut chunk = [0u8; 8192];
                loop {
                    match reader.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => kept.lock().unwrap().push(&chunk[..n]),
                    }
                }
            })
        });
        Self { kept, task }
    }

    /// Resolves when the pipe reaches end of file.
    async fn finished(&mut self) {
        if let Some(task) = &mut self.task {
            let _ = task.await;
        }
    }

    fn into_text(self) -> String {
        if let Some(task) = &self.task {
            task.abort();
        }
        self.kept.lock().unwrap().to_text()
    }
}

/// Keeps the first and last `cap` bytes of a stream and counts the rest.
struct HeadTail {
    cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    dropped: u64,
}

impl HeadTail {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            head: Vec::new(),
            tail: VecDeque::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.cap - self.head.len();
        let (to_head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(to_head);
        if rest.len() >= self.cap {
            self.dropped += (self.tail.len() + rest.len() - self.cap) as u64;
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - self.cap..]);
        } else {
            self.tail.extend(rest);
            let excess = self.tail.len().saturating_sub(self.cap);
            self.tail.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    fn to_text(&self) -> String {
        let (front, back) = self.tail.as_slices();
        let tail = [front, back].concat();
        if self.dropped == 0 {
            return String::from_utf8_lossy(&[self.head.as_slice(), &tail].concat()).into_owned();
        }
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            String::from_utf8_lossy(&self.head),
            self.dropped,
            String::from_utf8_lossy(&tail)
        )
    }
}

/// Process checks shared by the tests that kill tool processes.
#[cfg(test)]
pub(crate) mod testing {
    use std::time::Duration;

    /// Whether the process runs: it exists and is not a zombie (a killed
    /// child stays a zombie until its parent or init reaps it).
    pub fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // The state follows the command name, which is in parentheses
            // and may itself contain spaces or parentheses.
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X"),
            Err(_) => false,
        }
    }

    /// Waits up to two seconds for the process to stop running.
    pub async fn gone_soon(pid: u32) -> bool {
        for _ in 0..40 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let output = Bash::default().run(json!({"command": "echo hello"})).await;
        assert_eq!(output, ToolOutput::ok("hello\n[exit code 0]"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_with_stderr() {
        let output = Bash::default()
            .run(json!({"command": "echo out; echo bad >&2; exit 3"}))
            .await;
        assert!(output.is_error);
        assert_eq!(output.content, "out\n[stderr]\nbad\n[exit code 3]");
    }

    #[tokio::test]
    async fn timeout_keeps_partial_output() {
        let output = Bash::default()
            .run(json!({"command": "echo before; sleep 5", "timeout_secs": 1}))
            .await;
        assert!(output.is_error);
        assert!(output.content.starts_with("before\n"), "{}", output.content);
        assert!(output.content.contains("timed out after 1s"));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        // The background child prints its own pid, so the check looks at
        // exactly this process and not at a look-alike from another test
        // run on the same machine.
        let output = Bash::default()
            .run(json!({"command": "sleep 30 & echo $!; sleep 30", "timeout_secs": 1}))
            .await;
        assert!(output.is_error);
        let pid: u32 = output.content.lines().next().unwrap().parse().unwrap();
        assert!(
            testing::gone_soon(pid).await,
            "background child {pid} survived"
        );
    }

    #[tokio::test]
    async fn background_processes_do_not_block_the_call() {
        let started = Instant::now();
        let output = Bash::default()
            .run(json!({"command": "sleep 3 & echo started", "timeout_secs": 10}))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.starts_with("started\n"));
        assert!(output.content.contains("background processes"));
    }

    #[tokio::test]
    async fn huge_output_is_bounded() {
        let output = Bash::default()
            .run(json!({"command": "head -c 5000000 /dev/zero | tr '\\0' a"}))
            .await;
        assert!(!output.is_error);
        assert!(output.content.chars().count() < MAX_OUTPUT_CHARS + 200);
        assert!(output.content.contains("omitted"));
        assert!(output.content.ends_with("[exit code 0]"));
    }

    #[tokio::test]
    async fn hidden_env_vars_are_removed() {
        let output = Bash::default()
            .hide_env(["HOME"])
            .run(json!({"command": "printenv HOME"}))
            .await;
        assert!(output.is_error, "{}", output.content);
        assert_eq!(output.content, "[exit code 1]");
    }

    #[tokio::test]
    async fn denied_commands_are_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let output = Bash::default()
            .run(json!({"command": format!("touch {}; ls && rm -rf ~", marker.display())}))
            .await;
        assert_eq!(
            output,
            ToolOutput::error("blocked by policy: recursive `rm` of `~`")
        );
        assert!(!marker.exists(), "part of a blocked command ran");
    }

    #[tokio::test]
    async fn policy_can_be_replaced() {
        // A function named like a denied program, so nothing real runs.
        let command = "shutdown() { echo fake shutdown; }; shutdown";
        let output = Bash::default().run(json!({"command": command})).await;
        assert!(output.content.starts_with("blocked by policy:"));

        let output = Bash::default()
            .policy(CommandPolicy::default().allow_dangerous(true))
            .run(json!({"command": command}))
            .await;
        assert_eq!(output, ToolOutput::ok("fake shutdown\n[exit code 0]"));

        let output = Bash::default()
            .policy(CommandPolicy::default().deny_prefixes(["echo secret"]))
            .run(json!({"command": "echo secret"}))
            .await;
        assert_eq!(
            output,
            ToolOutput::error(
                "blocked by policy: matches JUNEES_DENY_COMMANDS entry `echo secret`"
            )
        );
    }

    #[tokio::test]
    async fn stdin_is_closed() {
        let output = Bash::default().run(json!({"command": "cat"})).await;
        assert_eq!(output, ToolOutput::ok("[exit code 0]"));
    }

    #[tokio::test]
    async fn missing_command_is_an_error() {
        let output = Bash::default().run(json!({})).await;
        assert!(output.is_error);
        assert!(output.content.contains("`command`"));
    }

    #[test]
    fn head_tail_keeps_everything_under_the_cap() {
        let mut kept = HeadTail::new(4);
        kept.push(b"abc");
        kept.push(b"defg");
        assert_eq!(kept.to_text(), "abcdefg");
    }

    #[test]
    fn head_tail_drops_the_middle() {
        let mut kept = HeadTail::new(3);
        kept.push(b"abcdef");
        kept.push(b"ghij");
        assert_eq!(kept.to_text(), "abc\n[... 4 bytes omitted ...]\nhij");
        kept.push(b"klmnopqrstu");
        assert_eq!(kept.to_text(), "abc\n[... 15 bytes omitted ...]\nstu");
    }
}
