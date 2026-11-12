//! The few git operations task mode needs, run with the `git` binary.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::sandbox::{Prepared, Sandbox};
use crate::tools::{CLAUDE_CODE_SECRET_ENV_VARS, SECRET_ENV_VARS};

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
/// run what it put there: no hooks, no fsmonitor command, and no signing
/// or signature check (`gpg.program` and its ssh and x509 kin are programs
/// too). Command-line settings win over every config file.
///
/// Git has no switch for the clean and smudge filters a config file
/// names, so these settings do not cover them: [`Repo`] runs git in a
/// work tree sessions write under the sessions' own sandbox.
const SAFE_CONFIG: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "log.showSignature=false",
];

/// Runs git in `dir` and returns its stdout, or an error with its stderr.
///
/// Git can still run commands from the repository's config (clean filters
/// on `git add`, for one), so it gets neither the repository variables nor
/// any credential, like the session's own bash. In a work tree a confined
/// session writes, go through [`Repo`] instead.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = command(dir);
    command.args(args);
    let output = command.output().context("cannot run git")?;
    stdout_of(args, output)
}

/// The stdout of git run with `args`, or an error with its stderr.
fn stdout_of(args: &[&str], output: Output) -> Result<String> {
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A git command in `dir` with the settings and environment of [`git`],
/// for callers that need its pipes; the subcommand and its arguments
/// still have to be added.
pub(crate) fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.args(SAFE_CONFIG).current_dir(dir);
    hide_env(&mut command);
    command
}

