//! `mark_feature_passing`: the only way a feature becomes passing. The
//! harness runs the feature's verify command itself and believes the exit
//! code, not the model.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::features::Ledger;
use super::git::REPO_ENV_VARS;
use crate::tools::{
    Bash, EditFile, ReadFile, Tool, ToolOutput, ToolRegistry, ToolSettings, WriteFile,
};

/// Verify commands may run a whole test suite, so they get the longest
/// timeout the bash tool allows.
pub const VERIFY_TIMEOUT_SECS: u64 = 600;

/// bash, read_file, write_file and edit_file for task sessions, with the user's tool
/// settings like the default tools. Their bash, like the default one,
/// cannot see the API key or the user's hidden variables, and it cannot see
/// [`REPO_ENV_VARS`] either: a session's git commands must act on the
/// repository the task is in, even when Junees itself was started from a
/// git hook or `git rebase --exec`.
pub fn session_tools(settings: &ToolSettings) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(session_bash(settings));
    tools.register(ReadFile::with_settings(settings));
    tools.register(WriteFile::with_settings(settings));
    tools.register(EditFile::with_settings(settings));
    tools.register_search(settings);
    tools
}

/// The bash of task sessions; verify commands run in one like it. Every
/// task tool is built from here, so the session bash and the verify bash
/// always get the same command policy and hidden variables.
fn session_bash(settings: &ToolSettings) -> Bash {
    Bash::with_settings(settings).hide_env(REPO_ENV_VARS.iter().copied())
}

/// Runs verify commands as the harness trusts them: from the repository
/// root, with `pipefail`, and with the session bash's timeout handling,
/// output limits, command policy and hidden variables.
pub struct Verifier {
    root: PathBuf,
    bash: Bash,
}

impl Verifier {
    pub fn new(root: PathBuf, settings: &ToolSettings) -> Self {
        Self {
            root,
            bash: session_bash(settings),
        }
    }

    /// Runs `verify`; the output is an error unless it exited 0.
    pub async fn run(&self, verify: &str) -> ToolOutput {
        // A newline, not `&&`, so the cd applies to the whole command.
        // With pipefail, `cargo test 2>&1 | tail` fails when the tests do.
        let command = format!(
            "cd {} || exit 1\nset -o pipefail\n{verify}",
            shell_quote(&self.root)
        );
        self.bash
            .run(json!({"command": command, "timeout_secs": VERIFY_TIMEOUT_SECS}))
            .await
    }
}

pub struct MarkFeaturePassing {
    ledger: Arc<Mutex<Ledger>>,
    verifier: Verifier,
}

impl MarkFeaturePassing {
    pub fn new(ledger: Arc<Mutex<Ledger>>, verifier: Verifier) -> Self {
        Self { ledger, verifier }
    }
}

#[async_trait]
impl Tool for MarkFeaturePassing {
    fn name(&self) -> &str {
        "mark_feature_passing"
    }

    fn description(&self) -> &str {
        "Ask the harness to mark a feature of .junees/features.json as passing. \
         The harness runs the feature's verify command from the repository root \
         and records the feature as passing only if it exits 0; otherwise you get \
         the command output back. This is the only way a feature can pass: edits \
         to features.json are reverted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The feature id, e.g. F3"}
            },
            "required": ["id"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let Some(id) = input.get("id").and_then(Value::as_str) else {
            return ToolOutput::error("missing required string argument `id`");
        };
        let verify = {
            let ledger = self.ledger.lock().unwrap();
            match ledger.get(id) {
                None => {
                    let ids: Vec<&str> = ledger.features().iter().map(|f| f.id.as_str()).collect();
                    return ToolOutput::error(format!(
                        "no feature has the id `{id}`; the ids are {}",
                        ids.join(", ")
                    ));
                }
                Some(feature) if feature.passes => {
                    return ToolOutput::ok(format!("{id} already passes."));
                }
                Some(feature) => feature.verify.clone(),
            }
        };

        let output = self.verifier.run(&verify).await;
        if output.is_error {
            return ToolOutput::error(format!(
                "{id} does not pass: its verify command failed, so it stays not passing.\n\
                 verify: {verify}\n\n{}",
                output.content
            ));
        }

        let mut ledger = self.ledger.lock().unwrap();
        if let Err(err) = ledger.mark_passing(id) {
            return ToolOutput::error(format!(
                "the verify command passed, but recording it failed: {err:#}"
            ));
        }
        ToolOutput::ok(format!(
            "{id} passes: its verify command succeeded. {}/{} features pass now.\n\n{}",
            ledger.passing(),
            ledger.total(),
            output.content
        ))
    }
}

