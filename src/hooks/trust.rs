//! `$JUNEES_HOME/trusted-hooks`: the project hooks files a person
//! trusted, one per line as `<sha256> <path>`. A file is trusted only with
//! the exact content (digest) it had when trusted, at the same path, so
//! any change asks again. The file sits in the state folder, which the
//! model's tools may not change ([`crate::tools::SettingsFile`]).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::ProjectFile;

/// The trust file's name in the state folder.
pub const FILE_NAME: &str = "trusted-hooks";

/// The trust file of the state folder `home`.
pub fn path(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

/// The (digest, path) entries; unreadable files and malformed lines
/// trust nothing.
fn entries(home: &Path) -> Vec<(String, String)> {
    let Ok(Some(bytes)) = super::read_file(&path(home)) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (digest, path) = line.split_once(' ')?;
            (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| (digest.to_string(), path.to_string()))
        })
        .collect()
}

/// Whether a person trusted `file` with its current content.
pub fn is_trusted(home: &Path, file: &ProjectFile) -> bool {
    status(home, file) == Status::Current
}

#[derive(PartialEq, Eq)]
pub(super) enum Status {
    New,
    Current,
    Changed,
}

/// Match the current digest and check for prior approval in one read.
pub(super) fn status(home: &Path, file: &ProjectFile) -> Status {
    let Some(path) = file.path.to_str() else {
        return Status::New;
    };
    let mut status = Status::New;
    for (digest, trusted) in entries(home) {
        if trusted == path {
            if file.hooks.is_ok() && digest == file.digest {
                return Status::Current;
            }
            status = Status::Changed;
        }
    }
    status
}

/// Whether a trust entry still exists for this path, independently of
/// the current content. This does not authorize any hook to run.
pub(super) fn was_trusted(home: &Path, path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| entries(home).iter().any(|(_, trusted)| trusted == path))
}

/// Trusts `file` with its current content, replacing what was trusted
/// for its path before.
pub fn trust(home: &Path, file: &ProjectFile) -> Result<()> {
    if let Err(err) = &file.hooks {
        bail!(
            "{} cannot be read, so it cannot be trusted: {err}",
            file.path.display()
        );
    }
    let path = shown_path(&file.path)?;
    let mut entries = entries(home);
    entries.retain(|(_, trusted)| *trusted != path);
    entries.push((file.digest.clone(), path));
    save(home, &entries)
}

/// Forgets every trust of the file at `file`; whether there was one.
pub fn untrust(home: &Path, file: &Path) -> Result<bool> {
    let path = shown_path(file)?;
    let mut entries = entries(home);
    let before = entries.len();
    entries.retain(|(_, trusted)| *trusted != path);
    if entries.len() == before {
        return Ok(false);
    }
    save(home, &entries)?;
    Ok(true)
}

fn shown_path(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("the hooks file's path is not UTF-8, so it cannot be trusted")?;
    if text.contains(['\n', '\r']) {
        bail!("the hooks file's path holds a line break, so it cannot be trusted");
    }
    Ok(text.to_string())
}

/// Writes the entries through a fresh temporary file (`O_EXCL`, no
/// links, `0600`) renamed over the trust file.
fn save(home: &Path, entries: &[(String, String)]) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("cannot create {}", home.display()))?;
    let target = path(home);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = home.join(format!(".{FILE_NAME}.{}.{nanos}.tmp", std::process::id()));
    let mut text = String::new();
    for (digest, path) in entries {
        text.push_str(&format!("{digest} {path}\n"));
    }
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(err).with_context(|| format!("cannot write {}", target.display()));
    }
    Ok(())
}
