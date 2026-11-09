//! The few git operations task mode needs, run with the `git` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Variables that point git at a repository other than the one in the
/// current folder. Git hooks and `git rebase --exec` set some of them (a
/// worktree's rebase exports `GIT_DIR`), and task mode always means the
/// repository it found from its folder, so they are removed.
pub const REPO_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Runs git in `dir` and returns its stdout, or an error with its stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    for name in REPO_ENV_VARS {
        command.env_remove(name);
    }
    let output = command.output().context("cannot run git")?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The root of the git work tree `dir` is in. Task mode does not create
/// repositories on its own, so outside one this is an error.
pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let root = git(dir, &["rev-parse", "--show-toplevel"]).with_context(|| {
        format!(
            "{} is not inside a git repository (run `git init` first)",
            dir.display()
        )
    })?;
    Ok(PathBuf::from(root.trim_end_matches('\n')))
}

/// Whether the work tree has changes or untracked files that are not ignored.
pub fn is_dirty(root: &Path) -> Result<bool> {
    let status = git(root, &["status", "--porcelain", "--untracked-files=all"])?;
    Ok(!status.trim().is_empty())
}

/// Stages everything (`git add -A`) and commits it.
pub fn commit_all(root: &Path, message: &str) -> Result<()> {
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-q", "-m", message])?;
    Ok(())
}

/// `git log --oneline -<count>`, or a note when there is nothing to show.
pub fn recent_log(root: &Path, count: usize) -> String {
    match git(root, &["log", "--oneline", &format!("-{count}")]) {
        Ok(log) if !log.trim().is_empty() => log,
        _ => "(no commits yet)\n".to_string(),
    }
}

/// Test helpers shared by the task tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A new repository with a local identity, so commits work anywhere.
    pub fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Junees Test"],
            &["config", "user.email", "test@junees.invalid"],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(dir.path(), args).unwrap();
        }
        dir
    }

    /// Commit subjects, newest first.
    pub fn subjects(root: &Path) -> Vec<String> {
        git(root, &["log", "--format=%s"])
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn toplevel_finds_the_root_and_refuses_other_folders() {
        let repo = repo();
        let sub = repo.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let root = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(toplevel(&sub).unwrap(), root);

        let plain = tempfile::tempdir().unwrap();
        let err = toplevel(plain.path()).unwrap_err();
        assert!(
            format!("{err}").contains("not inside a git repository"),
            "{err}"
        );
    }

    #[test]
    fn commits_a_dirty_tree() {
        let repo = repo();
        let root = repo.path();
        assert!(recent_log(root, 5).contains("no commits yet"));
        assert!(!is_dirty(root).unwrap());

        std::fs::write(root.join("a.txt"), "a").unwrap();
        assert!(is_dirty(root).unwrap());
        commit_all(root, "first").unwrap();
        assert!(!is_dirty(root).unwrap());
        assert_eq!(subjects(root), ["first"]);
        assert!(recent_log(root, 5).ends_with(" first\n"));

        let err = commit_all(root, "nothing").unwrap_err();
        assert!(err.to_string().contains("git commit"), "{err}");
    }
}