/// Quotes a path for bash.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::task::{dir, features};

    fn setup(verifies: &[&str]) -> (tempfile::TempDir, Arc<Mutex<Ledger>>, MarkFeaturePassing) {
        let root = tempfile::tempdir().unwrap();
        let list: Vec<Value> = verifies
            .iter()
            .enumerate()
            .map(|(i, verify)| {
                json!({"id": format!("F{}", i + 1), "description": "d", "verify": verify, "passes": false})
            })
            .collect();
        fs::create_dir_all(dir(root.path())).unwrap();
        fs::write(
            crate::task::features_path(root.path()),
            Value::from(list).to_string(),
        )
        .unwrap();
        let ledger = Arc::new(Mutex::new(Ledger::load(root.path()).unwrap()));
        let tool = MarkFeaturePassing::new(
            ledger.clone(),
            Verifier::new(root.path().to_path_buf(), &ToolSettings::default()),
        );
        (root, ledger, tool)
    }

    #[tokio::test]
    async fn a_passing_verify_command_marks_the_feature() {
        let (root, ledger, tool) = setup(&["true", "false"]);
        let output = tool.run(json!({"id": "F1"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("1/2 features pass now"));
        assert!(ledger.lock().unwrap().get("F1").unwrap().passes);
        assert!(features::load(root.path()).unwrap()[0].passes);

        let again = tool.run(json!({"id": "F1"})).await;
        assert_eq!(again, ToolOutput::ok("F1 already passes."));
    }

    #[tokio::test]
    async fn a_failing_verify_command_changes_nothing() {
        let (root, ledger, tool) = setup(&["true", "echo not yet; false"]);
        let output = tool.run(json!({"id": "F2"})).await;
        assert!(output.is_error);
        assert!(
            output.content.contains("F2 does not pass"),
            "{}",
            output.content
        );
        assert!(output.content.contains("not yet\n[exit code 1]"));
        assert!(!ledger.lock().unwrap().get("F2").unwrap().passes);
        assert!(!features::load(root.path()).unwrap()[1].passes);
    }

    #[tokio::test]
    async fn a_failure_inside_a_pipeline_fails_the_verify_command() {
        let (_root, ledger, tool) =
            setup(&["(echo 'test result: FAILED'; exit 101) 2>&1 | tail -5"]);
        let output = tool.run(json!({"id": "F1"})).await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("[exit code 101]"),
            "{}",
            output.content
        );
        assert!(!ledger.lock().unwrap().all_pass());
    }

    #[tokio::test]
    async fn verify_runs_in_the_repository_root() {
        let (root, ledger, tool) = setup(&["test -f marker.txt"]);
        assert!(tool.run(json!({"id": "F1"})).await.is_error);
        fs::write(root.path().join("marker.txt"), "").unwrap();
        let output = tool.run(json!({"id": "F1"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert!(ledger.lock().unwrap().all_pass());
    }

    #[tokio::test]
    async fn unknown_ids_and_bad_input_are_errors() {
        let (_root, _ledger, tool) = setup(&["true", "true"]);
        let output = tool.run(json!({"id": "F9"})).await;
        assert!(output.is_error);
        assert!(output.content.contains("the ids are F1, F2"));
        assert!(tool.run(json!({})).await.is_error);
    }

    #[tokio::test]
    async fn verify_commands_follow_the_users_tool_settings() {
        let root = tempfile::tempdir().unwrap();
        let settings = ToolSettings {
            hidden_env: vec!["HOME".into()],
            deny_commands: vec!["touch".into()],
            allow_dangerous: false,
            settings_file: None,
            approval_gate: false,
            webhook: false,
            gh_config_dir: None,
            instruction_files: Vec::new(),
            sandbox: None,
        };
        let verifier = Verifier::new(root.path().to_path_buf(), &settings);
        let output = verifier.run("test -z \"$HOME\"").await;
        assert!(!output.is_error, "{}", output.content);
        let output = verifier.run("touch made.txt").await;
        assert!(
            output.content.starts_with("blocked by policy"),
            "{}",
            output.content
        );
        assert!(!root.path().join("made.txt").exists());
    }

    #[test]
    fn session_tools_are_the_default_four() {
        let names: Vec<String> = session_tools(&ToolSettings::default())
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            names,
            [
                "bash",
                "read_file",
                "write_file",
                "edit_file",
                "grep",
                "glob",
                "list_dir"
            ]
        );
    }

    #[test]
    fn quotes_paths_with_quotes() {
        assert_eq!(
            shell_quote(std::path::Path::new("/a b/it's")),
            r"'/a b/it'\''s'"
        );
    }
}
