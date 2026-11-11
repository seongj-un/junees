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
    CLAUDE_CODE_SECRET_ENV_VARS, CommandPolicy, SECRET_ENV_VARS, Tool, ToolOutput, ToolSettings,
    Verdict, opt_u64_arg, str_arg, truncate_middle,
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
    /// Without git credentials ([`super::strip_git_credentials`]).
    webhook: bool,
    /// The run's GitHub CLI configuration folder ([`super::GhConfigDir`]).
    gh_config_dir: Option<std::path::PathBuf>,
}

impl Default for Bash {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(120),
            hidden_env: Vec::new(),
            policy: CommandPolicy::default(),
            webhook: false,
            gh_config_dir: None,
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

    /// The bash of the default tools: it never sees [`SECRET_ENV_VARS`],
    /// [`CLAUDE_CODE_SECRET_ENV_VARS`] or the user's extra hidden
    /// variables, and runs only the commands the user's [`CommandPolicy`]
    /// allows.
    pub fn with_settings(settings: &ToolSettings) -> Self {
        Self::default()
            .hide_env(SECRET_ENV_VARS.iter().copied())
            .hide_env(CLAUDE_CODE_SECRET_ENV_VARS.iter().copied())
            .hide_env(settings.hidden_env.iter().cloned())
            .policy(CommandPolicy::with_settings(settings))
            .without_git_credentials(settings.webhook, settings.gh_config_dir.clone())
    }

    /// Runs commands without the user's git and GitHub CLI credentials
    /// (a webhook run, [`ToolSettings::webhook`]), with `gh_config_dir`
    /// as the GitHub CLI configuration folder.
    pub fn without_git_credentials(
        mut self,
        webhook: bool,
        gh_config_dir: Option<std::path::PathBuf>,
    ) -> Self {
        self.webhook = webhook;
        self.gh_config_dir = gh_config_dir;
        self
    }

    /// The process that runs `command`.
    fn command(&self, command: &str) -> Command {
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
        if self.webhook {
            super::strip_git_credentials(&mut cmd, self.gh_config_dir.as_deref());
        }
        cmd
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

        let mut child = match self.command(command).spawn() {
            Ok(child) => child,
            Err(err) => return ToolOutput::error(format!("failed to start bash: {err}")),
        };
        // Kills the command and everything it started if this call ends
        // before the command does: on a timeout, and when the caller drops
        // the call (a cancelled turn).
        let mut group = GroupGuard(child.id());
        let mut stdout = Capture::start(child.stdout.take());
        let mut stderr = Capture::start(child.stderr.take());

        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(err)) => {
                return ToolOutput::error(format!("failed to wait for command: {err}"));
            }
            Err(_) => {
                group.kill();
                let _ = child.wait().await;
                None
            }
        };
        // bash is done; what it left running in the background stays.
        group.disarm();
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

/// The process group of a running command, killed when dropped unless
/// disarmed first.
pub(crate) struct GroupGuard(pub(crate) Option<u32>);

