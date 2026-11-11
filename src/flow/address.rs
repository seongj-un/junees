//! The git side of addressing a review: a worktree of the pull request's
//! head commit, a commit of the model's changes, the test command and a
//! plain push (never forced) to the branch.
//!
//! Git runs like task mode's ([`crate::task::git`]): no hooks, no
//! fsmonitor, without the repository variables or any credential Junees
//! holds. The push uses the user's own git credentials (credential helper,
//! ssh agent): Junees never hands its GitHub token to git. It pushes to the
//! `origin` URL read before the model ran, so a changed remote in the
//! repository's config cannot send the commit elsewhere.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::github::{Repo, parse_remote};
use crate::task::git::{self, git as run_git};

/// The folder of the worktrees, inside the route's folder.
pub const WORKTREES: &str = ".junees-worktrees";
/// Characters of the test output kept for the comment.
pub const TEST_TAIL: usize = 3_000;

/// A worktree ready for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// `origin` as configured (before the model ran).
    pub origin: String,
}

/// Checks a branch name from GitHub before it goes into a refspec.
pub fn check_branch(branch: &str) -> Result<()> {
    let ok = !branch.is_empty()
        && branch.len() <= 255
        && !branch.starts_with(['-', '/'])
        && !branch.contains("..")
        && !branch
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c));
    if !ok {
        bail!(
            "the branch name `{}` is not one Junees pushes to",
            crate::prompt_text::escape_hidden(branch)
        );
    }
    run_git(
        Path::new("/"),
        &["check-ref-format", &format!("refs/heads/{branch}")],
    )
    .map(|_| ())
    .with_context(|| format!("`{branch}` is not a valid branch name"))
}

/// The `origin` URL of the clone `dir` when it is `repo` on GitHub.
pub fn origin_of(dir: &Path, repo: &Repo) -> Result<String> {
    let url = run_git(dir, &["config", "--get", "remote.origin.url"])
        .with_context(|| format!("{} has no `origin` remote", dir.display()))?
        .trim()
        .to_string();
    match parse_remote(&url) {
        Some((_, found)) if found == *repo => Ok(url),
        _ => bail!(
            "the origin of {} is not {repo}; --address-reviews needs a clone of it",
            dir.display()
        ),
    }
}

/// Fetches `branch` and checks out its head `sha` in
/// `<dir>/.junees-worktrees/pr-<number>` (replacing an earlier one).
pub fn prepare(dir: &Path, repo: &Repo, number: u64, branch: &str, sha: &str) -> Result<Worktree> {
    check_branch(branch)?;
    let origin = origin_of(dir, repo)?;
    run_git(
        dir,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            &origin,
            &format!("refs/heads/{branch}"),
        ],
    )?;
    let fetched = run_git(dir, &["rev-parse", "FETCH_HEAD"])?
        .trim()
        .to_string();
    if fetched != sha {
        bail!(
            "the branch is at {} now, not {}; the newer push has its own event",
            super::merge::short(&fetched),
            super::merge::short(sha)
        );
    }
    exclude(dir)?;
    let path = dir.join(WORKTREES).join(format!("pr-{number}"));
    if path.exists() {
        let shown = path.to_string_lossy().to_string();
        if run_git(dir, &["worktree", "remove", "--force", &shown]).is_err() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
        }
    }
    run_git(dir, &["worktree", "prune"])?;
    let shown = path.to_string_lossy().to_string();
    run_git(
        dir,
        &["worktree", "add", "--quiet", "--detach", &shown, sha],
    )?;
    Ok(Worktree { path, origin })
}

/// Lists the worktrees folder in the repository's `info/exclude`, so the
/// clone does not look changed.
fn exclude(dir: &Path) -> Result<()> {
    let common = run_git(dir, &["rev-parse", "--git-common-dir"])?;
    let common = dir.join(common.trim());
    let info = common.join("info");
    std::fs::create_dir_all(&info).with_context(|| format!("cannot create {}", info.display()))?;
    let path = info.join("exclude");
    let line = format!("/{WORKTREES}/");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|known| known.trim() == line) {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    let start = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    writeln!(file, "{start}{line}").with_context(|| format!("cannot write {}", path.display()))
}

/// Commits everything the model changed (new secret-looking or very large
/// files left out); the new head, or `None` when nothing changed.
pub fn commit(worktree: &Path, message: &str) -> Result<Option<String>> {
    let commit = git::commit_all(worktree, message)?;
    if !commit.made {
        return Ok(None);
    }
    Ok(Some(head(worktree)?))
}

