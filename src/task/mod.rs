//! Long-running task mode: a goal split into a feature list that work
//! sessions, each with a fresh context, finish one feature at a time.
//!
//! The task lives in the repository it works on, under `.junees/`:
//! `features.json` (the feature list, see [`features`]), `progress.md` (notes
//! for the next session) and `state.json` (the harness's own counters, see
//! [`state`]). Only the harness decides that a feature passes, by running the
//! feature's verify command itself.

pub mod budget;
pub mod features;
pub mod git;
pub mod lock;
mod prompt;
pub mod runner;
pub mod state;
pub mod tools;

pub use runner::{Report, TaskEnv, init, run};

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Work sessions one `task run` starts at most, unless told otherwise.
pub const DEFAULT_MAX_SESSIONS: usize = 20;

/// A run stops after this many sessions in a row without a newly passing
/// feature.
pub const STALL_LIMIT: u32 = 3;

/// The folder of a task inside the repository at `root`.
pub fn dir(root: &Path) -> PathBuf {
    root.join(".junees")
}

pub fn features_path(root: &Path) -> PathBuf {
    dir(root).join("features.json")
}

/// Where a failed `task init` leaves the list it rejected.
pub fn rejected_features_path(root: &Path) -> PathBuf {
    dir(root).join("features.rejected.json")
}

pub fn progress_path(root: &Path) -> PathBuf {
    dir(root).join("progress.md")
}

pub fn state_path(root: &Path) -> PathBuf {
    dir(root).join("state.json")
}

/// The run log: one timestamped line per run event, for `tail -f`.
/// Git ignores it.
pub fn run_log_path(root: &Path) -> PathBuf {
    dir(root).join("run.log")
}

/// Adds a line to the run log, stamped with `unix_secs` in UTC.
pub fn append_run_log(root: &Path, unix_secs: u64, text: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_log_path(root))?;
    let line = format!(
        "{} {}\n",
        crate::session::format_utc_seconds(unix_secs),
        text.replace('\n', " ")
    );
    file.write_all(line.as_bytes())
}

/// Replaces a file through a temporary file and a rename, so a crash leaves
/// either the old or the new content, never half of it. Sessions can change
/// anything in the repository, so the folder is created again if it is
/// gone, and whatever sits at the temporary name (a symlink out of the
/// repository, say) is removed rather than written through.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    match fs::remove_file(&tmp) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    drop(file);
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

    #[test]
    fn write_atomic_recreates_the_folder_and_does_not_follow_a_planted_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone/x.json");
        write_atomic(&path, b"one").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one");

        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("gone/.x.json.tmp")).unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    }
}