impl GroupGuard {
    /// Kills the group now.
    pub(crate) fn kill(&mut self) {
        if let Some(pid) = self
            .0
            .take()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
        {
            // SAFETY: killpg only sends a signal. The group was created for
            // this command with process_group(0), so its id equals the
            // child's pid, and the child is not reaped yet, so the id still
            // names this group.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }

    /// Leaves the group alone from now on.
    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

fn ensure_newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

/// Reads a pipe in the background, keeping only its head and tail.
pub(crate) struct Capture {
    kept: Arc<Mutex<HeadTail>>,
    task: Option<JoinHandle<()>>,
}

impl Capture {
    pub(crate) fn start<R>(reader: Option<R>) -> Self
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
    pub(crate) async fn finished(&mut self) {
        if let Some(task) = &mut self.task {
            let _ = task.await;
        }
    }

    pub(crate) fn into_text(self) -> String {
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
    async fn dropping_the_call_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let bash = Bash::default();
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let pid = tokio::select! {
            output = bash.run(json!({ "command": command })) => {
                panic!("the command finished: {output:?}")
            }
            pid = async {
                loop {
                    let text = std::fs::read_to_string(&pid_file).unwrap_or_default();
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        break pid;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            } => pid,
        };
        // The call was dropped when the other branch won.
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

    #[test]
    fn webhook_runs_get_no_git_credentials() {
        let webhook = ToolSettings {
            webhook: true,
            ..ToolSettings::default()
        };
        let command = Bash::with_settings(&webhook).command("true");
        let envs: Vec<(String, Option<String>)> = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect();
        let get = |name: &str| {
            envs.iter()
                .find(|(key, _)| key == name)
                .map(|(_, v)| v.clone())
        };
        for name in ["SSH_AUTH_SOCK", "GIT_ASKPASS", "SSH_ASKPASS", "GH_TOKEN"] {
            assert_eq!(get(name), Some(None), "{name} not removed: {envs:?}");
        }
        assert_eq!(get("GIT_CONFIG_COUNT"), Some(Some("1".into())));
        assert_eq!(
            get("GIT_CONFIG_KEY_0"),
            Some(Some("credential.helper".into()))
        );
        assert_eq!(get("GIT_CONFIG_VALUE_0"), Some(Some(String::new())));
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some(Some("0".into())));
        assert_eq!(
            get("GH_CONFIG_DIR"),
            Some(Some(super::super::NO_GH_CONFIG_DIR.into()))
        );
        // The run's own GitHub CLI folder when it has one.
        let gh = super::super::GhConfigDir::new().unwrap();
        let own = Bash::with_settings(&ToolSettings {
            webhook: true,
            gh_config_dir: Some(gh.path().to_path_buf()),
            ..ToolSettings::default()
        })
        .command("true");
        assert!(
            own.as_std().get_envs().any(
                |(name, value)| name == "GH_CONFIG_DIR" && value == Some(gh.path().as_os_str())
            ),
            "{:?}",
            own.as_std().get_envs().collect::<Vec<_>>()
        );
        // Other runs keep them.
        let plain = Bash::with_settings(&ToolSettings::default()).command("true");
        assert!(
            !plain
                .as_std()
                .get_envs()
                .any(|(name, _)| name == "SSH_AUTH_SOCK" || name == "GIT_CONFIG_COUNT")
        );
    }

    #[tokio::test]
    async fn webhook_runs_reach_no_credential_helper() {
        // A repository whose own config has a helper that hands out a
        // password; the shell of a webhook run must not get it.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().display().to_string();
        let setup = format!(
            "unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR; git init -q {repo} && \
             git -C {repo} config credential.helper '!f() {{ echo username=u; echo password=leaked-pw; }}; f'"
        );
        let output = Bash::default().run(json!({"command": setup})).await;
        assert!(!output.is_error, "{}", output.content);
        let fill = format!(
            "unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR; \
             printf 'protocol=https\\nhost=git.invalid\\n\\n' | git -C {repo} credential fill; \
             echo \"agent=[$SSH_AUTH_SOCK] askpass=[$GIT_ASKPASS]\""
        );
        // `git credential` is refused by the webhook policy itself; the
        // environment is what this test checks.
        let tools = |webhook| {
            Bash::with_settings(&ToolSettings {
                webhook,
                ..ToolSettings::default()
            })
            .policy(CommandPolicy::default())
        };
        let plain = tools(false).run(json!({"command": fill})).await;
        assert!(
            plain.content.contains("password=leaked-pw"),
            "{}",
            plain.content
        );
        let webhook = tools(true).run(json!({"command": fill})).await;
        assert!(
            !webhook.content.contains("leaked-pw"),
            "{}",
            webhook.content
        );
        assert!(
            webhook.content.contains("agent=[] askpass=[]"),
            "{}",
            webhook.content
        );
    }

    #[test]
    fn a_runs_github_cli_folder_is_new_private_and_removed_after() {
        use std::os::unix::fs::PermissionsExt;
        let gh = super::super::GhConfigDir::new().unwrap();
        let path = gh.path().to_path_buf();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        assert_ne!(super::super::GhConfigDir::new().unwrap().path(), path);
        std::fs::write(path.join("config.yml"), "aliases: {}\n").unwrap();
        drop(gh);
        assert!(!path.exists());
    }

    /// The `gh` program on PATH, if any.
    fn gh_program() -> Option<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("gh"))
            .find(|path| path.is_file())
    }

    #[tokio::test]
    async fn gh_has_no_login_in_a_webhook_runs_shell() {
        // The review's probe with a real `gh`: it reads its login from
        // ~/.config/gh/hosts.yml by itself. A fake HOME holds one with a
        // fake token; nothing here reaches the network (no login means no
        // API call, and `gh auth token` only reads the file).
        let Some(gh) = gh_program() else {
            eprintln!("skipped: no gh on PATH");
            return;
        };
        let home = tempfile::tempdir().unwrap();
        let hosts = home.path().join(".config/gh/hosts.yml");
        std::fs::create_dir_all(hosts.parent().unwrap()).unwrap();
        let token = "gho_junees0fake0token0for0tests0only00000";
        std::fs::write(
            &hosts,
            format!("github.com:\n    oauth_token: {token}\n    user: fake\n"),
        )
        .unwrap();
        let fake_home = |command: &mut Command| {
            command
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join(".config"))
                .env("GH_NO_UPDATE_NOTIFIER", "1")
                .env("GH_PROMPT_DISABLED", "1")
                .env_remove("GH_HOST");
        };
        // Control: this fake login is one gh reads.
        let mut plain = Command::new(&gh);
        plain
            .args(["auth", "token"])
            .stdin(Stdio::null())
            .env_remove("GH_CONFIG_DIR");
        for name in super::super::WEBHOOK_REMOVED_ENV {
            plain.env_remove(name);
        }
        fake_home(&mut plain);
        let out = plain.output().await.unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), token);

        let dir = super::super::GhConfigDir::new().unwrap();
        let bash = Bash::with_settings(&ToolSettings {
            webhook: true,
            gh_config_dir: Some(dir.path().to_path_buf()),
            ..ToolSettings::default()
        });
        let script = format!(
            "{gh} auth token; echo \"token exit=$?\"; {gh} auth status 2>&1; echo \"status exit=$?\"",
            gh = gh.display()
        );
        let mut command = bash.command(&script);
        fake_home(&mut command);
        let out = command.output().await.unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!text.contains(token), "{text}");
        assert!(!text.contains("token exit=0"), "{text}");
        assert!(text.contains("not logged in"), "{text}");
        assert!(!text.contains("status exit=0"), "{text}");
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
