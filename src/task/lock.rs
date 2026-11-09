//! Keeps two `task run` from working on one repository at the same time.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};

/// An exclusive `flock` on the root folder of the repository a task works
/// in, held for as long as this value lives. The kernel drops it when the
/// process dies, so a lock that is held means the run holding it is still
/// alive. The descriptor is opened close-on-exec, so processes a session
/// leaves running do not keep it.
///
/// A flock belongs to the folder's inode. It is not on `.junees/`, which a
/// session may delete and the harness then creates again as a new folder
/// without the lock; the root is the folder the session works in.
pub struct RunLock {
    _dir: File,
}

impl RunLock {
    /// Takes the lock of the task in `root` without waiting; `None` if
    /// another process holds it.
    pub fn acquire(root: &Path) -> Result<Option<Self>> {
        let file = File::open(root).with_context(|| format!("cannot open {}", root.display()))?;
        // SAFETY: `file` owns the descriptor and outlives the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Self { _dir: file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        Err(err).with_context(|| format!("cannot lock {}", root.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::dir;

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
    fn replacing_the_task_folder_does_not_free_the_lock() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir(root.path())).unwrap();
        let held = RunLock::acquire(root.path()).unwrap();
        assert!(held.is_some());
        // A session deletes .junees/ and the harness creates it again.
        std::fs::remove_dir_all(dir(root.path())).unwrap();
        std::fs::create_dir_all(dir(root.path())).unwrap();
        assert!(RunLock::acquire(root.path()).unwrap().is_none());
    }
}
