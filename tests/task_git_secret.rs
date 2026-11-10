//! The harness's own git commands must not hand the API key or Claude
//! Code's token to commands a session planted in the repository: hooks, or
//! a clean filter in `.git/config` that `git add` runs. This file holds a
//! single test because it changes the process environment.

use std::path::Path;
use std::process::Command;

use junees::task::git;

const KEY: &str = "sk-ant-test-planted-hook-must-not-see-this";
const CLAUDE_TOKEN: &str = "claude-code-oauth-test-value";

fn run_git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    for name in git::REPO_ENV_VARS {
        command.env_remove(name);
    }
    assert!(command.status().unwrap().success(), "git {args:?}");
}

#[test]
fn harness_git_runs_no_planted_hook_and_hides_the_credentials() {
    let repo = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(repo.path()).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.name", "Junees Test"],
        &["config", "user.email", "test@junees.invalid"],
        &["config", "commit.gpgsign", "false"],
    ] {
        run_git(&root, args);
    }
    let outside = tempfile::tempdir().unwrap();
    let leak = outside.path().join("leaked.txt");
    let record = format!(
        "printenv ANTHROPIC_API_KEY CLAUDE_CODE_OAUTH_TOKEN >> '{}'",
        leak.display()
    );

    // What a session can plant with its bash: a pre-commit hook, and a clean
    // filter that git runs on `git add` whatever the hook settings.
    let hook = root.join(".git/hooks/pre-commit");
    let hook_ran = outside.path().join("hook-ran");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\n{record}\n", hook_ran.display()),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&hook).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&hook, perms).unwrap();
    run_git(
        &root,
        &["config", "filter.leak.clean", &format!("{record}; cat")],
    );
    std::fs::write(root.join(".gitattributes"), "*.txt filter=leak\n").unwrap();
    std::fs::write(root.join("work.txt"), "work\n").unwrap();

    // SAFETY: this test binary runs only this test, so no other thread reads
    // the environment concurrently.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", KEY);
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", CLAUDE_TOKEN);
    }
    assert!(git::is_dirty(&root).unwrap());
    git::commit_all(&root, "junees: checkpoint after session 1").unwrap();
    assert!(!git::is_dirty(&root).unwrap());

    // The filter ran (it cannot be turned off without breaking the
    // repository's own attributes), but it saw no key, and the hook never ran.
    assert!(!hook_ran.exists(), "the planted pre-commit hook ran");
    let seen = std::fs::read_to_string(&leak).expect("the clean filter ran");
    assert!(!seen.contains(KEY), "the key leaked: {seen}");
    assert!(
        !seen.contains(CLAUDE_TOKEN),
        "the Claude Code token leaked: {seen}"
    );
}