/// Removes the repository variables, the credentials Junees reads and
/// Claude Code's credentials from a git command's environment.
pub(crate) fn hide_env(command: &mut Command) {
    for name in REPO_ENV_VARS
        .iter()
        .chain(SECRET_ENV_VARS)
        .chain(CLAUDE_CODE_SECRET_ENV_VARS)
    {
        command.env_remove(name);
    }
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

/// A work tree the harness runs git in, and how.
///
/// A session can write the work tree's `.git/config`, `.git/info` and
/// `.gitattributes`. A clean or smudge filter named there is a program
/// that `git add`, `git status` and `git commit` run, and no setting
/// turns filters off as a whole (a user's own, such as git-lfs, must
/// keep working). So in a work tree whose session commands run in the
/// sandbox ([`crate::sandbox`]), [`Repo::confined`] runs the harness's
/// git in that same sandbox: a filter the session planted runs with no
/// more rights than the session's own commands had, without the
/// settings file, secrets or a write outside the work tree. Where the
/// session's commands are not confined (`sandbox = off`, `auto` without
/// Landlock), neither is git, which then gives a session nothing its
/// bash did not have; where they are refused (`require` without
/// Landlock), git is refused too.
pub struct Repo {
    root: PathBuf,
    sandbox: Option<Sandbox>,
    /// The git folder fixed with `GIT_DIR` ([`Repo::pinned`]).
    git_dir: Option<PathBuf>,
    /// Why confined git may not run here: its git folder lies outside
    /// the work tree and nothing shows it belongs to it ([`GitDir`]).
    unowned: Option<String>,
}

impl Repo {
    /// Git in `root` as it is: for work trees no confined session writes.
    pub fn unconfined(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            sandbox: None,
            git_dir: None,
            unowned: None,
        }
    }

    /// Git in `root` under `sandbox`, the sandbox of the sessions' bash
    /// there ([`Sandbox::for_tools`]; `None` when their commands are not
    /// confined). When the git folder lies outside `root` and belongs to
    /// it ([`outside_git_dir`]: a linked worktree, a submodule, a
    /// `--separate-git-dir` repository), git may also write that folder
    /// (for a linked worktree, the repository's common one); the
    /// sessions' commands still may not. The git folder is then pinned
    /// like [`Repo::pinned`] to the one checked here: a confined process
    /// left running could swap the `.git` file between that check and a
    /// later command, which would then run with that write access for a
    /// git folder of its own making (settings with a filter). A git
    /// folder outside `root` that does not belong to it gets no write,
    /// and confined git refuses to run there.
    pub fn confined(root: impl Into<PathBuf>, sandbox: Option<Sandbox>) -> Self {
        let root = root.into();
        let (git_dir, write, unowned) = match sandbox.as_ref().map(|_| outside_git_dir(&root)) {
            Some(GitDir::Outside { own, write }) => (Some(own), Some(write), None),
            Some(GitDir::Unowned(why)) => (None, None, Some(why)),
            Some(GitDir::Inside) | None => (None, None, None),
        };
        let sandbox = sandbox.map(|sandbox| sandbox.with_write(write));
        Self {
            root,
            sandbox,
            git_dir,
            unowned,
        }
    }

    /// Git in the work tree `root` with its git folder fixed at `git_dir`
    /// (`GIT_DIR` and `GIT_WORK_TREE`), whatever the `.git` file in `root`
    /// says by now.
    pub fn pinned(root: impl Into<PathBuf>, git_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            sandbox: None,
            git_dir: Some(git_dir.into()),
            unowned: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A git command in the work tree, its subcommand still to be added.
    fn command(&self) -> Command {
        let mut command = command(&self.root);
        if let Some(git_dir) = &self.git_dir {
            command
                .env("GIT_DIR", git_dir)
                .env("GIT_WORK_TREE", &self.root);
        }
        command
    }

    /// Runs `command` in the sandbox, if any.
    fn output(&self, mut command: Command) -> Result<Output> {
        let confinement = match self
            .sandbox
            .as_ref()
            .map(|sandbox| sandbox.prepare(&self.root))
        {
            None | Some(Prepared::Unconfined(_)) => None,
            Some(Prepared::Confined(confinement, _)) => Some(confinement),
            Some(Prepared::Refused(reason)) => bail!("git was not run: {reason}"),
        };
        if let (Some(_), Some(why)) = (&confinement, &self.unowned) {
            bail!("git was not run: {why}");
        }
        if let Some(confinement) = &confinement {
            confinement.apply_std(&mut command);
        }
        command.output().context("cannot run git")
    }

    /// [`git`] in the work tree, confined or pinned as set up.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        let mut command = self.command();
        command.args(args);
        stdout_of(args, self.output(command)?)
    }

    /// Whether the work tree has changes or untracked files that are not
    /// ignored.
    pub fn is_dirty(&self) -> Result<bool> {
        self.dirty(&[])
    }

    /// Like [`Self::is_dirty`], but blind to the task folder `.junees/`.
    pub fn is_dirty_outside_task(&self) -> Result<bool> {
        self.dirty(&["--", ".", ":(exclude).junees"])
    }

    /// Like [`Self::is_dirty`], but blind to the untracked files in
    /// `skip`: new files the harness leaves out of its commits
    /// ([`Self::untracked_left_out`]).
    pub fn is_dirty_besides(&self, skip: &[LeftOut]) -> Result<bool> {
        let status = self.git(&[
            "status",
            "--porcelain",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ])?;
        Ok(status
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .any(|entry| match entry.strip_prefix("?? ") {
                Some(path) => !skip.iter().any(|file| file.path == path),
                None => true,
            }))
    }

    fn dirty(&self, pathspec: &[&str]) -> Result<bool> {
        let mut args = vec!["status", "--porcelain", "--untracked-files=all"];
        args.extend(pathspec);
        Ok(!self.git(&args)?.trim().is_empty())
    }
}

/// Where the git folder of a work tree under the sandbox is
/// ([`outside_git_dir`]).
enum GitDir {
    /// In the work tree (`.git` is a folder), or git found none: git runs
    /// as it is.
    Inside,
    /// Outside the work tree and belonging to it: harness git may write
    /// `write`, with `own` pinned.
    Outside { own: PathBuf, write: PathBuf },
    /// Outside the work tree, and nothing shows it belongs to it: why.
    Unowned(String),
}

