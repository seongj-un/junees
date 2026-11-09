use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolOutput, opt_u64_arg, str_arg, truncate_middle};

const MAX_OUTPUT_CHARS: usize = 30_000;
const MAX_TIMEOUT_SECS: u64 = 600;

/// Runs a command with `bash -c` in the current working directory.
pub struct Bash {
    default_timeout: Duration,
}

impl Default for Bash {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(120),
        }
    }
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command with `bash -c` in the current working directory. \
         Returns stdout, stderr and the exit code. Stdin is closed, so interactive \
         commands will not work."
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

        let child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(err) => return ToolOutput::error(format!("failed to start bash: {err}")),
        };

        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => return ToolOutput::error(format!("failed to wait for command: {err}")),
            Err(_) => {
                return ToolOutput::error(format!(
                    "command timed out after {}s and was killed",
                    timeout.as_secs()
                ));
            }
        };

        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            ensure_newline(&mut text);
            text.push_str("[stderr]\n");
            text.push_str(&stderr);
        }
        ensure_newline(&mut text);
        match output.status.code() {
            Some(code) => text.push_str(&format!("[exit code {code}]")),
            None => text.push_str("[terminated by signal]"),
        }

        ToolOutput {
            content: truncate_middle(&text, MAX_OUTPUT_CHARS),
            is_error: !output.status.success(),
        }
    }
}

fn ensure_newline(text: &mut String) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
}

#[cfg(test)]
mod tests {
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
    async fn kills_commands_that_time_out() {
        let output = Bash::default()
            .run(json!({"command": "sleep 5", "timeout_secs": 1}))
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("timed out after 1s"));
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
}
