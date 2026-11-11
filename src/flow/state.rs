//! `$JUNEES_HOME/flows/state.json`: what the flows did, per route and pull
//! request, locked (`state.lock`) and written atomically like the routes.
//!
//! - Each action (review, address, merge) runs once per pull request and
//!   head commit: [`Locked::claim`] marks it before the run, and
//!   [`Locked::release`] takes the mark back when a run failed, so a
//!   redelivery can try again.
//! - Model runs (review, address) count toward an hourly cap per pull
//!   request; the count stays when a run fails.
//! - The commits the flow pushed, so their `synchronize` events are not
//!   reviewed again (unless `--re-review`).
//! - The recent results, for `junees webhook list`.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cron::store::{lock_file, read_private, write_private};

/// The format of state.json this build writes; newer ones are refused.
pub const FORMAT: u32 = 1;
/// Results kept per route.
pub const RECENT: usize = 20;
/// Marks kept per pull request.
const DONE_KEPT: usize = 50;
/// Pushed commits kept per pull request.
const PUSHED_KEPT: usize = 20;
/// The window of the hourly cap.
const HOUR: i64 = 3600;

/// A step of a flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Review,
    Address,
    Merge,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Address => "address",
            Self::Merge => "merge",
        }
    }

    /// Whether it is a model run (counted by the hourly cap).
    pub fn runs_model(self) -> bool {
        !matches!(self, Self::Merge)
    }
}

/// An action done (or going on) for a head commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    pub action: Action,
    pub sha: String,
    pub at: i64,
}

/// What the flows know about one pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrState {
    #[serde(default)]
    pub done: Vec<Done>,
    /// Start times of model runs within the last hour.
    #[serde(default)]
    pub runs: Vec<i64>,
    /// Commits the address step pushed.
    #[serde(default)]
    pub pushed: Vec<String>,
}

/// One handled event, for listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handled {
    pub at: i64,
    pub pr: u64,
    pub action: Action,
    pub result: String,
}

/// The state of one route's flow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteState {
    #[serde(default)]
    pub prs: BTreeMap<u64, PrState>,
    /// Newest last.
    #[serde(default)]
    pub recent: Vec<Handled>,
}

#[derive(Default, Serialize, Deserialize)]
struct File_ {
    format: u32,
    #[serde(default)]
    routes: BTreeMap<String, RouteState>,
}

/// What [`Locked::claim`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Marked: go ahead.
    Go,
    /// Done for this commit already (at that time).
    Done(i64),
    /// The hourly cap of model runs is reached.
    Limited(u32),
}

/// The state, read under the lock; [`Locked::save`] writes it back.
pub struct Locked {
    _lock: File,
    path: PathBuf,
    file: File_,
}

/// Takes the lock of `home`'s flow state (waiting up to 10 seconds).
pub fn lock(home: &Path) -> Result<Locked> {
    let dir = super::dir(home);
    let lock = lock_file(&dir, "state.lock")?;
    let path = dir.join("state.json");
    let file = match read_private(&path)? {
        None => File_::default(),
        Some(text) => {
            let file: File_ = serde_json::from_str(&text)
                .with_context(|| format!("{} is broken", path.display()))?;
            if file.format > FORMAT {
                bail!(
                    "{} was written by a newer junees (format {}); update junees",
                    path.display(),
                    file.format
                );
            }
            file
        }
    };
    Ok(Locked {
        _lock: lock,
        path,
        file,
    })
}

/// The state of route `route` as it is now (empty when unreadable).
pub fn route_state(home: &Path, route: &str) -> Result<RouteState> {
    Ok(lock(home)?.route(route).clone())
}

impl Locked {
    pub fn save(&mut self) -> Result<()> {
        self.file.format = FORMAT;
        let text = serde_json::to_string_pretty(&self.file)?;
        write_private(&self.path, &text)
    }

    /// Route `route`'s state, made when missing.
    pub fn route(&mut self, route: &str) -> &mut RouteState {
        self.file.routes.entry(route.to_string()).or_default()
    }

    /// Marks `action` on `sha` of pull request `pr` unless it was done, or
    /// (for model runs) `max_per_hour` runs started in the hour before
    /// `now`.
    pub fn claim(
        &mut self,
        route: &str,
        pr: u64,
        action: Action,
        sha: &str,
        now: i64,
        max_per_hour: u32,
    ) -> Claim {
        let state = self.route(route).prs.entry(pr).or_default();
        if let Some(done) = state
            .done
            .iter()
            .find(|done| done.action == action && done.sha == sha)
        {
            return Claim::Done(done.at);
        }
        state.runs.retain(|&at| at > now - HOUR && at <= now);
        if action.runs_model() {
            if state.runs.len() >= max_per_hour as usize {
                return Claim::Limited(max_per_hour);
            }
            state.runs.push(now);
        }
        state.done.push(Done {
            action,
            sha: sha.to_string(),
            at: now,
        });
        if state.done.len() > DONE_KEPT {
            let extra = state.done.len() - DONE_KEPT;
            state.done.drain(..extra);
        }
        Claim::Go
    }