/// Where the git folder of `root` is. The harness's commits write in it
/// (index, objects, the branch), which the sandbox of `root` does not
/// allow when it lies outside `root`. It belongs to `root` only when
/// `root/.git` points at it and the folder, which a session cannot write,
/// names `root` back: a linked worktree's `gitdir` file
/// (`<common>/worktrees/<name>/gitdir` is `root/.git`; the common folder
/// is written then), or `core.worktree` in its config (submodules,
/// `--separate-git-dir` after `git config core.worktree`). So a `.git`
/// file a session pointed at another repository gets no write into that
/// one.
fn outside_git_dir(root: &Path) -> GitDir {
    let Ok(out) = git(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    ) else {
        return GitDir::Inside;
    };
    let mut lines = out.lines().map(std::fs::canonicalize);
    let (Some(Ok(own)), Some(Ok(common)), Ok(root)) =
        (lines.next(), lines.next(), std::fs::canonicalize(root))
    else {
        return GitDir::Inside;
    };
    if own.starts_with(&root) {
        return GitDir::Inside;
    }
    let unowned = |why: &str| {
        GitDir::Unowned(format!(
            "its git folder {} lies outside the working folder {} and {why}, so the \
             sandboxed git may not write it",
            own.display(),
            root.display()
        ))
    };
    if dot_git_target(&root).as_ref() != Some(&own) {
        return unowned("`.git` does not point at it");
    }
    if own.parent() == Some(&common.join("worktrees")) {
        let back = std::fs::read_to_string(own.join("gitdir"))
            .ok()
            .and_then(|back| std::fs::canonicalize(own.join(back.trim_end_matches('\n'))).ok());
        return if back == Some(root.join(".git")) {
            GitDir::Outside { own, write: common }
        } else {
            unowned("its `gitdir` file does not name this work tree")
        };
    }
    if own != common {
        return unowned("it is no linked worktree's");
    }
    let config = own.join("config");
    let config = config.to_string_lossy();
    let named = git(
        &root,
        &[
            "config",
            "--file",
            &config,
            "--no-includes",
            "--get",
            "core.worktree",
        ],
    );
    match named {
        Ok(named) => {
            let named = std::fs::canonicalize(own.join(named.trim_end_matches('\n'))).ok();
            if named.as_ref() == Some(&root) {
                GitDir::Outside {
                    write: own.clone(),
                    own,
                }
            } else {
                unowned("its core.worktree names another folder")
            }
        }
        Err(_) => unowned(&format!(
            "names no work tree (core.worktree; for a `git init --separate-git-dir` \
             repository, run `git config core.worktree {}` once)",
            root.display()
        )),
    }
}

/// The folder `root/.git` stands for: the `gitdir:` of a `.git` file
/// (relative to `root`), or the `.git` folder itself.
fn dot_git_target(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if !std::fs::metadata(&dot_git).ok()?.is_file() {
        return std::fs::canonicalize(dot_git).ok();
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text
        .strip_prefix("gitdir: ")?
        .trim_end_matches(['\n', '\r']);
    std::fs::canonicalize(root.join(target)).ok()
}

/// New files larger than this (1 MiB) stay out of harness commits: build
/// output, downloads and data files a session or a verify command left
/// behind, which would bloat the history for good.
pub const MAX_NEW_FILE_BYTES: u64 = 1024 * 1024;

/// What [`commit_all`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Commit {
    /// Whether a commit was made; not when everything new was left out.
    pub made: bool,
    /// New files left out (see [`left_out_reason`]).
    pub left_out: Vec<LeftOut>,
}

/// A new file the harness does not commit, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct LeftOut {
    /// Relative to the root of the work tree.
    pub path: String,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reason {
    /// Its name says it may hold a secret (see [`looks_secret`]).
    Secret,
    /// It is larger than [`MAX_NEW_FILE_BYTES`]; the size in bytes.
    TooLarge(u64),
}

impl std::fmt::Display for LeftOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            Reason::Secret => write!(f, "{} (the name looks like a secret)", self.path),
            Reason::TooLarge(bytes) => write!(
                f,
                "{} ({:.1} MiB, over the 1 MiB limit for new files)",
                self.path,
                bytes as f64 / (1024.0 * 1024.0)
            ),
        }
    }
}

/// Why the harness would not commit the new file at `path` (relative to
/// `root`), if it would not.
pub fn left_out_reason(root: &Path, path: &str) -> Option<Reason> {
    if looks_secret(path) {
        return Some(Reason::Secret);
    }
    let size = std::fs::symlink_metadata(root.join(path)).ok()?.len();
    (size > MAX_NEW_FILE_BYTES).then_some(Reason::TooLarge(size))
}

