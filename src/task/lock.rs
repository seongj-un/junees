//! Keeps two `task run` from working on one repository at the same time.

use std::fs::File;
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
    /// Takes the lock of the task in `root`; `None` if another process
    /// holds it.
    ///
    /// A lock that looks taken is tried again for a moment: a child that
    /// another thread of this process forks shares every descriptor, and
    /// with it a lock this process just released, until the child execs.
    pub fn acquire(root: &Path) -> Result<Option<Self>> {
        let file = File::open(root).with_context(|| format!("cannot open {}", root.display()))?;
        let taken = crate::flock::try_exclusive(&file)
            .with_context(|| format!("cannot lock {}", root.display()))?;
        Ok(taken.then_some(Self { _dir: file }))
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
