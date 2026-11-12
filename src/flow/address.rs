//! The git side of addressing a review: a worktree of the pull request's
//! head commit, a commit of the model's changes, the test command and a
//! plain push (never forced) to the branch.
//!
//! Git runs like task mode's ([`crate::task::git`]): no hooks, no
//! fsmonitor, no signing, without the repository variables or any
//! credential Junees holds. After the model ran, it runs with the
//! worktree's git folder fixed (`GIT_DIR`, [`Worktree::repo`]) as git
//! found it before: the model can rewrite the worktree's `.git` file to
//! lead into a copy of the repository's folder whose config names a clean
//! filter, and git would run that program outside the sandbox on the
//! commit. The commit stops when the `.git` file leads elsewhere now
//! ([`Worktree::check_git_dirs`]). The folders it leads to before are the
//! route clone's, which the model's sandbox does not let it write, so
//! their filters are the user's own; the [`ConfigSnapshot`] catches a
//! change where the sandbox is off. The commit itself runs unconfined:
//! confining it would mean letting the sandbox write the clone's git
//! folder, where a filter could then plant settings for the next fetch
//! and checkout. The push uses the user's own git credentials (credential helper,
//! ssh agent): Junees never hands its GitHub token to git. It pushes to the
//! `origin` URL read before the model ran, so a changed `remote.origin.url`
//! cannot send the commit elsewhere. Other settings still apply to a URL
//! given on the command line (`url.<x>.pushInsteadOf`, `credential.helper`),
//! so the flow takes a [`ConfigSnapshot`] of every git config file the
//! push reads before the model runs and pushes nothing when one changed
//! ([`ConfigSnapshot::changes`]). It does not undo the change: it names
//! the keys for the user to check. The model's bash may not write git's
//! configuration in the first place ([`crate::tools::CommandPolicy::webhook_run`]).

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::github::{Repo, parse_remote};
use crate::sandbox::{Prepared, Sandbox};
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
    /// The worktree's own git folder and the repository's common one, as
    /// git found them before the model ran.
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

impl Worktree {
    /// Git in the worktree with its git folder fixed at [`Self::git_dir`],
    /// whatever its `.git` file says by now.
    pub fn repo(&self) -> git::Repo {
        git::Repo::pinned(&self.path, &self.git_dir)
    }

    /// Fails when git finds other git folders for the worktree than
    /// before the model ran: a rewritten `.git` file, or a changed
    /// `commondir` in the worktree's own git folder.
    pub fn check_git_dirs(&self) -> Result<()> {
        let found = git_dirs(&git::Repo::unconfined(&self.path))?;
        let pinned = git_dirs(&self.repo())?;
        let expected = (self.git_dir.clone(), self.common_dir.clone());
        for (now, how) in [
            (found, "the worktree's .git file"),
            (pinned, "its git folder"),
        ] {
            if now != expected {
                bail!(
                    "{how} now leads to the git folders {} and {}, not {} and {} as before the \
                     model ran; nothing was committed or pushed (check the worktree {})",
                    now.0.display(),
                    now.1.display(),
                    expected.0.display(),
                    expected.1.display(),
                    self.path.display()
                );
            }
        }
        Ok(())
    }
}

/// The git folder and common git folder git finds for the work tree of
/// `repo`, absolute.
fn git_dirs(repo: &git::Repo) -> Result<(PathBuf, PathBuf)> {
    let out = repo.git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
    ])?;
    let mut lines = out.lines();
    match (lines.next(), lines.next()) {
        (Some(own), Some(common)) => Ok((PathBuf::from(own), PathBuf::from(common))),
        _ => bail!(
            "`git rev-parse` named no git folders in {}",
            repo.root().display()
        ),
    }
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
    let (git_dir, common_dir) = git_dirs(&git::Repo::unconfined(&path))?;
    Ok(Worktree {
        path,
        origin,
        git_dir,
        common_dir,
    })
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
/// files left out) with the git folder fixed ([`Worktree::repo`]), after
/// checking the worktree still leads to it ([`Worktree::check_git_dirs`]);
/// the new head, or `None` when nothing changed.
pub fn commit(worktree: &Worktree, message: &str) -> Result<Option<String>> {
    worktree.check_git_dirs()?;
    let commit = worktree.repo().commit_all(message)?;
    if !commit.made {
        return Ok(None);
    }
    Ok(Some(head(worktree)?))
}