/// The worktree's head commit.
pub fn head(worktree: &Path) -> Result<String> {
    Ok(run_git(worktree, &["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// Pushes the worktree's head to `branch` of `origin`, without force.
pub fn push(worktree: &Path, origin: &str, branch: &str) -> Result<()> {
    check_branch(branch)?;
    run_git(
        worktree,
        &[
            "push",
            "--quiet",
            "--no-verify",
            origin,
            &format!("HEAD:refs/heads/{branch}"),
        ],
    )
    .map(|_| ())
}

/// The test command: `sh -c` in `worktree`, in a process group of its
/// own, without the repository variables, Junees's credentials and the
/// variables in `hidden`.
fn test_command(worktree: &Path, command: &str, hidden: &[String]) -> tokio::process::Command {
    let mut child = tokio::process::Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    for name in git::REPO_ENV_VARS
        .iter()
        .chain(crate::tools::SECRET_ENV_VARS)
        .chain(crate::tools::CLAUDE_CODE_SECRET_ENV_VARS)
    {
        child.env_remove(name);
    }
    for name in hidden {
        child.env_remove(name);
    }
    child
}

/// How the test command went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tested {
    pub passed: bool,
    /// `exit status: 1`, `timed out after 600 s`.
    pub how: String,
    /// The end of its output (stdout and stderr), hidden characters
    /// escaped.
    pub tail: String,
}

/// Runs the [`test_command`] with its output to `log`; kills its group
/// after `limit`.
pub async fn test(
    worktree: &Path,
    command: &str,
    log: &Path,
    hidden: &[String],
    limit: Duration,
) -> Result<Tested> {
    if let Some(dir) = log.parent() {
        crate::cron::store::create_private_dir(dir)?;
    }
    let mut out = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(log)
        .with_context(|| format!("cannot write {}", log.display()))?;
    let mut child = test_command(worktree, command, hidden);
    child.stdout(out.try_clone()?).stderr(out.try_clone()?);
    let mut child = child.spawn().context("cannot start the test command")?;
    let pid = child.id().map(|pid| pid as i32);
    let how = match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) if status.success() => None,
        Ok(Ok(status)) => Some(status.to_string()),
        Ok(Err(err)) => Some(format!("cannot wait for it: {err}")),
        Err(_) => {
            if let Some(pid) = pid {
                crate::cron::daemon::signal_group(pid, libc::SIGKILL);
            }
            let _ = child.wait().await;
            Some(format!("timed out after {} s", limit.as_secs()))
        }
    };
    let mut text = Vec::new();
    let length = out.seek(SeekFrom::End(0))?;
    out.seek(SeekFrom::Start(length.saturating_sub(TEST_TAIL as u64 * 4)))?;
    out.read_to_end(&mut text)?;
    let text = String::from_utf8_lossy(&text);
    let chars: Vec<char> = text.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(TEST_TAIL)..]
        .iter()
        .collect();
    Ok(Tested {
        passed: how.is_none(),
        how: how.unwrap_or_else(|| "exit status: 0".to_string()),
        tail: crate::prompt_text::escape_hidden(tail.trim()),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::process::Command;

    use super::*;

    /// Runs git in `dir` for a test (without the repository variables).
    pub fn sh_git(dir: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.args(args).current_dir(dir);
        git::hide_env(&mut command);
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare repository standing in for GitHub's `o/n` (branch `feature`
    /// one commit past `main`) and a clone of it whose origin reads
    /// `https://git.invalid/o/n.git` (git rewrites it to the bare one).
    /// Returns (root, clone, bare, head of feature).
    pub fn remote_and_clone() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        let seed = root.path().join("seed");
        let clone = root.path().join("clone");
        std::fs::create_dir(&seed).unwrap();
        sh_git(
            root.path(),
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        sh_git(&seed, &["init", "-q", "-b", "main"]);
        for (key, value) in [("user.name", "Test"), ("user.email", "t@example.com")] {
            sh_git(&seed, &["config", key, value]);
        }
        std::fs::write(seed.join("lib.txt"), "one\n").unwrap();
        sh_git(&seed, &["add", "."]);
        sh_git(&seed, &["commit", "-q", "-m", "init"]);
        sh_git(&seed, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(seed.join("lib.txt"), "one\ntwo\n").unwrap();
        sh_git(&seed, &["commit", "-q", "-am", "feature"]);
        let bare_text = bare.to_string_lossy().to_string();
        sh_git(&seed, &["push", "-q", &bare_text, "main", "feature"]);
        let head = sh_git(&seed, &["rev-parse", "HEAD"]);
        sh_git(root.path(), &["clone", "-q", &bare_text, "clone"]);
        sh_git(
            &clone,
            &["remote", "set-url", "origin", "https://git.invalid/o/n.git"],
        );
        sh_git(
            &clone,
            &[
                "config",
                &format!("url.{bare_text}.insteadOf"),
                "https://git.invalid/o/n.git",
            ],
        );
        for (key, value) in [("user.name", "Junees"), ("user.email", "j@example.com")] {
            sh_git(&clone, &["config", key, value]);
        }
        (root, clone, bare, head)
    }

    #[test]
    fn checks_branch_names_before_they_reach_a_refspec() {
        for good in ["feature", "fix/parser-2", "a.b_c"] {
            check_branch(good).unwrap();
        }
        for bad in [
            "", "-f", "--force", "a..b", "a b", "a:b", "x\u{7}", "/abs", "a.lock", "end/",
        ] {
            assert!(check_branch(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn makes_a_worktree_at_the_head_commits_and_pushes_without_force() {
        let (_root, clone, bare, head) = remote_and_clone();
        let repo = Repo::parse("o/n").unwrap();
        let other = Repo::parse("o/other").unwrap();
        assert!(origin_of(&clone, &other).is_err());
        let err = prepare(&clone, &repo, 7, "feature", &"0".repeat(40)).unwrap_err();
        assert!(err.to_string().contains("the newer push"), "{err}");
        let tree = prepare(&clone, &repo, 7, "feature", &head).unwrap();
        assert_eq!(tree.path, clone.join(".junees-worktrees/pr-7"));
        assert_eq!(tree.origin, "https://git.invalid/o/n.git");
        assert_eq!(super::head(&tree.path).unwrap(), head);
        // The clone does not look changed.
        assert_eq!(sh_git(&clone, &["status", "--porcelain"]), "");
        // Again (an earlier worktree is replaced).
        std::fs::write(tree.path.join("junk"), "x").unwrap();
        let tree = prepare(&clone, &repo, 7, "feature", &head).unwrap();
        assert!(!tree.path.join("junk").exists());

        assert_eq!(commit(&tree.path, "nothing").unwrap(), None);
        std::fs::write(tree.path.join("lib.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(tree.path.join(".env"), "TOKEN=x\n").unwrap();
        let new = commit(&tree.path, "fix: address review").unwrap().unwrap();
        push(&tree.path, &tree.origin, "feature").unwrap();
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature"]), new);
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature~1"]), head);
        let files = sh_git(&bare, &["ls-tree", "--name-only", "feature"]);
        assert_eq!(files, "lib.txt", "the .env file stays out");

        // A push over a branch that moved meanwhile is refused (no force).
        let tree = prepare(&clone, &repo, 8, "feature", &new).unwrap();
        sh_git(&tree.path, &["checkout", "-q", "--detach", &head]);
        std::fs::write(tree.path.join("lib.txt"), "other\n").unwrap();
        commit(&tree.path, "diverge").unwrap().unwrap();
        assert!(push(&tree.path, &tree.origin, "feature").is_err());
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature"]), new);
    }

    #[test]
    fn the_test_command_gets_no_credentials() {
        let hidden = vec!["DB_PASSWORD".to_string()];
        let command = test_command(Path::new("/"), "true", &hidden);
        let removed: Vec<String> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().to_string())
            .collect();
        for name in [
            "ANTHROPIC_API_KEY",
            "JUNEES_GITHUB_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "GIT_DIR",
            "DB_PASSWORD",
        ] {
            assert!(removed.iter().any(|r| r == name), "{name}: {removed:?}");
        }
    }

    #[tokio::test]
    async fn the_test_command_runs_without_credentials_and_with_a_time_limit() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/t.log");
        let hidden = vec!["PATH_SECRET_X".to_string()];
        let passed = test(
            dir.path(),
            "pwd; echo \"key=[$ANTHROPIC_API_KEY] gh=[$JUNEES_GITHUB_TOKEN] x=[$PATH_SECRET_X]\"",
            &log,
            &hidden,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(passed.passed, "{passed:?}");
        assert!(passed.tail.contains("key=[] gh=[] x=[]"), "{passed:?}");
        assert!(
            passed.tail.contains(&*dir.path().to_string_lossy()),
            "{passed:?}"
        );
        let failed = test(
            dir.path(),
            "echo 실패 >&2; exit 3",
            &log,
            &[],
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(!failed.passed);
        assert_eq!(failed.how, "exit status: 3");
        assert_eq!(failed.tail, "실패");
        let started = std::time::Instant::now();
        let slow = test(
            dir.path(),
            "sleep 30",
            &log,
            &[],
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert!(!slow.passed);
        assert!(slow.how.starts_with("timed out"), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