/// Splits git's `-z` output into paths and keeps the ones the harness would
/// leave out.
fn left_out_of(root: &Path, paths: &str) -> Vec<LeftOut> {
    paths
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter_map(|path| {
            left_out_reason(root, path).map(|reason| LeftOut {
                path: path.to_string(),
                reason,
            })
        })
        .collect()
}

impl Repo {
    /// Untracked files (not ignored) the harness would leave out of its
    /// commits, limited to `pathspec` like [`Self::is_dirty`]. They keep a
    /// work tree dirty after a harness commit.
    pub fn untracked_left_out(&self, pathspec: &[&str]) -> Result<Vec<LeftOut>> {
        let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z"];
        args.extend(pathspec);
        Ok(left_out_of(&self.root, &self.git(&args)?))
    }

    /// Stages everything (`git add -A`) except new files that look like
    /// secrets or are larger than [`MAX_NEW_FILE_BYTES`], and commits it.
    pub fn commit_all(&self, message: &str) -> Result<Commit> {
        self.git(&["add", "-A"])?;
        // Rename and copy detection would hide a new file filled from a
        // tracked one (`mv .env.example .env`) from the secret check, so
        // that check sees every new path. The size check keeps detection:
        // a large file that was only moved is already in the history.
        let added = self.git(&[
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            "-z",
        ])?;
        let new_content =
            self.git(&["diff", "--cached", "--name-only", "--diff-filter=AC", "-z"])?;
        let new_content: Vec<&str> = new_content.split('\0').collect();
        let left_out: Vec<LeftOut> = left_out_of(&self.root, &added)
            .into_iter()
            .filter(|file| {
                file.reason == Reason::Secret || new_content.contains(&file.path.as_str())
            })
            .collect();
        if !left_out.is_empty() {
            let mut args = vec!["--literal-pathspecs", "rm", "--cached", "-q", "--"];
            args.extend(left_out.iter().map(|file| file.path.as_str()));
            self.git(&args)?;
        }
        let staged = self.git(&["diff", "--cached", "--name-only", "-z"])?;
        if staged.is_empty() {
            return Ok(Commit {
                made: false,
                left_out,
            });
        }
        self.git(&["commit", "-q", "--no-verify", "-m", message])?;
        Ok(Commit {
            made: true,
            left_out,
        })
    }
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

impl Repo {
    /// Whether git ignores `path` (relative to the root; a trailing `/`
    /// for a folder), through any `.gitignore`, `.git/info/exclude` or the
    /// user's global excludes, whether or not it exists.
    pub fn is_ignored(&self, path: &str) -> Result<bool> {
        let mut command = self.command();
        command
            .args(["check-ignore", "-q", "--no-index", "--", path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        match self.output(command)?.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("`git check-ignore {path}` failed"),
        }
    }

    /// Whether git tracks any file under `path`.
    pub fn tracks_any(&self, path: &str) -> Result<bool> {
        Ok(!self
            .git(&["--literal-pathspecs", "ls-files", "-z", "--", path])?
            .is_empty())
    }

    /// `git log --oneline -<count>`, or a note when there is nothing to
    /// show.
    pub fn recent_log(&self, count: usize) -> String {
        match self.git(&["log", "--oneline", &format!("-{count}")]) {
            Ok(log) if !log.trim().is_empty() => log,
            _ => "(no commits yet)\n".to_string(),
        }
    }

    /// The oldest commit before HEAD whose subject is `subject`, if any.
    /// Replacement refs (`git replace`) are ignored here and in
    /// [`Self::show`]: a session could plant one to swap the content of
    /// an old commit.
    pub fn oldest_with_subject(&self, subject: &str) -> Result<Option<String>> {
        let log = self.git(&["--no-replace-objects", "log", "--format=%H %s"])?;
        Ok(log
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, line_subject)| *line_subject == subject)
            .map(|(commit, _)| commit.to_string())
            .next_back())
    }

    /// The content of `path`, relative to the root, in `commit`.
    pub fn show(&self, commit: &str, path: &str) -> Result<String> {
        self.git(&["--no-replace-objects", "show", &format!("{commit}:{path}")])
    }
}

