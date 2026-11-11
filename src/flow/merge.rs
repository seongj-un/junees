//! Merge when ready: fixed conditions the flow code checks itself, so no
//! text in the pull request can talk it into a merge.
//!
//! A pull request is merged (method `merge`, with the head commit fixed)
//! when every condition holds ([`conditions`]):
//!
//! 1. it is open and not a draft;
//! 2. its head is still the commit the event was about;
//! 3. it has at least one check (check run or commit status) and every
//!    one passed (`success`, `neutral`, `skipped`);
//! 4. no reviewer's last review (APPROVED, CHANGES_REQUESTED or DISMISSED;
//!    comments do not count) requests changes;
//! 5. at least `--min-approvals` reviewers approved the head commit, or,
//!    with `--allow-self-review`, the flow's own last review of the head
//!    commit says "머지 가능" with no 필수 item;
//! 6. GitHub says it is mergeable (`true`; `null` means not computed yet).
//!
//! Approvals of older commits do not count: the address step pushes
//! commits no person has seen.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::{Value, json};

use super::FlowConfig;
use super::prompts::Review;
use crate::github::Client;

/// Check conclusions that pass.
const PASSING: &[&str] = &["success", "neutral", "skipped"];
/// Reviews and check runs read at most.
const MAX_ITEMS: usize = 300;

/// A check run or commit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// `success`, `failure`, `pending`, `in_progress`, ...
    pub outcome: String,
}