/// The files under `.github/` ([`crate::flow::merge::GITHUB_DIR`]) that
/// the worktree's head changes from `base` (old and new names of a
/// rename). The flow does not push such a change: a workflow it pushes
/// runs with the repository's secrets.
pub fn github_changes(worktree: &Worktree, base: &str) -> Result<Vec<String>> {
    let names = worktree.repo().git(&[
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        base,
        "HEAD",
    ])?;
    Ok(names
        .split('\0')
        .filter(|name| name.starts_with(crate::flow::merge::GITHUB_DIR))
        .map(str::to_string)
        .collect())
}

/// The worktree's head commit.
pub fn head(worktree: &Worktree) -> Result<String> {
    Ok(worktree
        .repo()
        .git(&["rev-parse", "HEAD"])?
        .trim()
        .to_string())
}

/// Pushes the worktree's head to `branch` of its `origin` (as read before
/// the model ran), without force.
pub fn push(worktree: &Worktree, branch: &str) -> Result<()> {
    check_branch(branch)?;
    worktree
        .repo()
        .git(&[
            "push",
            "--quiet",
            "--no-verify",
            &worktree.origin,
            &format!("HEAD:refs/heads/{branch}"),
        ])
        .map(|_| ())
}

/// How many levels of `include.path` and `includeIf.*.path`
/// [`ConfigSnapshot::take`] follows (git itself stops at 10).
const INCLUDE_DEPTH: usize = 5;

/// The git config files a push from a worktree reads, besides the
/// system's (root's to change), with their contents at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSnapshot {
    /// Each file and its bytes (`None`: not there).
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl ConfigSnapshot {
    /// The repository's `config` and `config.worktree`, the worktree's own
    /// `config.worktree`, and the user's `~/.gitconfig` and
    /// `$XDG_CONFIG_HOME/git/config` (`~/.config/git/config`) as they are
    /// now, and the files they include (`include.path`, and
    /// `includeIf.<condition>.path` whatever the condition), up to
    /// [`INCLUDE_DEPTH`] levels deep.
    pub fn take(worktree: &Path) -> Result<Self> {
        let folder = |args: &[&str]| -> Result<PathBuf> {
            Ok(worktree.join(run_git(worktree, args)?.trim()))
        };
        let common = folder(&["rev-parse", "--git-common-dir"])?;
        let own = folder(&["rev-parse", "--git-dir"])?;
        let mut paths = vec![
            common.join("config"),
            common.join("config.worktree"),
            own.join("config.worktree"),
        ];
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = &home {
            paths.push(home.join(".gitconfig"));
        }
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(".config")));
        if let Some(xdg) = xdg {
            paths.push(xdg.join("git/config"));
        }
        let mut files: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
        let mut queue: Vec<(PathBuf, usize)> = paths.into_iter().map(|path| (path, 0)).collect();
        let mut next = 0;
        while let Some((path, depth)) = queue.get(next).cloned() {
            next += 1;
            if files.iter().any(|(known, _)| *known == path) {
                continue;
            }
            let bytes = read_config(&path)?;
            if depth < INCLUDE_DEPTH {
                for include in includes(&path, bytes.as_deref(), home.as_deref()) {
                    queue.push((include, depth + 1));
                }
            }
            files.push((path, bytes));
        }
        Ok(Self { files })
    }

    /// What changed since: `<file>: <key>, <key>` per changed file (key
    /// names, never values, which may hold credentials); empty when
    /// nothing did.
    pub fn changes(&self) -> Result<Vec<String>> {
        let mut changed = Vec::new();
        for (path, before) in &self.files {
            let now = read_config(path)?;
            if now == *before {
                continue;
            }
            let keys = match (
                config_entries(before.as_deref()),
                config_entries(now.as_deref()),
            ) {
                (Some(before), Some(now)) => {
                    let mut keys: Vec<&String> = before
                        .keys()
                        .chain(now.keys())
                        .filter(|key| before.get(*key) != now.get(*key))
                        .collect();
                    keys.sort();
                    keys.dedup();
                    if keys.is_empty() {
                        "no setting (layout or comments only)".to_string()
                    } else {
                        keys.iter()
                            .map(|key| crate::prompt_text::escape_hidden(key))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                }
                _ => "the file is no longer readable as git config".to_string(),
            };
            changed.push(format!("{}: {keys}", path.display()));
        }
        Ok(changed)
    }
}

