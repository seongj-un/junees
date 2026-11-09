//! Long-running task mode: a goal split into a feature list that work
//! sessions, each with a fresh context, finish one feature at a time.
//!
//! The task lives in the repository it works on, under `.junees/`:
//! `features.json` (the feature list, see [`features`]), `progress.md` (notes
//! for the next session) and `state.json` (the harness's own counters, see
//! [`state`]). Only the harness decides that a feature passes, by running the
//! feature's verify command itself.

pub mod features;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The folder of a task inside the repository at `root`.
pub fn dir(root: &Path) -> PathBuf {
    root.join(".junees")
}

pub fn features_path(root: &Path) -> PathBuf {
    dir(root).join("features.json")
}

pub fn progress_path(root: &Path) -> PathBuf {
    dir(root).join("progress.md")
}

pub fn state_path(root: &Path) -> PathBuf {
    dir(root).join("state.json")
}

/// Replaces a file through a temporary file and a rename, so a crash leaves
/// either the old or the new content, never half of it.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_replaces_the_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