/// [`Repo`]'s operations on an unconfined work tree, for tests.
#[cfg(test)]
mod plain {
    use super::*;

    pub fn is_dirty(root: &Path) -> Result<bool> {
        Repo::unconfined(root).is_dirty()
    }

    pub fn is_dirty_outside_task(root: &Path) -> Result<bool> {
        Repo::unconfined(root).is_dirty_outside_task()
    }

    pub fn untracked_left_out(root: &Path, pathspec: &[&str]) -> Result<Vec<LeftOut>> {
        Repo::unconfined(root).untracked_left_out(pathspec)
    }

    pub fn commit_all(root: &Path, message: &str) -> Result<Commit> {
        Repo::unconfined(root).commit_all(message)
    }

    pub fn is_ignored(root: &Path, path: &str) -> Result<bool> {
        Repo::unconfined(root).is_ignored(path)
    }

    pub fn tracks_any(root: &Path, path: &str) -> Result<bool> {
        Repo::unconfined(root).tracks_any(path)
    }

    pub fn recent_log(root: &Path, count: usize) -> String {
        Repo::unconfined(root).recent_log(count)
    }

    pub fn oldest_with_subject(root: &Path, subject: &str) -> Result<Option<String>> {
        Repo::unconfined(root).oldest_with_subject(subject)
    }

