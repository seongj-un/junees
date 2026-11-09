//! `.junees/state.json`: what the harness needs to carry on after it stops
//! or crashes. Git ignores it (through `.junees/.gitignore`), because it
//! changes after every commit the harness makes.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::features::Feature;
use super::{state_path, write_atomic};
use crate::message::Usage;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Work sessions started so far, over every `task run`. Session numbers
    /// continue from here.
    #[serde(default)]
    pub sessions: u64,
    /// Work sessions in a row that ended without a newly passing feature.
    #[serde(default)]
    pub stalled: u32,
    /// Tokens used by every task session, the init session included.
    #[serde(default)]
    pub usage: Usage,
    /// Why the last `task run` stopped. `None` while a run is going, so a
    /// `None` at the start of a run means the previous one crashed.
    #[serde(default)]
    pub stop: Option<Stop>,
    /// Set while a `task run` works and cleared when it stops. Found set by
    /// a run that got the task's lock, it means the run that set it
    /// crashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<Running>,
    /// The harness's copy of the feature list as of the last save. Crash
    /// recovery restores features.json from it, since a session that
    /// crashed may have left its own edits there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<Feature>>,
}

/// Which process runs the task, and since when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Running {
    pub pid: u32,
    /// Unix seconds.
    pub started: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    pub reason: StopReason,
    /// The last session that ran (0 if none ever did).
    pub session: u64,
    /// The error, for [`StopReason::Error`]; which budget ran out, for
    /// [`StopReason::Budget`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Every feature passes.
    Complete,
    /// The run used up its `--max-sessions`.
    MaxSessions,
    /// Several sessions in a row passed no new feature.
    Stalled,
    /// A session ended with an error.
    Error,
    /// The task used up its token budget, or the run its time budget.
    Budget,
    /// SIGINT or SIGTERM: the running session was cancelled, wrapped up
    /// and checkpointed.
    Interrupted,
}

impl StopReason {
    /// Whether the task can go on as it is with another `task run`.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Stalled | Self::Error)
    }
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            StopReason::Complete => write!(f, "every feature passes"),
            StopReason::MaxSessions => write!(f, "reached --max-sessions"),
            StopReason::Stalled => write!(
                f,
                "stalled: no new feature passed in {} sessions in a row",
                super::STALL_LIMIT
            ),
            StopReason::Error => write!(
                f,
                "session {} failed: {}",
                self.session,
                self.detail.as_deref().unwrap_or("unknown error")
            ),
            StopReason::Budget => {
                write!(f, "{}", self.detail.as_deref().unwrap_or("budget used up"))
            }
            StopReason::Interrupted => write!(f, "interrupted"),
        }
    }
}

impl State {
    /// The state of the task in `root`; a fresh one if there is no file yet.
    pub fn load(root: &Path) -> Result<Self> {
        let path = state_path(root);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is corrupt", path.display())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = state_path(root);
        let mut text = serde_json::to_string_pretty(self).expect("state always serializes");
        text.push('\n');
        write_atomic(&path, text.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_is_fresh_and_saved_state_loads_back() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(super::super::dir(root.path())).unwrap();
        assert_eq!(State::load(root.path()).unwrap(), State::default());

        let state = State {
            sessions: 4,
            stalled: 1,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 7,
                ..Usage::default()
            },
            stop: Some(Stop {
                reason: StopReason::Error,
                session: 4,
                detail: Some("network down".into()),
            }),
            running: Some(Running {
                pid: 42,
                started: 1_794_205_024,
            }),
            features: Some(
                crate::task::features::parse(
                    r#"[{"id": "F1", "description": "d", "verify": "true", "passes": true}]"#,
                )
                .unwrap(),
            ),
        };
        state.save(root.path()).unwrap();
        assert_eq!(State::load(root.path()).unwrap(), state);
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(state_path(root.path())).unwrap()).unwrap();
        assert_eq!(raw["stop"]["reason"], "error");
        assert_eq!(raw["running"]["pid"], 42);
        assert_eq!(raw["features"][0]["id"], "F1");
        // An L3 state.json has neither.
        let old: State = serde_json::from_str(r#"{"sessions": 2}"#).unwrap();
        assert_eq!((old.running, old.features), (None, None));
        assert_eq!(
            state.stop.unwrap().to_string(),
            "session 4 failed: network down"
        );
    }

    #[test]
    fn corrupt_state_is_an_error() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(super::super::dir(root.path())).unwrap();
        fs::write(state_path(root.path()), "{oops").unwrap();
        let err = State::load(root.path()).unwrap_err();
        assert!(err.to_string().contains("is corrupt"), "{err}");
    }
}
