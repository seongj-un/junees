//! `mark_feature_passing`: the only way a feature becomes passing. The
//! harness runs the feature's verify command itself and believes the exit
//! code, not the model.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::features::Ledger;
use crate::tools::{Bash, SECRET_ENV_VARS, Tool, ToolOutput};

/// Verify commands may run a whole test suite, so they get the longest
/// timeout the bash tool allows.
pub const VERIFY_TIMEOUT_SECS: u64 = 600;

pub struct MarkFeaturePassing {
    ledger: Arc<Mutex<Ledger>>,
    root: PathBuf,
    bash: Bash,
}

impl MarkFeaturePassing {
    /// Verify commands run in `root`, with the bash tool's output limits and
    /// without the API key variables.
    pub fn new(ledger: Arc<Mutex<Ledger>>, root: PathBuf) -> Self {
        Self {
            ledger,
            root,
            bash: Bash::default().hide_env(SECRET_ENV_VARS.iter().copied()),
        }
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

        // A newline, not `&&`, so the cd applies to the whole command.
        let command = format!("cd {} || exit 1\n{verify}", shell_quote(&self.root));
        let output = self
            .bash
            .run(json!({"command": command, "timeout_secs": VERIFY_TIMEOUT_SECS}))
            .await;
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
fn shell_quote(path: &std::path::Path) -> String {
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
        let tool = MarkFeaturePassing::new(ledger.clone(), root.path().to_path_buf());
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

    #[test]
    fn quotes_paths_with_quotes() {
        assert_eq!(
            shell_quote(std::path::Path::new("/a b/it's")),
            r"'/a b/it'\''s'"
        );
    }
}
