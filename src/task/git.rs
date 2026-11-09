//! The few git operations task mode needs, run with the `git` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::tools::SECRET_ENV_VARS;

/// Variables that point git at a repository other than the one in the
/// current folder, or hand it settings from a parent git process. Git hooks
/// and `git rebase --exec` set some of them (a worktree's rebase exports
/// `GIT_DIR`, and `git -c ...` passes its settings, `core.worktree` or
/// `core.hooksPath` among them, on in `GIT_CONFIG_PARAMETERS`). Task mode
/// always means the repository it found from its folder, with that
/// repository's own settings, so they are removed. Without
/// `GIT_CONFIG_COUNT` git ignores the `GIT_CONFIG_KEY_<n>` and
/// `GIT_CONFIG_VALUE_<n>` pairs.
pub const REPO_ENV_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Settings put in front of every git command the harness runs. A session
/// can write `.git/hooks` and `.git/config`, and the harness's git must not
/// run what it put there: no hooks and no fsmonitor command. Command-line
/// settings win over every config file.
const SAFE_CONFIG: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
];

/// Runs git in `dir` and returns its stdout, or an error with its stderr.
///
/// Git can still run commands from the repository's config (clean filters
/// on `git add`, for one), so it gets neither the repository variables nor
/// the API key, like the session's own bash.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(SAFE_CONFIG).args(args).current_dir(dir);
    for name in REPO_ENV_VARS.iter().chain(SECRET_ENV_VARS) {
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
    dirty(root, &[])
}

/// Like [`is_dirty`], but blind to the task folder `.junees/`.
pub fn is_dirty_outside_task(root: &Path) -> Result<bool> {
    dirty(root, &["--", ".", ":(exclude).junees"])
}

fn dirty(root: &Path, pathspec: &[&str]) -> Result<bool> {
    let mut args = vec!["status", "--porcelain", "--untracked-files=all"];
    args.extend(pathspec);
    Ok(!git(root, &args)?.trim().is_empty())
}

/// What [`commit_all`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Commit {
    /// Whether a commit was made; not when everything new was left out.
    pub made: bool,
    /// New files left out because their names say they may hold secrets.
    pub left_out: Vec<String>,
}

/// Stages everything (`git add -A`) except new files that look like
/// secrets (see [`looks_secret`]), and commits it.
pub fn commit_all(root: &Path, message: &str) -> Result<Commit> {
    git(root, &["add", "-A"])?;
    let added = git(
        root,
        &["diff", "--cached", "--name-only", "--diff-filter=A", "-z"],
    )?;
    let left_out: Vec<String> = added
        .split('\0')
        .filter(|path| !path.is_empty() && looks_secret(path))
        .map(String::from)
        .collect();
    if !left_out.is_empty() {
        let mut args = vec!["--literal-pathspecs", "rm", "--cached", "-q", "--"];
        args.extend(left_out.iter().map(String::as_str));
        git(root, &args)?;
    }
    let staged = git(root, &["diff", "--cached", "--name-only", "-z"])?;
    if staged.is_empty() {
        return Ok(Commit {
            made: false,
            left_out,
        });
    }
    git(root, &["commit", "-q", "--no-verify", "-m", message])?;
    Ok(Commit {
        made: true,
        left_out,
    })
}

/// Whether a file's name says it may hold a secret: `.env` files, keys and
/// certificates, ssh keys, `.netrc`. The harness does not commit new ones
/// on its own; a session or the user can still commit them on purpose.
pub fn looks_secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.starts_with(".env")
        || name == ".netrc"
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|ext| name.ends_with(ext))
        || ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
            .iter()
            .any(|key| name.starts_with(key))
}

/// `git log --oneline -<count>`, or a note when there is nothing to show.
pub fn recent_log(root: &Path, count: usize) -> String {
    match git(root, &["log", "--oneline", &format!("-{count}")]) {
        Ok(log) if !log.trim().is_empty() => log,
        _ => "(no commits yet)\n".to_string(),
    }
}

/// The oldest commit before HEAD whose subject is `subject`, if any.
/// Replacement refs (`git replace`) are ignored here and in [`show`]: a
/// session could plant one to swap the content of an old commit.
pub fn oldest_with_subject(root: &Path, subject: &str) -> Result<Option<String>> {
    let log = git(root, &["--no-replace-objects", "log", "--format=%H %s"])?;
    Ok(log
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, line_subject)| *line_subject == subject)
        .map(|(commit, _)| commit.to_string())
        .next_back())
}

