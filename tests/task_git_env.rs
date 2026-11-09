//! Task mode must work on the repository it was started in even when the
//! environment names another one, as `git rebase --exec` does in a worktree
//! (`GIT_DIR`) and git hooks do. This file holds a single test because it
//! changes the process environment.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use junees::task::features::Ledger;
use junees::task::git;
use junees::task::tools::{MarkFeaturePassing, session_tools};
use junees::tools::Tool;
use serde_json::json;

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn new_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q"]);
    run_git(dir.path(), &["config", "user.name", "Junees Test"]);
    run_git(dir.path(), &["config", "user.email", "test@junees.invalid"]);
    run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
    dir
}

#[tokio::test]
async fn git_dir_in_the_environment_does_not_redirect_task_mode() {
    let task = new_repo();
    let decoy = new_repo();
    let root = std::fs::canonicalize(task.path()).unwrap();
    // SAFETY: this test binary runs only this test, so no other thread reads
    // the environment concurrently.
    unsafe {
        std::env::set_var("GIT_DIR", decoy.path().join(".git"));
    }

    // The harness's own git commands.
    assert_eq!(git::toplevel(&root).unwrap(), root);
    std::fs::create_dir_all(root.join(".junees")).unwrap();
    std::fs::write(
        root.join(".junees/features.json"),
        json!([{"id": "F1", "description": "d", "verify": "git log --format=%s | grep -qx first", "passes": false}])
            .to_string(),
    )
    .unwrap();
    git::commit_all(&root, "first").unwrap();
    assert!(!git::is_dirty(&root).unwrap());

    // The session's bash.
    let output = session_tools()
        .dispatch(
            "bash",
            json!({"command": format!("cd '{}' && git log --format=%s", root.display())}),
        )
        .await;
    assert_eq!(output.content, "first\n[exit code 0]");

    // Verify commands.
    let ledger = Arc::new(Mutex::new(Ledger::load(&root).unwrap()));
    let output = MarkFeaturePassing::new(ledger.clone(), root.clone())
        .run(json!({"id": "F1"}))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(ledger.lock().unwrap().all_pass());

    // Nothing reached the other repository.
    let decoy_log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(decoy.path())
        .env_remove("GIT_DIR")
        .output()
        .unwrap();
    assert!(
        !decoy_log.status.success(),
        "the decoy repository got a commit"
    );
}