/// The files config file `path` (its bytes `bytes`) includes, resolved as
/// git does (git-config(1), "Includes"): `~/` is the home folder `home`,
/// a relative path starts in the including file's folder.
fn includes(path: &Path, bytes: Option<&[u8]>, home: Option<&Path>) -> Vec<PathBuf> {
    let Some(entries) = config_entries(bytes) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|(key, _)| {
            *key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"))
        })
        .flat_map(|(_, values)| values)
        .filter_map(|value| include_path(path, value, home))
        .collect()
}

/// Where include `value` of config file `from` points.
fn include_path(from: &Path, value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = value.strip_prefix("~/") {
        return home.map(|home| home.join(rest));
    }
    let path = Path::new(value);
    if value.is_empty() || value.starts_with('~') {
        // `~user/`: another user's home, not one this user writes.
        None
    } else if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        from.parent().map(|dir| dir.join(path))
    }
}

/// The bytes of config file `path`, `None` when it is not there.
fn read_config(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The settings in config text `bytes` (key -> values in order), read by
/// git itself (`git config --file - --no-includes --list --null`: a
/// relative include in text from standard input is an error otherwise);
/// `None` when git cannot read it.
fn config_entries(bytes: Option<&[u8]>) -> Option<std::collections::BTreeMap<String, Vec<String>>> {
    use std::io::Write as _;
    let mut entries = std::collections::BTreeMap::new();
    let Some(bytes) = bytes else {
        return Some(entries);
    };
    let mut command = std::process::Command::new("git");
    command
        .args(["config", "--file", "-", "--no-includes", "--list", "--null"])
        .current_dir("/")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    git::hide_env(&mut command);
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let input = bytes.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().ok()?;
    let _ = writer.join();
    if !output.status.success() {
        return None;
    }
    for entry in output
        .stdout
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
    {
        let text = String::from_utf8_lossy(entry);
        let (key, value) = text.split_once('\n').unwrap_or((&text, ""));
        entries
            .entry(key.to_string())
            .or_insert_with(Vec::new)
            .push(value.to_string());
    }
    Some(entries)
}

/// The test command: `sh -c` in `worktree`, in a session and process
/// group of its own ([`crate::tools::own_session`]), without the repository variables, Junees's credentials, the
/// variables in `hidden`, and the user's git and GitHub CLI credentials
/// like the model's shell ([`crate::tools::strip_git_credentials`], with
/// `gh_config` as the GitHub CLI folder): it runs code of the pull request
/// and of the model.
fn test_command(
    worktree: &Path,
    command: &str,
    hidden: &[String],
    gh_config: &Path,
) -> tokio::process::Command {
    let mut child = tokio::process::Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::tools::own_session(&mut child);
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
    crate::tools::strip_git_credentials(&mut child, Some(gh_config));
    child
}

/// The sandbox of the test command: the one a webhook run's bash gets
/// from `tools` (`require` unless `sandbox` is set; the settings file,
/// secrets and git and GitHub CLI credentials out of reach, the data
/// folders read-only), so the pull request's code it runs gets no more
/// than the model's commands. `None` with `sandbox = off`.
pub fn test_sandbox(tools: &crate::tools::ToolSettings, gh_config: &Path) -> Option<Sandbox> {
    let trigger = crate::cron::run::Trigger::Webhook {
        route: "",
        github_allow: &[],
        gh_config,
        context_dir: Path::new("/"),
        approvers: &[],
    };
    Sandbox::for_tools(&trigger.tool_settings(tools))
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

/// Runs the [`test_command`] with its output to `log`, confined by
/// `sandbox` ([`test_sandbox`]); kills its group after `limit`. Fails
/// without running it when the sandbox refuses.
pub async fn test(
    worktree: &Path,
    command: &str,
    log: &Path,
    hidden: &[String],
    gh_config: &Path,
    sandbox: Option<&Sandbox>,
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
    let mut child = test_command(worktree, command, hidden, gh_config);
    // The confinement is kept until the child is spawned.
    let (confinement, note) = match sandbox.map(|sandbox| sandbox.prepare(worktree)) {
        None => (None, None),
        Some(Prepared::Confined(confinement, note)) => (Some(confinement), note),
        Some(Prepared::Unconfined(note)) => (None, note),
        Some(Prepared::Refused(reason)) => bail!("the test command was not run: {reason}"),
    };
    if let Some(confinement) = &confinement {
        confinement.apply(&mut child);
    }
    if let Some(note) = note {
        writeln!(out, "{note}")?;
    }
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

    /// Pushes `refs` from the repository `dir` to the bare repository
    /// `bare`, named by its path, so no remote or upstream of any
    /// repository counts. Both must sit in the system temp folder and
    /// apart from the crate, so a test push never reaches the crate's own
    /// repository or its origin. The only test push `tools::test_scan`
    /// allows.
    pub fn push_to_temp_bare(dir: &Path, bare: &Path, refs: &[&str]) {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        for path in [dir, bare] {
            let path = path.canonicalize().unwrap();
            assert!(
                path.starts_with(&temp)
                    && !path.starts_with(&crate_dir)
                    && !crate_dir.starts_with(&path),
                "{} is not a temporary folder",
                path.display()
            );
        }
        assert!(
            bare.join("HEAD").is_file() && !bare.join(".git").exists(),
            "{} is not a bare repository",
            bare.display()
        );
        let bare_text = bare.to_string_lossy();
        let mut args = vec!["push", "-q", bare_text.as_ref()];
        args.extend(refs);
        sh_git(dir, &args);
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
        push_to_temp_bare(&seed, &bare, &["main", "feature"]);
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
        assert_eq!(super::head(&tree).unwrap(), head);
        // The clone does not look changed.
        assert_eq!(sh_git(&clone, &["status", "--porcelain"]), "");
        // Again (an earlier worktree is replaced).
        std::fs::write(tree.path.join("junk"), "x").unwrap();
        let tree = prepare(&clone, &repo, 7, "feature", &head).unwrap();
        assert!(!tree.path.join("junk").exists());

        assert_eq!(commit(&tree, "nothing").unwrap(), None);
        std::fs::write(tree.path.join("lib.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(tree.path.join(".env"), "TOKEN=x\n").unwrap();
        let new = commit(&tree, "fix: address review").unwrap().unwrap();
        push(&tree, "feature").unwrap();
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature"]), new);
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature~1"]), head);
        let files = sh_git(&bare, &["ls-tree", "--name-only", "feature"]);
        assert_eq!(files, "lib.txt", "the .env file stays out");

        // A push over a branch that moved meanwhile is refused (no force).
        let tree = prepare(&clone, &repo, 8, "feature", &new).unwrap();
        sh_git(&tree.path, &["checkout", "-q", "--detach", &head]);
        std::fs::write(tree.path.join("lib.txt"), "other\n").unwrap();
        commit(&tree, "diverge").unwrap().unwrap();
        assert!(push(&tree, "feature").is_err());
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature"]), new);
    }

    #[test]
    fn a_rewritten_git_file_stops_the_commit_and_its_filter_never_runs() {
        // The review's attack: the model points the worktree's `.git` file
        // at a git folder of its own whose config names a clean filter.
        let (root, clone, bare, head) = remote_and_clone();
        let repo = Repo::parse("o/n").unwrap();
        let tree = prepare(&clone, &repo, 7, "feature", &head).unwrap();
        assert_eq!(tree.git_dir, tree.common_dir.join("worktrees/pr-7"));
        tree.check_git_dirs().unwrap();
        let escaped = root.path().join("escaped");
        let planted = tree.path.join("planted");
        sh_git(&tree.path, &["init", "-q", "planted"]);
        let filter = format!("echo escaped >> '{}'; cat", escaped.display());
        sh_git(&planted, &["config", "filter.planted.clean", &filter]);
        let real = std::fs::read_to_string(tree.path.join(".git")).unwrap();
        let pointer = format!("gitdir: {}\n", planted.join(".git").display());
        std::fs::write(tree.path.join(".git"), pointer).unwrap();
        std::fs::write(tree.path.join(".gitattributes"), "* filter=planted\n").unwrap();
        std::fs::write(tree.path.join(".gitignore"), "/planted/\n").unwrap();
        std::fs::write(tree.path.join("lib.txt"), "changed\n").unwrap();
        // Plain git now reads the planted config: the control.
        assert_eq!(
            sh_git(&tree.path, &["config", "filter.planted.clean"]),
            filter
        );

        let err = commit(&tree, "fix").unwrap_err().to_string();
        assert!(
            err.contains("the worktree's .git file now leads to"),
            "{err}"
        );
        assert!(!escaped.exists());
        // Git with the folder fixed reads the real one, whatever the file
        // says: the head is the pull request's, and a commit (past the
        // check) runs no filter.
        assert_eq!(super::head(&tree).unwrap(), head);
        assert!(tree.repo().commit_all("fix").unwrap().made);
        assert!(!escaped.exists());
        std::fs::write(tree.path.join(".git"), real).unwrap();
        tree.check_git_dirs().unwrap();
        push(&tree, "feature").unwrap();
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature~1"]), head);
    }

    #[test]
    fn a_changed_git_config_is_found_by_key() {
        // The review's probe: `url.<other>.pushInsteadOf <origin>` in the
        // repository's config sends this module's push elsewhere.
        let (root, clone, bare, head) = remote_and_clone();
        let repo = Repo::parse("o/n").unwrap();
        let tree = prepare(&clone, &repo, 7, "feature", &head).unwrap();
        let snapshot = ConfigSnapshot::take(&tree.path).unwrap();
        assert_eq!(snapshot.changes().unwrap(), Vec::<String>::new());
        let other = root.path().join("other.git");
        sh_git(root.path(), &["init", "-q", "--bare", "other.git"]);
        let other_text = other.to_string_lossy().to_string();
        sh_git(
            &tree.path,
            &[
                "config",
                &format!("url.{other_text}.pushInsteadOf"),
                &tree.origin,
            ],
        );
        let changes = snapshot.changes().unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert!(
            changes[0].ends_with(&format!("config: url.{other_text}.pushinsteadof")),
            "{changes:?}"
        );
        // Unchecked, the push goes to the other repository: this is what
        // the snapshot guards.
        std::fs::write(tree.path.join("lib.txt"), "changed\n").unwrap();
        commit(&tree, "fix").unwrap().unwrap();
        push(&tree, "feature").unwrap();
        assert_eq!(sh_git(&bare, &["rev-parse", "refs/heads/feature"]), head);
        assert_ne!(sh_git(&other, &["rev-parse", "refs/heads/feature"]), head);

        // A value changed, a key removed, the worktree's own file, and a
        // file git cannot read.
        let snapshot = ConfigSnapshot::take(&tree.path).unwrap();
        sh_git(&clone, &["config", "user.email", "other@example.com"]);
        sh_git(&clone, &["config", "--unset", "user.name"]);
        let changes = snapshot.changes().unwrap();
        assert!(
            changes[0].ends_with("config: user.email, user.name"),
            "{changes:?}"
        );
        assert!(!changes[0].contains("other@example.com"), "{changes:?}");
        let common = clone.join(".git");
        let snapshot = ConfigSnapshot::take(&tree.path).unwrap();
        std::fs::write(
            common.join("worktrees/pr-7/config.worktree"),
            "[core]\n\tsshCommand = sh -c id\n",
        )
        .unwrap();
        let changes = snapshot.changes().unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert!(
            changes[0].ends_with("pr-7/config.worktree: core.sshcommand"),
            "{changes:?}"
        );
        let snapshot = ConfigSnapshot::take(&tree.path).unwrap();
        let mut text = std::fs::read_to_string(common.join("config")).unwrap();
        text.push_str("[broken\n");
        std::fs::write(common.join("config"), text).unwrap();
        let changes = snapshot.changes().unwrap();
        assert!(
            changes[0].ends_with("config: the file is no longer readable as git config"),
            "{changes:?}"
        );
    }

    #[test]
    fn included_config_files_are_compared_too() {
        // An include the model cannot add (git config is refused) but can
        // change: a file the repository's config already includes.
        let (root, clone, _bare, head) = remote_and_clone();
        let repo = Repo::parse("o/n").unwrap();
        let tree = prepare(&clone, &repo, 7, "feature", &head).unwrap();
        let common = clone.join(".git");
        sh_git(&clone, &["config", "include.path", "extra.cfg"]);
        let conditional = root.path().join("conditional.cfg");
        sh_git(
            &clone,
            &[
                "config",
                "includeIf.gitdir:/elsewhere/.path",
                &conditional.to_string_lossy(),
            ],
        );
        std::fs::write(
            common.join("extra.cfg"),
            "[include]\n\tpath = nested/more.cfg\n",
        )
        .unwrap();
        std::fs::create_dir(common.join("nested")).unwrap();
        std::fs::write(common.join("nested/more.cfg"), "[user]\n\tname = x\n").unwrap();
        let snapshot = ConfigSnapshot::take(&tree.path).unwrap();
        assert_eq!(snapshot.changes().unwrap(), Vec::<String>::new());
        for (file, text) in [
            (
                common.join("extra.cfg"),
                "[url \"/tmp/other\"]\n\tpushInsteadOf = x\n",
            ),
            (
                common.join("nested/more.cfg"),
                "[core]\n\tsshCommand = sh -c id\n",
            ),
            (conditional.clone(), "[credential]\n\thelper = store\n"),
        ] {
            let before = std::fs::read(&file).ok();
            std::fs::write(&file, text).unwrap();
            let changes = snapshot.changes().unwrap();
            assert_eq!(changes.len(), 1, "{}: {changes:?}", file.display());
            assert!(
                changes[0].starts_with(&file.display().to_string()),
                "{changes:?}"
            );
            match before {
                Some(bytes) => std::fs::write(&file, bytes).unwrap(),
                None => std::fs::remove_file(&file).unwrap(),
            }
        }
        assert_eq!(snapshot.changes().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn include_paths_resolve_as_git_resolves_them() {
        let from = Path::new("/repo/.git/config");
        let home = Some(Path::new("/home/u"));
        for (value, wanted) in [
            ("extra.cfg", Some("/repo/.git/extra.cfg")),
            ("../shared/x", Some("/repo/.git/../shared/x")),
            ("/etc/x.cfg", Some("/etc/x.cfg")),
            ("~/x.cfg", Some("/home/u/x.cfg")),
            ("~other/x.cfg", None),
            ("", None),
        ] {
            assert_eq!(
                include_path(from, value, home).as_deref(),
                wanted.map(Path::new),
                "{value}"
            );
        }
        assert_eq!(include_path(from, "~/x", None), None);
    }

    #[test]
    fn the_test_command_gets_no_credentials() {
        let hidden = vec!["DB_PASSWORD".to_string()];
        let gh = Path::new("/run/gh-config");
        let command = test_command(Path::new("/"), "true", &hidden, gh);
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
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "GH_TOKEN",
            "GITHUB_TOKEN",
        ] {
            assert!(removed.iter().any(|r| r == name), "{name}: {removed:?}");
        }
        let set = |name: &str| {
            command
                .as_std()
                .get_envs()
                .find(|(key, _)| *key == name)
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().to_string())
        };
        assert_eq!(set("GH_CONFIG_DIR").as_deref(), Some("/run/gh-config"));
        assert_eq!(set("GIT_CONFIG_COUNT").as_deref(), Some("1"));
        assert_eq!(
            set("GIT_CONFIG_KEY_0").as_deref(),
            Some("credential.helper")
        );
        assert_eq!(set("GIT_CONFIG_VALUE_0").as_deref(), Some(""));
        assert_eq!(set("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn the_test_command_reaches_no_credential_helper() {
        // A clone whose own config has a helper that hands out a password,
        // as the user's helper would: the test command (code of the pull
        // request) must not get it.
        let (_root, clone, _bare, _head) = remote_and_clone();
        sh_git(
            &clone,
            &[
                "config",
                "credential.helper",
                "!f() { echo username=u; echo password=test-cmd-pw; }; f",
            ],
        );
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        let gh = crate::tools::GhConfigDir::new().unwrap();
        let tested = test(
            &clone,
            "printf 'protocol=https\\nhost=git.invalid\\n\\n' | git credential fill; \
             echo \"gh=[$GH_CONFIG_DIR] agent=[$SSH_AUTH_SOCK]\"",
            &log,
            &[],
            gh.path(),
            None,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(!tested.tail.contains("test-cmd-pw"), "{tested:?}");
        assert!(
            tested
                .tail
                .contains(&format!("gh=[{}] agent=[]", gh.path().display())),
            "{tested:?}"
        );
    }

    #[tokio::test]
    async fn the_test_command_runs_without_credentials_and_with_a_time_limit() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/t.log");
        let hidden = vec!["PATH_SECRET_X".to_string()];
        let gh = Path::new("/nonexistent/gh");
        let passed = test(
            dir.path(),
            "pwd; echo \"key=[$ANTHROPIC_API_KEY] gh=[$JUNEES_GITHUB_TOKEN] x=[$PATH_SECRET_X]\"",
            &log,
            &hidden,
            gh,
            None,
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
            gh,
            None,
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
            gh,
            None,
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert!(!slow.passed);
        assert!(slow.how.starts_with("timed out"), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Tool settings with a state folder in `home` holding a settings
    /// file and a secret, the sandbox mode unset and no temporary
    /// folders writable (the test folder is in one).
    fn sandboxed_tools(home: &Path) -> crate::tools::ToolSettings {
        let junees = home.join(".junees");
        std::fs::create_dir_all(junees.join("secrets")).unwrap();
        std::fs::write(junees.join("config.toml"), "api_key = \"fake-key\"\n").unwrap();
        std::fs::write(junees.join("secrets/token"), "fake-secret\n").unwrap();
        crate::tools::ToolSettings {
            settings_file: Some(crate::tools::SettingsFile::new(
                junees.join("config.toml"),
                Some(home.to_path_buf()),
            )),
            sandbox: Some(crate::sandbox::Settings {
                mode: None,
                write: Vec::new(),
                read: None,
                webhook_read: Vec::new(),
                network: true,
                temp: Vec::new(),
                unattended: false,
            }),
            ..crate::tools::ToolSettings::default()
        }
    }

    #[tokio::test]
    async fn the_test_command_runs_in_the_webhook_runs_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (home, tree) = (root.join("home"), root.join("tree"));
        std::fs::create_dir_all(&tree).unwrap();
        let tools = sandboxed_tools(&home);
        let gh = Path::new("/nonexistent/gh");
        let sandbox = test_sandbox(&tools, gh).unwrap();
        // Unattended: `require` when the mode is not set.
        assert_eq!(sandbox.settings().mode(), crate::sandbox::Mode::Require);
        if let crate::sandbox::Support::Unavailable(why) = sandbox.support() {
            eprintln!("skipped: {why}");
            return;
        }
        let command = format!(
            "cat {j}/config.toml || echo config-refused; \
             cat {j}/secrets/token || echo secret-refused; \
             echo x > {r}/outside || echo outside-refused; \
             echo y > made && echo tree-ok",
            j = home.join(".junees").display(),
            r = root.display(),
        );
        let log = root.join("logs/t.log");
        let tested = test(
            &tree,
            &command,
            &log,
            &[],
            gh,
            Some(&sandbox),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(tested.passed, "{tested:?}");
        for line in [
            "config-refused",
            "secret-refused",
            "outside-refused",
            "tree-ok",
        ] {
            assert!(tested.tail.contains(line), "{line}: {tested:?}");
        }
        assert!(!tested.tail.contains("fake-"), "{tested:?}");
        assert!(!root.join("outside").exists());
        assert!(tree.join("made").exists());
    }

    #[tokio::test]
    async fn a_test_command_the_sandbox_refuses_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let tree = root.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let tools = sandboxed_tools(&root.join("home"));
        let gh = Path::new("/nonexistent/gh");
        let sandbox =
            test_sandbox(&tools, gh)
                .unwrap()
                .with_support(crate::sandbox::Support::Unavailable(
                    "the kernel has no Landlock (fake)".into(),
                ));
        let error = test(
            &tree,
            "echo y > ran",
            &root.join("t.log"),
            &[],
            gh,
            Some(&sandbox),
            Duration::from_secs(30),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the test command was not run: the sandbox is required (sandbox = require) but \
             the kernel has no Landlock (fake)"
        );
        assert!(!tree.join("ran").exists());
        // `sandbox = off`: no sandbox at all.
        let mut off = tools.clone();
        off.sandbox.as_mut().unwrap().mode = Some(crate::sandbox::Mode::Off);
        assert!(test_sandbox(&off, gh).is_none());
    }
}
