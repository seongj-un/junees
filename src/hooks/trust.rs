//! `$JUNEES_HOME/trusted-hooks`: the project hooks files a person
//! trusted, one per line as `<sha256> <path>`. A file is trusted only with
//! the exact content (digest) it had when trusted, at the same path, so
//! any change asks again. The file sits in the state folder, which the
//! model's tools may not change ([`crate::tools::SettingsFile`]).

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{MAX_FILE_BYTES, ProjectFile};

/// The trust file's name in the state folder.
pub const FILE_NAME: &str = "trusted-hooks";
/// A stable inode for the writer lock; the ledger itself is replaced.
pub(crate) const LOCK_FILE_NAME: &str = "trusted-hooks.lock";

/// The trust file of the state folder `home`.
pub fn path(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

#[derive(PartialEq, Eq)]
pub(super) enum Status {
    New,
    Current,
    Changed,
}

/// A validated snapshot of the approval record. Only absence means that
/// there is no record to read; never use an unreadable record as empty.
#[derive(Default)]
pub(crate) struct Ledger(Vec<(String, String)>);

impl Ledger {
    pub(crate) fn read(home: Option<&Path>) -> Result<Self> {
        let Some(home) = home else {
            return Ok(Self::default());
        };
        let path = path(home);
        let name = crate::prompt_text::escape_hidden(&path.display().to_string());
        let Some(bytes) = super::read_file(&path).with_context(|| format!("cannot read {name}"))?
        else {
            return Ok(Self::default());
        };
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("cannot parse {name}: it is not UTF-8 text"))?;
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let Some((digest, path)) = line.split_once(' ').filter(|(digest, path)| {
                digest.len() == 64
                    && digest.bytes().all(|b| b.is_ascii_hexdigit())
                    && !path.is_empty()
                    && !path.contains(['\r', '\0'])
            }) else {
                bail!(
                    "cannot parse {name}: invalid approval entry at line {}",
                    index + 1
                );
            };
            entries.push((digest.to_string(), path.to_string()));
        }
        Ok(Self(entries))
    }

    /// Match the current digest and check for prior approval in one read.
    pub(super) fn status(&self, file: &ProjectFile) -> Status {
        let Some(path) = file.path.to_str() else {
            return Status::New;
        };
        let mut status = Status::New;
        for (digest, trusted) in &self.0 {
            if trusted == path {
                if file.hooks.is_ok() && *digest == file.digest {
                    return Status::Current;
                }
                status = Status::Changed;
            }
        }
        status
    }

    pub(crate) fn is_trusted(&self, file: &ProjectFile) -> bool {
        self.status(file) == Status::Current
    }

    /// Whether there is a prior entry, not permission to run this file.
    pub(super) fn was_trusted(&self, path: &Path) -> bool {
        path.to_str()
            .is_some_and(|path| self.0.iter().any(|(_, trusted)| trusted == path))
    }
}

/// Whether a person trusted `file` with its current content.
pub fn is_trusted(home: &Path, file: &ProjectFile) -> Result<bool> {
    Ok(Ledger::read(Some(home))?.is_trusted(file))
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
    update(home, shown_path(&file.path)?, Some(&file.digest)).map(|_| ())
}

/// Forgets every trust of the file at `file`; whether there was one.
pub fn untrust(home: &Path, file: &Path) -> Result<bool> {
    update(home, shown_path(file)?, None)
}

/// Apply one approval (or revocation) to the latest ledger under the
/// writer lock. Neither project validation nor a human prompt holds it.
fn update(home: &Path, entry: String, digest: Option<&str>) -> Result<bool> {
    if digest.is_some() {
        std::fs::create_dir_all(home)
            .with_context(|| format!("cannot create {}", home.display()))?;
    }
    let lock_path = home.join(LOCK_FILE_NAME);
    let name = crate::prompt_text::escape_hidden(&lock_path.display().to_string());
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&lock_path)
    {
        Ok(file) => file,
        // Revoking an approval from a missing state folder is a no-op.
        Err(err) if digest.is_none() && err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("cannot open {name}")),
    };
    let meta = lock
        .metadata()
        .with_context(|| format!("cannot inspect {name}"))?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        bail!("{name} must be a regular file with no group or other permissions");
    }
    if !crate::flock::try_exclusive(&lock).with_context(|| format!("cannot lock {name}"))? {
        bail!("{name} is locked by another junees; try again");
    }

    let Ledger(mut entries) = Ledger::read(Some(home))?;
    let before = entries.len();
    entries.retain(|(_, trusted)| *trusted != entry);
    if let Some(digest) = digest {
        entries.push((digest.to_string(), entry));
    } else if entries.len() == before {
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
    if text.is_empty() || text.contains('\0') {
        bail!("the hooks file's path is invalid, so it cannot be trusted");
    }
    Ok(text.to_string())
}

/// Writes the entries through a fresh temporary file (`O_EXCL`, no
/// links, `0600`) renamed over the trust file.
fn save(home: &Path, entries: &[(String, String)]) -> Result<()> {
    let target = path(home);
    let mut text = String::new();
    for (digest, path) in entries {
        if text.len() + digest.len() + path.len() + 2 > MAX_FILE_BYTES as usize {
            bail!(
                "cannot write {}: the approval record would exceed {} KiB",
                target.display(),
                MAX_FILE_BYTES / 1024
            );
        }
        text.push_str(digest);
        text.push(' ');
        text.push_str(path);
        text.push('\n');
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = home.join(format!(".{FILE_NAME}.{}.{nanos}.tmp", std::process::id()));

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
