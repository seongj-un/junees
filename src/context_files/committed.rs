//! The last commit's (`HEAD`) version of instruction files, read with the
//! harness's own git ([`crate::task::git::command`]: no hooks, no
//! fsmonitor, no repository variables or credentials). Webhook runs read
//! the files from there ([`super::ContextFiles::load_committed`]), and
//! every run compares the files it read with it, to say which have
//! uncommitted changes.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Stdio};

/// What `HEAD` holds at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Object {
    /// A file (a link inside the commit is followed to it).
    Blob { oid: String, size: u64 },
    /// Nothing.
    Missing,
    /// Something that is not read, and why.
    Other(String),
}

/// The repository whose top folder (the one holding `.git`) is `top`.
pub(super) struct Head<'a> {
    pub top: &'a Path,
}

impl Head<'_> {
    /// What `HEAD` holds at each of `rels` (paths from the top, `/`
    /// between folders), links inside the commit followed. A repository
    /// without commits holds nothing.
    pub fn lookup(&self, rels: &[String]) -> Result<Vec<Object>, String> {
        if rels.is_empty() {
            return Ok(Vec::new());
        }
        let mut child = self
            .spawn(&["cat-file", "--batch-check", "--follow-symlinks"], true)
            .map_err(|err| format!("cannot run git: {err}"))?;
        let mut input = String::new();
        for rel in rels {
            input.push_str(&format!("HEAD:{rel}\n"));
        }
        if let Some(mut stdin) = child.stdin.take() {
            // A failed write shows as git's failure below.
            let _ = stdin.write_all(input.as_bytes());
        }
        let output = child
            .wait_with_output()
            .map_err(|err| format!("cannot run git: {err}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let mut out = Vec::new();
        for _ in rels {
            let line = lines.next().ok_or("git gave fewer answers than asked")?;
            let object = match line.split_once(' ') {
                // These are followed by one more line, the link's target
                // or the name asked for.
                Some(("symlink", _)) => {
                    lines.next();
                    Object::Other("a link out of the repository in the last commit".into())
                }
                Some(("dangling", _)) => {
                    lines.next();
                    Object::Other("a link to nothing in the last commit".into())
                }
                Some(("loop", _)) => {
                    lines.next();
                    Object::Other("a link loop in the last commit".into())
                }
                Some(("notdir", _)) => {
                    lines.next();
                    Object::Missing
                }
                _ if line.ends_with(" missing") => Object::Missing,
                _ => {
                    let parts: Vec<&str> = line.split(' ').collect();
                    match parts[..] {
                        [oid, "blob", size] => Object::Blob {
                            oid: oid.to_string(),
                            size: size.parse().map_err(|_| format!("git said `{line}`"))?,
                        },
                        [_, kind, _] => Object::Other(format!(
                            "not a regular file (a {kind} in the last commit)"
                        )),
                        _ => return Err(format!("git said `{line}`")),
                    }
                }
            };
            out.push(object);
        }
        Ok(out)
    }

    /// The first `limit` bytes of blob `oid`.
    pub fn read(&self, oid: &str, limit: usize) -> Result<Vec<u8>, String> {
        let mut child = self.blob(oid)?;
        let mut bytes = Vec::new();
        let read = child
            .stdout
            .take()
            .map(|stdout| stdout.take(limit as u64).read_to_end(&mut bytes));
        // Whatever is left is not needed.
        let _ = child.kill();
        let _ = child.wait();
        match read {
            Some(Ok(_)) => Ok(bytes),
            Some(Err(err)) => Err(format!("cannot read the last commit: {err}")),
            None => Err("cannot read the last commit".into()),
        }
    }

    /// Whether the regular file at `path` holds exactly blob `oid` of
    /// `size` bytes. Compared byte by byte, without git's filters: a
    /// repository that converts line endings on checkout shows its files
    /// as changed.
    pub fn same(&self, oid: &str, size: u64, path: &Path) -> Result<bool, String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|err| err.to_string())?;
        let meta = file.metadata().map_err(|err| err.to_string())?;
        if !meta.is_file() || meta.len() != size {
            return Ok(false);
        }
        let mut child = self.blob(oid)?;
        let same = match child.stdout.take() {
            Some(stdout) => equal(file, stdout).map_err(|err| err.to_string()),
            None => Err("cannot read the last commit".into()),
        };
        let _ = child.kill();
        let _ = child.wait();
        same
    }

    fn blob(&self, oid: &str) -> Result<Child, String> {
        self.spawn(&["cat-file", "blob", oid], false)
            .map_err(|err| format!("cannot run git: {err}"))
    }

    fn spawn(&self, args: &[&str], stdin: bool) -> std::io::Result<Child> {
        let mut command = crate::task::git::command(self.top);
        command
            .args(args)
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The repository is the one in the top folder, never one above
        // it (when its `.git` is not a repository).
        if let Some(parent) = self.top.parent() {
            command.env("GIT_CEILING_DIRECTORIES", parent);
        }
        command.spawn()
    }
}

/// Whether `a` and `b` give the same bytes.
fn equal(mut a: impl Read, mut b: impl Read) -> std::io::Result<bool> {
    let mut left = [0u8; 8192];
    let mut right = [0u8; 8192];
    loop {
        let n = read_full(&mut a, &mut left)?;
        let m = read_full(&mut b, &mut right)?;
        if left[..n] != right[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buf` unless the input ends first; the number of bytes read.
fn read_full(input: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}
