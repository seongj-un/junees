//! GitHub pull request flows over webhooks (`junees webhook add-github`):
//! a webhook route with a [`FlowConfig`] runs fixed steps for the
//! events of one repository instead of a prompt template.
//!
//! - review ([`run`]): on `pull_request` opened, synchronize and
//!   ready_for_review the model reads the pull request and leaves a
//!   COMMENT review in the form of `flows/review.md` ([`prompts`]).
//! - address ([`address`]): on a `pull_request_review` that asks for
//!   changes, on a branch of the repository itself, the model fixes the
//!   code in a git worktree; the flow commits, runs the test command and
//!   pushes a new commit (never forced), then comments.
//! - merge ([`merge`]): on `check_suite`, `status` and
//!   `pull_request_review` the flow code (not the model) checks fixed
//!   conditions and merges, only with `--merge-when-ready` and the user's
//!   `github_flow_merge = true`.
//!
//! [`state`] keeps `flows/state.json`: each action once per PR and head
//! commit, an hourly cap of model runs per PR, the commits the flow pushed,
//! and the recent results for listings.

pub mod address;
pub mod merge;
pub mod prompts;
pub mod run;
pub mod setup;
pub mod state;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::github::Write;

/// The setting that lets flows merge (`github_flow_merge`).
pub const MERGE_ENV: &str = "JUNEES_GITHUB_FLOW_MERGE";

/// Model runs (review, address) per pull request and hour by default.
pub const DEFAULT_RUNS_PER_HOUR: u32 = 4;

/// The `flows` folder of the state folder `home`.
pub fn dir(home: &Path) -> PathBuf {
    home.join("flows")
}

/// What a flow route does, stored on its route (`routes.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowConfig {
    /// `owner/name`.
    pub repo: String,
    #[serde(default)]
    pub review: bool,
    #[serde(default)]
    pub address_reviews: bool,
    #[serde(default)]
    pub merge_when_ready: bool,
    /// Run in the worktree after the model's changes, before the push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_cmd: Option<String>,
    /// Approvals on the head commit a merge needs.
    #[serde(default = "one")]
    pub min_approvals: u32,
    /// Lets the flow's own review ("머지 가능", no 필수 items, on the head
    /// commit) stand in for missing approvals.
    #[serde(default)]
    pub allow_self_review: bool,
    /// Reviews again after the flow's own pushes.
    #[serde(default)]
    pub re_review: bool,
    #[serde(default = "default_runs")]
    pub max_runs_per_hour: u32,
}

fn one() -> u32 {
    1
}

fn default_runs() -> u32 {
    DEFAULT_RUNS_PER_HOUR
}

impl FlowConfig {
    /// `review, address-reviews, merge-when-ready`.
    pub fn actions(&self) -> String {
        let mut words = Vec::new();
        if self.review {
            words.push("review");
        }
        if self.address_reviews {
            words.push("address-reviews");
        }
        if self.merge_when_ready {
            words.push("merge-when-ready");
        }
        words.join(", ")
    }

    /// The webhook events the route takes.
    pub fn events(&self) -> Vec<String> {
        let mut events: Vec<String> = Vec::new();
        let mut add = |event: &str| {
            if !events.iter().any(|known| known == event) {
                events.push(event.to_string());
            }
        };
        if self.review {
            add("pull_request.opened");
            add("pull_request.synchronize");
            add("pull_request.ready_for_review");
        }
        if self.address_reviews {
            add("pull_request_review.submitted");
        }
        if self.merge_when_ready {
            add("check_suite.completed");
            add("status");
            add("pull_request_review.submitted");
        }
        events
    }

    /// The GitHub writes the model's runs need: `review` for reviews,
    /// `comment` for replies while addressing. Never `merge`: the flow
    /// code merges, not the model.
    pub fn github_allow(&self) -> Vec<Write> {
        let mut writes = Vec::new();
        if self.review {
            writes.push(Write::Review);
        }
        if self.address_reviews {
            writes.push(Write::Comment);
        }
        writes
    }

    /// Checks the values `add-github` cannot type-check.
    pub fn check(&self) -> Result<()> {
        if crate::github::Repo::parse(&self.repo).is_none() {
            bail!("--repo takes owner/name (got `{}`)", self.repo);
        }
        if !(self.review || self.address_reviews || self.merge_when_ready) {
            bail!("give at least one of --review, --address-reviews and --merge-when-ready");
        }
        if !(1..=20).contains(&self.min_approvals) {
            bail!("--min-approvals takes 1 to 20");
        }
        if !(1..=100).contains(&self.max_runs_per_hour) {
            bail!("--max-runs-per-hour takes 1 to 100");
        }
        if let Some(command) = &self.test_cmd {
            if command.trim().is_empty() {
                bail!("--test-cmd is empty; leave it out to push without a test");
            }
            if let Some(c) = command.chars().find(|&c| crate::prompt_text::is_hidden(c)) {
                bail!(
                    "--test-cmd contains U+{:04X}, a control or bidirectional formatting character",
                    c as u32
                );
            }
        }
        Ok(())
    }
}

/// Whether the user's `github_flow_merge` lets flows merge.
pub fn merge_enabled(lookup: &dyn Fn(&str) -> Option<String>) -> Result<bool> {
    match lookup(MERGE_ENV).map(|value| value.trim().to_ascii_lowercase()) {
        None => Ok(false),
        Some(value) => match value.as_str() {
            "" | "0" | "false" | "no" | "off" => Ok(false),
            "1" | "true" | "yes" | "on" => Ok(true),
            _ => bail!("{MERGE_ENV} takes true or false (got `{value}`)"),
        },
    }
}

#[cfg(test)]
pub(crate) fn config(repo: &str) -> FlowConfig {
    FlowConfig {
        repo: repo.to_string(),
        review: true,
        address_reviews: true,
        merge_when_ready: true,
        test_cmd: None,
        min_approvals: 1,
        allow_self_review: false,
        re_review: false,
        max_runs_per_hour: DEFAULT_RUNS_PER_HOUR,
    }
}