    /// Takes back the mark of `action` on `sha` (the run failed), keeping
    /// its count in the hourly cap.
    pub fn release(&mut self, route: &str, pr: u64, action: Action, sha: &str) {
        if let Some(state) = self.route(route).prs.get_mut(&pr) {
            state
                .done
                .retain(|done| !(done.action == action && done.sha == sha));
        }
    }

    /// Notes that the flow pushed `sha` to pull request `pr`.
    pub fn pushed(&mut self, route: &str, pr: u64, sha: &str) {
        let state = self.route(route).prs.entry(pr).or_default();
        state.pushed.push(sha.to_string());
        if state.pushed.len() > PUSHED_KEPT {
            let extra = state.pushed.len() - PUSHED_KEPT;
            state.pushed.drain(..extra);
        }
    }

    /// Whether the flow pushed `sha` to pull request `pr`.
    pub fn was_pushed(&mut self, route: &str, pr: u64, sha: &str) -> bool {
        self.route(route)
            .prs
            .get(&pr)
            .is_some_and(|state| state.pushed.iter().any(|pushed| pushed == sha))
    }

    /// Adds a result to the route's recent list.
    pub fn record(&mut self, route: &str, handled: Handled) {
        let recent = &mut self.route(route).recent;
        recent.push(handled);
        if recent.len() > RECENT {
            let extra = recent.len() - RECENT;
            recent.drain(..extra);
        }
    }

    /// Forgets route `route` (removed).
    pub fn forget(&mut self, route: &str) {
        self.file.routes.remove(route);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn claims_once_per_commit_and_caps_model_runs_per_hour() {
        let home = tempfile::tempdir().unwrap();
        let mut state = lock(home.path()).unwrap();
        assert_eq!(state.claim("r", 7, Action::Review, "a", 1000, 2), Claim::Go);
        assert_eq!(
            state.claim("r", 7, Action::Review, "a", 1001, 2),
            Claim::Done(1000)
        );
        // Another action or commit is its own.
        assert_eq!(state.claim("r", 7, Action::Merge, "a", 1002, 2), Claim::Go);
        assert_eq!(state.claim("r", 8, Action::Review, "a", 1002, 2), Claim::Go);
        assert_eq!(state.claim("x", 7, Action::Review, "a", 1002, 2), Claim::Go);
        assert_eq!(
            state.claim("r", 7, Action::Address, "a", 1003, 2),
            Claim::Go
        );
        // Two model runs on #7 within the hour: the third waits.
        assert_eq!(
            state.claim("r", 7, Action::Review, "b", 1004, 2),
            Claim::Limited(2)
        );
        // Merges are not model runs.
        assert_eq!(state.claim("r", 7, Action::Merge, "b", 1004, 2), Claim::Go);
        // A failed run gives its mark back but keeps its count.
        state.release("r", 7, Action::Review, "a");
        assert_eq!(
            state.claim("r", 7, Action::Review, "a", 1005, 2),
            Claim::Limited(2)
        );
        // An hour later the count is gone.
        assert_eq!(
            state.claim("r", 7, Action::Review, "a", 1000 + HOUR, 2),
            Claim::Go
        );
        state.save().unwrap();
        drop(state);

        let mut state = lock(home.path()).unwrap();
        assert_eq!(
            state.claim("r", 7, Action::Review, "a", 9999, 2),
            Claim::Done(1000 + HOUR)
        );
        let path = home.path().join("flows/state.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn remembers_pushes_and_recent_results_within_bounds() {
        let home = tempfile::tempdir().unwrap();
        let mut state = lock(home.path()).unwrap();
        for i in 0..30 {
            state.pushed("r", 7, &format!("sha{i}"));
            state.record(
                "r",
                Handled {
                    at: i,
                    pr: 7,
                    action: Action::Review,
                    result: format!("r{i}"),
                },
            );
        }
        assert!(state.was_pushed("r", 7, "sha29"));
        assert!(!state.was_pushed("r", 7, "sha0"));
        assert!(!state.was_pushed("r", 8, "sha29"));
        state.save().unwrap();
        drop(state);
        let route = route_state(home.path(), "r").unwrap();
        assert_eq!(route.recent.len(), RECENT);
        assert_eq!(route.recent.last().unwrap().result, "r29");
        assert_eq!(route.prs[&7].pushed.len(), PUSHED_KEPT);

        let mut state = lock(home.path()).unwrap();
        state.forget("r");
        state.save().unwrap();
        drop(state);
        assert_eq!(
            route_state(home.path(), "r").unwrap(),
            RouteState::default()
        );

        std::fs::write(
            home.path().join("flows/state.json"),
            r#"{"format": 2, "routes": {}}"#,
        )
        .unwrap();
        let err = lock(home.path()).err().unwrap().to_string();
        assert!(err.contains("newer junees"), "{err}");
    }
}