/// The content of `path`, relative to the root, in `commit`.
pub fn show(root: &Path, commit: &str, path: &str) -> Result<String> {
    git(
        root,
        &["--no-replace-objects", "show", &format!("{commit}:{path}")],
    )
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

        std::fs::create_dir_all(root.join(".junees")).unwrap();
        std::fs::write(root.join(".junees/notes.md"), "n").unwrap();
        assert!(is_dirty(root).unwrap());
        assert!(!is_dirty_outside_task(root).unwrap());
        std::fs::write(root.join("b.txt"), "b").unwrap();
        assert!(is_dirty_outside_task(root).unwrap());
        commit_all(root, "second").unwrap();

        assert_eq!(commit_all(root, "nothing").unwrap(), Commit::default());
        assert_eq!(subjects(root), ["second", "first"]);
    }

    #[test]
    fn leaves_new_files_that_look_like_secrets_out() {
        let repo = repo();
        let root = repo.path();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        for name in [
            ".env",
            "conf/.env.local",
            "conf/server.pem",
            "id_ed25519",
            "main.rs",
        ] {
            std::fs::write(root.join(name), "x").unwrap();
        }
        let commit = commit_all(root, "first").unwrap();
        assert!(commit.made);
        assert_eq!(
            commit.left_out,
            [".env", "conf/.env.local", "conf/server.pem", "id_ed25519"]
        );
        let tracked = git(root, &["ls-files"]).unwrap();
        assert_eq!(tracked, "main.rs\n");

        // Only those left: no commit, and they stay where they are.
        let commit = commit_all(root, "second").unwrap();
        assert!(!commit.made);
        assert_eq!(commit.left_out.len(), 4);
        assert_eq!(subjects(root), ["first"]);
        assert!(root.join(".env").exists());

        // A file the user already tracks is theirs to commit.
        git(root, &["add", "-f", ".env"]).unwrap();
        git(root, &["commit", "-q", "-m", "track env"]).unwrap();
        std::fs::write(root.join(".env"), "y").unwrap();
        let commit = commit_all(root, "third").unwrap();
        assert!(commit.made);
        assert_eq!(commit.left_out.len(), 3);
    }

    #[test]
    fn finds_the_oldest_commit_with_a_subject_and_ignores_replacements() {
        let repo = repo();
        let root = repo.path();
        assert!(oldest_with_subject(root, "init").is_err());
        std::fs::write(root.join("a.txt"), "first").unwrap();
        commit_all(root, "init").unwrap();
        let first = git(root, &["rev-parse", "HEAD"]).unwrap();
        let first = first.trim();
        std::fs::write(root.join("a.txt"), "second").unwrap();
        commit_all(root, "init").unwrap();
        std::fs::write(root.join("a.txt"), "third").unwrap();
        commit_all(root, "init again").unwrap();
        assert_eq!(
            oldest_with_subject(root, "init").unwrap().as_deref(),
            Some(first)
        );
        assert_eq!(oldest_with_subject(root, "other").unwrap(), None);
        assert_eq!(show(root, first, "a.txt").unwrap(), "first");

        // A replacement ref does not swap the content read from it.
        let head = git(root, &["rev-parse", "HEAD"]).unwrap();
        git(root, &["replace", first, head.trim()]).unwrap();
        assert_eq!(show(root, first, "a.txt").unwrap(), "first");
        assert!(show(root, first, "missing.txt").is_err());
    }

    #[test]
    fn secret_names() {
        for name in [
            ".env",
            ".env.production",
            "a/b/.envrc",
            "tls.key",
            "ca.pem",
            "id_rsa",
            "id_rsa.pub",
            ".netrc",
        ] {
            assert!(looks_secret(name), "{name}");
        }
        for name in [
            "env.rs",
            "src/environment.md",
            "id_card.rs",
            "keys.rs",
            "pem.txt",
            "README.md",
        ] {
            assert!(!looks_secret(name), "{name}");
        }
    }

    #[test]
    fn runs_no_hook_or_fsmonitor_a_session_planted() {
        let repo = repo();
        let root = repo.path();
        let marker = root.join("ran.txt");
        let script = format!("#!/bin/sh\necho \"$0\" >> '{}'\n", marker.display());
        let hooks = root.join(".git/hooks");
        let custom = root.join("custom-hooks");
        std::fs::create_dir_all(&custom).unwrap();
        for dir in [&hooks, &custom] {
            for name in ["pre-commit", "commit-msg", "post-commit", "fsmonitor"] {
                let path = dir.join(name);
                std::fs::write(&path, &script).unwrap();
                let mut perms = std::fs::metadata(&path).unwrap().permissions();
                std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
                std::fs::set_permissions(&path, perms).unwrap();
            }
        }
        std::fs::write(root.join("a.txt"), "a").unwrap();
        commit_all(root, "with default hooks").unwrap();
        // A hooks folder and an fsmonitor named in the repository's config.
        git(root, &["config", "core.hooksPath", "custom-hooks"]).unwrap();
        let fsmonitor = custom.join("fsmonitor");
        git(
            root,
            &["config", "core.fsmonitor", &fsmonitor.to_string_lossy()],
        )
        .unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        assert!(is_dirty(root).unwrap());
        commit_all(root, "with configured hooks").unwrap();
        assert!(!is_dirty(root).unwrap());
        assert!(
            !marker.exists(),
            "{}",
            std::fs::read_to_string(&marker).unwrap()
        );
        assert_eq!(
            subjects(root),
            ["with configured hooks", "with default hooks"]
        );
    }
}
