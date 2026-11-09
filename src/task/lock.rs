//! Keeps two `task run` from working on one repository at the same time.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

use super::dir;

/// An exclusive `flock` on the task folder `.junees/`, held for as long as
/// this value lives. The kernel drops it when the process dies, so a lock
/// that is held means the run holding it is still alive. The descriptor is
/// opened close-on-exec, so processes a session leaves running do not keep
/// it.
pub struct RunLock {
    _dir: File,
}

impl RunLock {
    /// Takes the lock of the task in `root` without waiting; `None` if
    /// another process holds it.
    pub fn acquire(root: &Path) -> Result<Option<Self>> {
        let dir = dir(root);
        let file = File::open(&dir).with_context(|| format!("cannot open {}", dir.display()))?;
        // SAFETY: `file` owns the descriptor and outlives the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Self { _dir: file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        Err(err).with_context(|| format!("cannot lock {}", dir.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir(root.path())).unwrap();
        let first = RunLock::acquire(root.path()).unwrap();
        assert!(first.is_some());
        assert!(RunLock::acquire(root.path()).unwrap().is_none());
        drop(first);
        assert!(RunLock::acquire(root.path()).unwrap().is_some());
    }

    #[test]
    fn a_missing_task_folder_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        let err = RunLock::acquire(root.path()).err().unwrap();
        assert!(err.to_string().contains("cannot open"), "{err}");
    }
}
