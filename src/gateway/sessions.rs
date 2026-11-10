//! Which saved session each Discord channel continues. Sessions are the
//! ordinary journals in `$JUNEES_HOME/sessions/`; this index
//! (`$JUNEES_HOME/gateway/discord-sessions.json`, channel id to session
//! id) ties a channel to its latest one, so a restarted gateway continues
//! every channel where it was.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::engine::{Conversation, Engines};
use crate::session::{self, Session, SessionWriter};

/// The index file inside the state folder.
pub const INDEX_FILE: &str = "gateway/discord-sessions.json";

/// Channel id to session id.
pub struct SessionIndex {
    path: Option<PathBuf>,
    channels: BTreeMap<String, String>,
}

impl SessionIndex {
    /// The index of the state folder `home`; empty (and not saved) without
    /// one. A broken file is an error: overwriting it would lose every
    /// channel's session.
    pub fn load(home: Option<&Path>) -> Result<Self> {
        let Some(home) = home else {
            return Ok(Self {
                path: None,
                channels: BTreeMap::new(),
            });
        };
        let path = home.join(INDEX_FILE);
        let channels = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is broken; fix or delete it (the sessions themselves stay)",
                    path.display()
                )
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        Ok(Self {
            path: Some(path),
            channels,
        })
    }

    pub fn get(&self, channel: &str) -> Option<&str> {
        self.channels.get(channel).map(String::as_str)
    }

    /// Ties `channel` to `session` and saves the index.
    pub fn set(&mut self, channel: &str, session: &str) -> Result<()> {
        self.channels
            .insert(channel.to_string(), session.to_string());
        self.save()
    }

    /// Writes the index through a temporary file and a rename (folder
    /// 0700, file 0600, like the sessions).
    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let dir = path.parent().context("the index has no folder")?;
        let write = || -> std::io::Result<()> {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            let tmp = dir.join(".discord-sessions.json.tmp");
            let _ = fs::remove_file(&tmp);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(serde_json::to_string_pretty(&self.channels)?.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        };
        write().with_context(|| format!("cannot write {}", path.display()))
    }
}

/// Gives `conversation` the session `channel` had (resumed and locked for
/// this process), or a new one tied to the channel. Returns a line about
/// which session it is. Without a state folder nothing is saved.
pub fn open(
    index: &mut SessionIndex,
    home: Option<&Path>,
    cwd: &Path,
    engines: &dyn Engines,
    channel: &str,
    conversation: &mut dyn Conversation,
) -> String {
    let Some(home) = home else {
        conversation.reset(None);
        return "session not saved (neither JUNEES_HOME nor HOME is set)".into();
    };
    let backend = engines.session_backend();
    if let Some(id) = index.get(channel).map(str::to_string) {
        match resume(home, &id, backend) {
            Ok((session, writer)) => {
                let size = if session.meta.backend.is_some() {
                    format!("{} Claude Code turns", session.transcript.claude_turns)
                } else {
                    format!("{} messages", session.transcript.messages.len())
                };
                let mut status = format!("resumed {} ({size})", session.meta.id);
                if let Some(warning) = &session.warning {
                    status.push_str(&format!("; {warning}"));
                }
                conversation.resume(session.transcript, Some(Box::new(writer)));
                return status;
            }
            Err(err) => {
                let status = start(index, home, cwd, engines, channel, conversation);
                return format!("could not continue session {id} ({err:#}); {status}");
            }
        }
    }
    start(index, home, cwd, engines, channel, conversation)
}

/// A new session for `channel`, tied to it in the index.
pub fn start(
    index: &mut SessionIndex,
    home: &Path,
    cwd: &Path,
    engines: &dyn Engines,
    channel: &str,
    conversation: &mut dyn Conversation,
) -> String {
    match SessionWriter::create_for(home, cwd, &engines.model(), engines.session_backend()) {
        Ok((meta, writer)) => {
            conversation.reset(Some(Box::new(writer)));
            match index.set(channel, &meta.id) {
                Ok(()) => format!("session {}", meta.id),
                Err(err) => format!(
                    "session {} (the channel will not find it after a restart: {err:#})",
                    meta.id
                ),
            }
        }
        Err(err) => {
            conversation.reset(None);
            format!("session not saved: {err:#}")
        }
    }
}

/// The session `id`, if it is a conversation of `backend` and no other
/// Junees has it open.
fn resume(home: &Path, id: &str, backend: Option<&str>) -> Result<(Session, SessionWriter)> {
    let path = session::find_by_prefix(home, id)?;
    if let Some(meta) = session::read_meta(&path)
        && meta.backend.as_deref() != backend
    {
        anyhow::bail!(
            "it was run by {}, and the gateway runs on {}",
            meta.backend.as_deref().unwrap_or("the API backend"),
            backend.unwrap_or("the API backend")
        );
    }
    Session::resume(&path)
}
