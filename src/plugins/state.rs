//! `$JUNEES_HOME/plugins/state.json`: which plugins are enabled, and which
//! of them the user trusts (their tools run without approval questions).
//! A plugin that is only installed stays off.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::manifest::check_owner;

/// The file's name in the plugins folder (not a valid plugin name, so it
/// cannot clash with one).
pub const FILE_NAME: &str = "state.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub enabled: BTreeSet<String>,
    #[serde(default)]
    pub trusted: BTreeSet<String>,
}

/// The state in the plugins folder `dir`; none yet means nothing enabled.
/// It decides which programs run, so it must be the user's and private.
pub fn load(dir: &Path) -> Result<State> {
    let path = dir.join(FILE_NAME);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    check_owner(&path, &metadata, true)?;
    if metadata.len() > MAX_FILE_BYTES {
        bail!("{} is larger than 1 MiB", path.display());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is broken", path.display()))
}

/// Replaces the state in `dir` at once (a temporary file renamed over it),
/// private to the user.
pub fn save(dir: &Path, state: &State) -> Result<()> {
    let path = dir.join(FILE_NAME);
    let temp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let mut text = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
        text.push('\n');
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    };
    write().map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        anyhow::anyhow!("cannot write {}: {err}", path.display())
    })
}