    pub fn show(root: &Path, commit: &str, path: &str) -> Result<String> {
        Repo::unconfined(root).show(commit, path)
    }
}
#[cfg(test)]
pub use plain::*;

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

    /// Writes the shell script `body` to `path` and makes it executable.
    /// A shell writes it, so no write descriptor of this process can leak
    /// into another test's fork (which would make exec fail with ETXTBSY).
    pub fn script(path: &Path, body: &str) {
        let status = Command::new("sh")
            .args([
                "-c",
                "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"",
                "sh",
            ])
            .arg(body)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
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
        let paths: Vec<&str> = commit.left_out.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [".env", "conf/.env.local", "conf/server.pem", "id_ed25519"]
        );
        assert!(commit.left_out.iter().all(|f| f.reason == Reason::Secret));
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
    fn leaves_large_new_files_out() {
        let repo = repo();
        let root = repo.path();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/app.bin"), vec![0u8; 2_000_000]).unwrap();
        let limit = MAX_NEW_FILE_BYTES as usize;
        std::fs::write(root.join("at-limit.bin"), vec![0u8; limit]).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        let commit = commit_all(root, "first").unwrap();
        assert!(commit.made);
        assert_eq!(
            commit.left_out,
            [LeftOut {
                path: "target/app.bin".into(),
                reason: Reason::TooLarge(2_000_000)
            }]
        );
        assert_eq!(
            commit.left_out[0].to_string(),
            "target/app.bin (1.9 MiB, over the 1 MiB limit for new files)"
        );
        assert_eq!(git(root, &["ls-files"]).unwrap(), "at-limit.bin\nmain.rs\n");
        assert_eq!(untracked_left_out(root, &[]).unwrap(), commit.left_out);

        // A tracked file may grow past the limit.
        std::fs::write(root.join("main.rs"), vec![b'a'; 2_000_000]).unwrap();
        let commit = commit_all(root, "second").unwrap();
        assert!(commit.made);
        assert_eq!(commit.left_out.len(), 1);
        assert_eq!(subjects(root), ["second", "first"]);

        // A large file that was only moved is already in the history.
        std::fs::rename(root.join("main.rs"), root.join("moved.rs")).unwrap();
        let commit = commit_all(root, "third").unwrap();
        assert!(commit.made);
        assert_eq!(commit.left_out.len(), 1);
        assert_eq!(
            git(root, &["ls-files"]).unwrap(),
            "at-limit.bin\nmoved.rs\n"
        );

        // A large new copy is new content, even where the user's config
        // makes git detect copies.
        git(root, &["config", "diff.renames", "copies"]).unwrap();
        let mut text = vec![b'a'; 2_000_000];
        text.push(b'\n');
        std::fs::write(root.join("moved.rs"), &text).unwrap();
        text.push(b'b');
        std::fs::write(root.join("copy.rs"), &text).unwrap();
        let commit = commit_all(root, "fourth").unwrap();
        let paths: Vec<&str> = commit.left_out.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["copy.rs", "target/app.bin"]);
    }

    #[test]
    fn leaves_a_secret_out_that_git_sees_as_a_rename() {
        let repo = repo();
        let root = repo.path();
        let example = "DB_HOST=localhost\nDB_PORT=5432\nDB_NAME=app\nDB_USER=app\n";
        std::fs::write(root.join(".env.example"), example).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        git(root, &["add", "-A"]).unwrap();
        git(root, &["commit", "-q", "-m", "first"]).unwrap();
        // Filled from the example: git pairs the two as a rename.
        std::fs::rename(root.join(".env.example"), root.join(".env")).unwrap();
        std::fs::write(root.join(".env"), format!("{example}DB_PASSWORD=x\n")).unwrap();
        let commit = commit_all(root, "second").unwrap();
        assert!(commit.made);
        assert_eq!(
            commit.left_out,
            [LeftOut {
                path: ".env".into(),
                reason: Reason::Secret
            }]
        );
        assert_eq!(git(root, &["ls-files"]).unwrap(), "main.rs\n");
        assert!(root.join(".env").exists());
    }

    #[test]
    fn tells_ignored_and_tracked_folders() {
        let repo = repo();
        let root = repo.path();
        std::fs::write(root.join(".gitignore"), "/target\n").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build/gen.sh"), "true").unwrap();
        commit_all(root, "first").unwrap();
        assert!(is_ignored(root, "target/").unwrap());
        assert!(!is_ignored(root, "dist/").unwrap());
        assert!(tracks_any(root, "build/").unwrap());
        assert!(!tracks_any(root, "dist/").unwrap());
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

    #[test]
    fn runs_no_signing_program_a_session_configured() {
        let repo = repo();
        let root = repo.path();
        let marker = root.join(".git/signed.txt");
        let gpg = root.join(".git/fake-gpg");
        // Signs like gpg (git wants SIG_CREATED on the status fd) and
        // records every call.
        script(
            &gpg,
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncat > /dev/null\n\
                 printf '\\n[GNUPG:] SIG_CREATED D 1 8 00 0 X\\n' >&2\n\
                 printf -- '-----BEGIN PGP SIGNATURE-----\\n\\nx\\n-----END PGP SIGNATURE-----\\n'\n",
                marker.display()
            ),
        );
        let program = gpg.to_string_lossy();
        // A signed commit in the history, so `git log` has one to check.
        std::fs::write(root.join("a.txt"), "a").unwrap();
        git(root, &["add", "a.txt"]).unwrap();
        let mut signed = Command::new("git");
        signed
            .args(["-c", &format!("gpg.program={program}")])
            .args(["commit", "-q", "-S", "-m", "signed"])
            .current_dir(root);
        hide_env(&mut signed);
        let output = signed.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(marker.exists(), "the fake gpg did not sign");
        std::fs::remove_file(&marker).unwrap();

        for (key, value) in [
            ("gpg.program", &*program),
            ("gpg.ssh.program", &*program),
            ("commit.gpgSign", "true"),
            ("tag.gpgSign", "true"),
            ("log.showSignature", "true"),
        ] {
            git(root, &["config", key, value]).unwrap();
        }
        std::fs::write(root.join("b.txt"), "b").unwrap();
        assert!(commit_all(root, "unsigned").unwrap().made);
        assert!(recent_log(root, 5).contains(" signed\n"));
        assert!(
            !marker.exists(),
            "{}",
            std::fs::read_to_string(&marker).unwrap()
        );
        assert_eq!(subjects(root), ["unsigned", "signed"]);
    }

    /// Whether this kernel has Landlock; says why a test skips if not.
    fn landlock() -> bool {
        match crate::sandbox::Support::current() {
            crate::sandbox::Support::Abi(_) => true,
            crate::sandbox::Support::Unavailable(why) => {
                eprintln!("skipped: {why}");
                false
            }
        }
    }

    /// A sandbox like a session's, without the temporary folders (the
    /// test folders are in one): commands write only in the work tree.
    fn sandbox() -> Sandbox {
        let settings = crate::sandbox::Settings {
            mode: Some(crate::sandbox::Mode::Require),
            write: Vec::new(),
            read: None,
            webhook_read: Vec::new(),
            network: true,
            temp: Vec::new(),
            unattended: false,
        };
        Sandbox::new(settings, Vec::new(), Vec::new())
    }

    #[test]
    fn a_filter_a_session_planted_runs_in_the_sessions_sandbox() {
        if !landlock() {
            return;
        }
        let repo = repo();
        let root = repo.path();
        let outside = tempfile::tempdir().unwrap();
        let escaped = outside.path().join("escaped");
        let ran = root.join(".git/filter-ran");
        let filter = format!(
            "echo ran >> '{}'; (echo escaped >> '{}'); cat",
            ran.display(),
            escaped.display()
        );
        git(root, &["config", "filter.planted.clean", &filter]).unwrap();
        std::fs::write(root.join(".gitattributes"), "* filter=planted\n").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();

        let confined = Repo::confined(root, Some(sandbox()));
        assert!(confined.is_dirty().unwrap());
        assert!(confined.commit_all("planted").unwrap().made);
        assert!(ran.exists(), "the filter did not run");
        assert!(!escaped.exists(), "the filter wrote outside the work tree");
        assert!(!confined.is_dirty().unwrap());
        // The control: unconfined, the same filter gets out.
        std::fs::write(root.join("b.txt"), "b\n").unwrap();
        assert!(commit_all(root, "plain").unwrap().made);
        assert!(escaped.exists());
    }

    #[test]
    fn git_is_refused_where_the_sessions_commands_are() {
        let repo = repo();
        let root = repo.path();
        let fake = crate::sandbox::Support::Unavailable("no Landlock (fake)".into());
        let refused = Repo::confined(root, Some(sandbox().with_support(fake)));
        let err = refused.is_dirty().unwrap_err().to_string();
        assert_eq!(
            err,
            "git was not run: the sandbox is required (sandbox = require) but no Landlock (fake)"
        );
    }

    #[test]
    fn a_linked_worktree_commits_in_the_sandbox_and_a_swapped_one_does_not() {
        if !landlock() {
            return;
        }
        let repo = repo();
        let main = repo.path();
        std::fs::write(main.join("a.txt"), "a").unwrap();
        commit_all(main, "first").unwrap();
        let trees = tempfile::tempdir().unwrap();
        let linked = trees.path().join("linked");
        let shown = linked.to_string_lossy();
        git(main, &["worktree", "add", "-q", "-b", "side", &shown]).unwrap();
        std::fs::write(linked.join("b.txt"), "b").unwrap();
        let confined = Repo::confined(&linked, Some(sandbox()));
        assert!(confined.commit_all("on side").unwrap().made);
        assert_eq!(subjects(&linked), ["on side", "first"]);

        // A `.git` file pointed at another repository gets no write there.
        let other = super::testing::repo();
        std::fs::write(other.path().join("c.txt"), "c").unwrap();
        commit_all(other.path(), "other").unwrap();
        let pointer = format!("gitdir: {}\n", other.path().join(".git").display());
        std::fs::write(linked.join(".git"), pointer).unwrap();
        std::fs::write(linked.join("d.txt"), "d").unwrap();
        let swapped = Repo::confined(&linked, Some(sandbox()));
        assert!(swapped.commit_all("elsewhere").is_err());
        assert_eq!(subjects(other.path()), ["other"]);
    }

    #[test]
    fn a_linked_worktree_keeps_the_git_folder_it_was_checked_with() {
        if !landlock() {
            return;
        }
        let repo = repo();
        let main = repo.path();
        std::fs::write(main.join("a.txt"), "a").unwrap();
        commit_all(main, "first").unwrap();
        let trees = tempfile::tempdir().unwrap();
        let linked = trees.path().join("linked");
        let shown = linked.to_string_lossy();
        git(main, &["worktree", "add", "-q", "-b", "side", &shown]).unwrap();
        let confined = Repo::confined(&linked, Some(sandbox()));

        // The `.git` file changes after the check: git still uses the
        // checked folder.
        let other = super::testing::repo();
        std::fs::write(other.path().join("c.txt"), "c").unwrap();
        commit_all(other.path(), "other").unwrap();
        let pointer = format!("gitdir: {}\n", other.path().join(".git").display());
        std::fs::write(linked.join(".git"), pointer).unwrap();
        std::fs::write(linked.join("b.txt"), "b").unwrap();
        assert!(confined.commit_all("on side").unwrap().made);
        let side = git(main, &["log", "--format=%s", "side"]).unwrap();
        assert_eq!(side, "on side\nfirst\n");
        assert_eq!(subjects(other.path()), ["other"]);
    }

    #[test]
    fn a_separate_git_folder_needs_core_worktree_to_commit_in_the_sandbox() {
        if !landlock() {
            return;
        }
        let trees = tempfile::tempdir().unwrap();
        let root = trees.path().join("repo");
        let git_dir = tempfile::tempdir().unwrap();
        let git_dir = git_dir.path().join("gitdir");
        let (shown_dir, shown_root) = (git_dir.to_string_lossy(), root.to_string_lossy());
        git(
            trees.path(),
            &["init", "-q", "--separate-git-dir", &shown_dir, &shown_root],
        )
        .unwrap();
        for (key, value) in [
            ("user.name", "Junees Test"),
            ("user.email", "test@junees.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git(&root, &["config", key, value]).unwrap();
        }
        std::fs::write(root.join("a.txt"), "a").unwrap();
        commit_all(&root, "first").unwrap();

        // Nothing in the git folder names the work tree: refused, with how
        // to fix it.
        std::fs::write(root.join("b.txt"), "b").unwrap();
        let refused = Repo::confined(&root, Some(sandbox()));
        let err = refused.commit_all("refused").unwrap_err().to_string();
        assert!(err.contains("git config core.worktree"), "{err}");

        git(&root, &["config", "core.worktree", &shown_root]).unwrap();
        let confined = Repo::confined(&root, Some(sandbox()));
        assert!(confined.commit_all("second").unwrap().made);
        assert_eq!(subjects(&root), ["second", "first"]);
    }

    #[test]
    fn a_submodule_commits_in_the_sandbox() {
        if !landlock() {
            return;
        }
        let source = repo();
        std::fs::write(source.path().join("s.txt"), "s").unwrap();
        commit_all(source.path(), "source").unwrap();
        let outer = repo();
        let shown = source.path().to_string_lossy();
        git(
            outer.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &shown,
                "sub",
            ],
        )
        .unwrap();
        let sub = outer.path().join("sub");
        for (key, value) in [
            ("user.name", "Junees Test"),
            ("user.email", "test@junees.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            git(&sub, &["config", key, value]).unwrap();
        }
        assert!(std::fs::metadata(sub.join(".git")).unwrap().is_file());
        std::fs::write(sub.join("t.txt"), "t").unwrap();
        let confined = Repo::confined(&sub, Some(sandbox()));
        assert!(confined.commit_all("in sub").unwrap().made);
        assert_eq!(subjects(&sub), ["in sub", "source"]);
    }

    #[test]
    fn confined_git_and_its_filters_run_in_a_session_of_their_own() {
        if !landlock() {
            return;
        }
        let repo = repo();
        let root = repo.path();
        // Fields 5 to 7 of /proc/<pid>/stat: process group, session and
        // controlling terminal (0: none), of the filter's parent (git).
        let seen = root.join(".git/session");
        let filter = format!(
            "read -r _ _ _ _ group session tty _ < /proc/$PPID/stat; \
             echo \"$PPID $group $session $tty\" > '{}'; cat",
            seen.display()
        );
        git(root, &["config", "filter.planted.clean", &filter]).unwrap();
        std::fs::write(root.join(".gitattributes"), "* filter=planted\n").unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();

        let confined = Repo::confined(root, Some(sandbox()));
        assert!(confined.commit_all("planted").unwrap().made);
        let line = std::fs::read_to_string(&seen).unwrap();
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "{line}");
        assert_eq!(fields[1], fields[0], "{line}");
        assert_eq!(fields[2], fields[0], "{line}");
        assert_eq!(fields[3], "0", "{line}");
    }
}