/// A review as the merge step needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFact {
    pub user: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`.
    pub state: String,
    pub commit: String,
    pub body: String,
}

/// What the merge step reads about a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub open: bool,
    pub draft: bool,
    pub head_sha: String,
    pub mergeable: Option<bool>,
    pub checks: Vec<Check>,
    /// Oldest first.
    pub reviews: Vec<ReviewFact>,
}

/// One condition and whether it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn condition(name: &'static str, ok: bool, detail: impl Into<String>) -> Condition {
    Condition {
        name,
        ok,
        detail: detail.into(),
    }
}

/// The conditions for merging `facts` at commit `sha`; `owner` is the
/// token's user (the flow's own reviews).
pub fn conditions(facts: &Facts, sha: &str, config: &FlowConfig, owner: &str) -> Vec<Condition> {
    let mut out = Vec::new();
    out.push(condition(
        "open",
        facts.open && !facts.draft,
        if !facts.open {
            "closed"
        } else if facts.draft {
            "draft"
        } else {
            "open"
        },
    ));
    out.push(condition(
        "head",
        facts.head_sha == sha,
        if facts.head_sha == sha {
            format!("head is {}", short(sha))
        } else {
            format!(
                "head moved from {} to {}",
                short(sha),
                short(&facts.head_sha)
            )
        },
    ));
    let failing: Vec<&Check> = facts
        .checks
        .iter()
        .filter(|check| !PASSING.contains(&check.outcome.as_str()))
        .collect();
    out.push(condition(
        "checks",
        !facts.checks.is_empty() && failing.is_empty(),
        if facts.checks.is_empty() {
            "no checks".to_string()
        } else if failing.is_empty() {
            format!("{} passed", facts.checks.len())
        } else {
            let names: Vec<String> = failing
                .iter()
                .map(|check| format!("{} {}", check.name, check.outcome))
                .collect();
            format!("not passed: {}", names.join(", "))
        },
    ));
    // Per reviewer the last review that sets a state.
    let mut last: BTreeMap<&str, &ReviewFact> = BTreeMap::new();
    for review in &facts.reviews {
        if matches!(
            review.state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        ) {
            last.insert(review.user.as_str(), review);
        }
    }
    let requesting: Vec<&str> = last
        .values()
        .filter(|review| review.state == "CHANGES_REQUESTED")
        .map(|review| review.user.as_str())
        .collect();
    out.push(condition(
        "changes requested",
        requesting.is_empty(),
        if requesting.is_empty() {
            "none".to_string()
        } else {
            format!("by {}", requesting.join(", "))
        },
    ));
    let approvers: Vec<&str> = last
        .values()
        .filter(|review| review.state == "APPROVED" && review.commit == facts.head_sha)
        .map(|review| review.user.as_str())
        .collect();
    let enough = approvers.len() >= config.min_approvals as usize;
    let own = facts
        .reviews
        .iter()
        .rev()
        .find(|review| review.user.eq_ignore_ascii_case(owner) && review.commit == facts.head_sha)
        .map(|review| Review::parse(&review.body));
    let own_ready = own
        .as_ref()
        .is_some_and(|review| review.ready() && review.must == 0);
    let approved = enough || (config.allow_self_review && own_ready);
    let mut detail = format!(
        "{} of {} on the head commit",
        approvers.len(),
        config.min_approvals
    );
    if !approvers.is_empty() {
        detail.push_str(&format!(" ({})", approvers.join(", ")));
    }
    if !enough && config.allow_self_review {
        detail.push_str(match &own {
            Some(_) if own_ready => "; Junees's review says 머지 가능 with no 필수 item",
            Some(_) => "; Junees's review does not say 머지 가능 with no 필수 item",
            None => "; no Junees review of the head commit",
        });
    }
    out.push(condition("approvals", approved, detail));
    out.push(condition(
        "mergeable",
        facts.mergeable == Some(true),
        match facts.mergeable {
            Some(true) => "yes",
            Some(false) => "no (conflicts or blocked)",
            None => "not computed yet",
        },
    ));
    out
}

/// Whether every condition holds.
pub fn ready(conditions: &[Condition]) -> bool {
    conditions.iter().all(|condition| condition.ok)
}

/// `name: detail` of the conditions that do not hold.
pub fn missing(conditions: &[Condition]) -> String {
    let parts: Vec<String> = conditions
        .iter()
        .filter(|condition| !condition.ok)
        .map(|condition| format!("{}: {}", condition.name, condition.detail))
        .collect();
    parts.join("; ")
}

/// The conditions as a Markdown list for the PR comment.
pub fn table(conditions: &[Condition]) -> String {
    conditions
        .iter()
        .map(|condition| {
            format!(
                "- [{}] {}: {}\n",
                if condition.ok { "x" } else { " " },
                condition.name,
                condition.detail
            )
        })
        .collect()
}

/// Reads the facts of pull request `n` of `repo` (`owner/name`) from
/// `pr`, the pull request as GitHub sent it just now.
pub async fn facts(client: &Client, repo: &str, n: u64, pr: &Value) -> Result<Facts, String> {
    let head_sha = pr["head"]["sha"]
        .as_str()
        .ok_or("GitHub sent no head commit")?
        .to_string();
    let reviews = client
        .list(&format!("repos/{repo}/pulls/{n}/reviews"), None, MAX_ITEMS)
        .await?;
    let runs = client
        .list(
            &format!("repos/{repo}/commits/{head_sha}/check-runs"),
            Some("check_runs"),
            MAX_ITEMS,
        )
        .await?;
    let status = client
        .get(&format!("repos/{repo}/commits/{head_sha}/status"))
        .await?;
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    let mut checks: Vec<Check> = runs
        .items
        .iter()
        .map(|run| Check {
            name: text(&run["name"]),
            outcome: if run["status"].as_str() == Some("completed") {
                text(&run["conclusion"])
            } else {
                text(&run["status"])
            },
        })
        .collect();
    if runs.more {
        checks.push(Check {
            name: "(more check runs than read)".into(),
            outcome: "unknown".into(),
        });
    }
    for status in status["statuses"].as_array().into_iter().flatten() {
        checks.push(Check {
            name: text(&status["context"]),
            outcome: text(&status["state"]),
        });
    }
    let reviews = reviews
        .items
        .iter()
        .map(|review| ReviewFact {
            user: text(&review["user"]["login"]),
            state: text(&review["state"]).to_ascii_uppercase(),
            commit: text(&review["commit_id"]),
            body: text(&review["body"]),
        })
        .collect();
    Ok(Facts {
        open: pr["state"].as_str() == Some("open") && pr["merged"].as_bool() != Some(true),
        draft: pr["draft"].as_bool() == Some(true),
        head_sha,
        mergeable: pr["mergeable"].as_bool(),
        checks,
        reviews,
    })
}

/// Merges pull request `n` at `sha` (method merge): the merge commit, or
/// GitHub's refusal.
pub async fn merge(client: &Client, repo: &str, n: u64, sha: &str) -> Result<String, String> {
    let merged = client
        .request(
            Method::PUT,
            &format!("repos/{repo}/pulls/{n}/merge"),
            Some(&json!({"sha": sha, "merge_method": "merge"})),
        )
        .await?;
    Ok(merged["sha"].as_str().unwrap_or_default().to_string())
}

/// The first 7 characters of a commit id.
pub fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const OLD: &str = "2222222222222222222222222222222222222222";

    fn review(user: &str, state: &str, commit: &str, body: &str) -> ReviewFact {
        ReviewFact {
            user: user.into(),
            state: state.into(),
            commit: commit.into(),
            body: body.into(),
        }
    }

    fn green() -> Facts {
        Facts {
            open: true,
            draft: false,
            head_sha: HEAD.into(),
            mergeable: Some(true),
            checks: vec![
                Check {
                    name: "ci".into(),
                    outcome: "success".into(),
                },
                Check {
                    name: "lint".into(),
                    outcome: "skipped".into(),
                },
            ],
            reviews: vec![review("alice", "APPROVED", HEAD, "")],
        }
    }

    /// The names of the conditions that do not hold.
    fn failing(facts: &Facts, config: &FlowConfig) -> Vec<&'static str> {
        conditions(facts, HEAD, config, "junees-bot")
            .into_iter()
            .filter(|condition| !condition.ok)
            .map(|condition| condition.name)
            .collect()
    }

    #[test]
    fn merges_only_when_every_condition_holds() {
        let config = super::super::config("o/n");
        assert!(failing(&green(), &config).is_empty());
        type Change = Box<dyn Fn(&mut Facts)>;
        let table: Vec<(&str, Change, Vec<&str>)> = vec![
            (
                "a check failed",
                Box::new(|f| f.checks[0].outcome = "failure".into()),
                vec!["checks"],
            ),
            (
                "a check is pending",
                Box::new(|f| f.checks[1].outcome = "in_progress".into()),
                vec!["checks"],
            ),
            (
                "a status is pending",
                Box::new(|f| {
                    f.checks.push(Check {
                        name: "ext".into(),
                        outcome: "pending".into(),
                    })
                }),
                vec!["checks"],
            ),
            ("no checks", Box::new(|f| f.checks.clear()), vec!["checks"]),
            (
                "changes requested remain",
                Box::new(|f| {
                    f.reviews
                        .insert(0, review("bob", "CHANGES_REQUESTED", OLD, "fix"));
                    f.reviews.push(review("bob", "COMMENTED", HEAD, "still"));
                }),
                vec!["changes requested"],
            ),
            (
                "no approval",
                Box::new(|f| f.reviews.clear()),
                vec!["approvals"],
            ),
            (
                "approval of an older commit",
                Box::new(|f| f.reviews[0].commit = OLD.into()),
                vec!["approvals"],
            ),
            (
                "approval dismissed",
                Box::new(|f| f.reviews.push(review("alice", "DISMISSED", HEAD, ""))),
                vec!["approvals"],
            ),
            (
                "mergeable not computed",
                Box::new(|f| f.mergeable = None),
                vec!["mergeable"],
            ),
            (
                "conflicts",
                Box::new(|f| f.mergeable = Some(false)),
                vec!["mergeable"],
            ),
            ("draft", Box::new(|f| f.draft = true), vec!["open"]),
            ("closed", Box::new(|f| f.open = false), vec!["open"]),
            (
                "head moved",
                Box::new(|f| f.head_sha = OLD.into()),
                // The approval was of HEAD, now an older commit.
                vec!["head", "approvals"],
            ),
        ];
        for (what, change, wanted) in table {
            let mut facts = green();
            change(&mut facts);
            assert_eq!(failing(&facts, &config), wanted, "{what}");
        }
        // A later approval clears an earlier request of the same reviewer.
        let mut facts = green();
        facts
            .reviews
            .insert(0, review("alice", "CHANGES_REQUESTED", OLD, "fix"));
        assert!(failing(&facts, &config).is_empty());
        // Two approvals needed.
        let two = FlowConfig {
            min_approvals: 2,
            ..config.clone()
        };
        assert_eq!(failing(&green(), &two), ["approvals"]);
        let mut facts = green();
        facts.reviews.push(review("carol", "APPROVED", HEAD, ""));
        assert!(failing(&facts, &two).is_empty());
    }

    #[test]
    fn the_flows_own_review_counts_only_when_allowed() {
        let config = super::super::config("o/n");
        let own = FlowConfig {
            allow_self_review: true,
            ..config.clone()
        };
        let mut facts = green();
        facts.reviews = vec![review(
            "junees-bot",
            "COMMENTED",
            HEAD,
            "판정: 머지 가능\n### 필수\n- 없음\n### 권장\n- x\n",
        )];
        assert_eq!(failing(&facts, &config), ["approvals"]);
        assert!(failing(&facts, &own).is_empty());
        let detail = conditions(&facts, HEAD, &own, "junees-bot")[4]
            .detail
            .clone();
        assert!(
            detail.contains("Junees's review says 머지 가능"),
            "{detail}"
        );
        // Must items, another verdict, an older commit or someone else's
        // review do not count.
        for (user, commit, body) in [
            ("junees-bot", HEAD, "판정: 머지 가능\n### 필수\n- a bug\n"),
            ("junees-bot", HEAD, "판정: 수정 필요\n"),
            ("junees-bot", OLD, "판정: 머지 가능\n"),
            ("mallory", HEAD, "판정: 머지 가능\n"),
        ] {
            facts.reviews = vec![review(user, "COMMENTED", commit, body)];
            assert_eq!(failing(&facts, &own), ["approvals"], "{user} {body}");
        }
        // The last review of the head commit decides.
        facts.reviews = vec![
            review("junees-bot", "COMMENTED", HEAD, "판정: 머지 가능\n"),
            review("junees-bot", "COMMENTED", HEAD, "판정: 수정 필요\n"),
        ];
        assert_eq!(failing(&facts, &own), ["approvals"]);
    }

    #[test]
    fn the_table_marks_each_condition() {
        let mut facts = green();
        facts.mergeable = None;
        let list = conditions(&facts, HEAD, &super::super::config("o/n"), "me");
        assert!(!ready(&list));
        assert_eq!(missing(&list), "mergeable: not computed yet");
        let text = table(&list);
        assert!(
            text.starts_with("- [x] open: open\n- [x] head: head is 1111111\n"),
            "{text}"
        );
        assert!(
            text.contains("- [ ] mergeable: not computed yet\n"),
            "{text}"
        );
    }
}
