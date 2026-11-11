//! Merge when ready: fixed conditions the flow code checks itself, so no
//! text in the pull request can talk it into a merge.
//!
//! A pull request is merged (method `merge`, with the head commit fixed)
//! when every condition holds ([`conditions`]):
//!
//! 1. it is open and not a draft;
//! 2. its head is still the commit the event was about;
//! 3. it has at least one check (check run or commit status) and every
//!    one passed (`success`, `neutral`, `skipped`), and every
//!    `--require-check` is there and ended `success`;
//! 4. it changes nothing under `.github/` (workflows, the actions and
//!    settings they use in the repository: such a pull request could write
//!    its own passing checks; the flow comments instead);
//! 5. no reviewer's last review (APPROVED, CHANGES_REQUESTED or DISMISSED;
//!    comments do not count) requests changes;
//! 6. the flow's own last review of the head commit, if there is one,
//!    says "머지 가능" with no 필수 item;
//! 7. at least `--min-approvals` reviewers approved the head commit, or,
//!    with `--allow-self-review`, the flow's own last review of the head
//!    commit says "머지 가능" with no 필수 item;
//! 8. GitHub says it is mergeable (`true`; `null` means not computed yet).
//!
//! Approvals of older commits do not count: the address step pushes
//! commits no person has seen. Only the reviews of people with write
//! access (or of the route's `--approver` logins) count, approvals and
//! change requests alike ([`super::access`]); with `--approver`, only the
//! listed logins' approvals count toward `--min-approvals`, while anyone
//! with write access still blocks with a change request.

use std::collections::BTreeMap;

use reqwest::Method;
use serde_json::{Value, json};

use super::FlowConfig;
use super::access::Access;
use super::prompts::Review;
use crate::github::Client;

/// Check conclusions that pass.
const PASSING: &[&str] = &["success", "neutral", "skipped"];
/// Reviews, check runs and changed files read at most.
const MAX_ITEMS: usize = 300;
/// GitHub's folder: Actions workflows, and the composite actions,
/// scripts and settings a workflow may use from the repository
/// (`uses: ./.github/actions/x`).
pub const GITHUB_DIR: &str = ".github/";
/// The name of the condition on files under [`GITHUB_DIR`].
pub const GITHUB_CONDITION: &str = ".github";

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
    /// Whether the reviewer's approvals and change requests count (write
    /// access, or an `--approver`).
    pub counts: bool,
    /// Whether the reviewer's approval counts toward `--min-approvals`
    /// (it counts, and there is no `--approver` list or the reviewer is on
    /// it).
    pub approver: bool,
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
    /// The changed files under [`GITHUB_DIR`] (new or old name).
    pub github_files: Vec<String>,
    /// More files changed than were read: changes under `.github/` unknown.
    pub more_files: bool,
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

/// The flow's own last review of the head commit: a COMMENTED review by
/// the token's user `owner` in the review form (a verdict or 필수 items;
/// replies to line comments are reviews too, without either).
fn own_review(facts: &Facts, owner: &str) -> Option<Review> {
    facts
        .reviews
        .iter()
        .rev()
        .filter(|review| {
            review.user.eq_ignore_ascii_case(owner)
                && review.state == "COMMENTED"
                && review.commit == facts.head_sha
        })
        .map(|review| Review::parse(&review.body))
        .find(|review| review.verdict.is_some() || review.must > 0)
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
    if !config.required_checks.is_empty() {
        let mut wrong = Vec::new();
        for name in &config.required_checks {
            let found: Vec<&Check> = facts
                .checks
                .iter()
                .filter(|check| check.name == *name)
                .collect();
            if found.is_empty() {
                wrong.push(format!("{name} missing"));
            } else if let Some(check) = found.iter().find(|check| check.outcome != "success") {
                wrong.push(format!("{name} {}", check.outcome));
            }
        }
        out.push(condition(
            "required checks",
            wrong.is_empty(),
            if wrong.is_empty() {
                format!("{} succeeded", config.required_checks.join(", "))
            } else {
                format!("not succeeded: {}", wrong.join(", "))
            },
        ));
    }
    out.push(condition(
        GITHUB_CONDITION,
        facts.github_files.is_empty() && !facts.more_files,
        if !facts.github_files.is_empty() {
            let mut shown: Vec<String> = facts
                .github_files
                .iter()
                .take(5)
                .map(|file| crate::prompt_text::escape_hidden(file))
                .collect();
            if facts.github_files.len() > 5 {
                shown.push(format!("{} more", facts.github_files.len() - 5));
            }
            format!("changes {}", shown.join(", "))
        } else if facts.more_files {
            format!("more than {MAX_ITEMS} files changed; not read")
        } else {
            format!("no change under {GITHUB_DIR}")
        },
    ));
    // Per reviewer the last review that sets a state, of the reviewers
    // whose reviews count.
    let mut last: BTreeMap<&str, &ReviewFact> = BTreeMap::new();
    // Reviewers whose reviews do not count at all.
    let mut ignored: Vec<&str> = Vec::new();
    for review in &facts.reviews {
        if matches!(
            review.state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        ) {
            if review.counts {
                last.insert(review.user.as_str(), review);
            } else if !ignored.contains(&review.user.as_str()) {
                ignored.push(review.user.as_str());
            }
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
        .filter(|review| {
            review.state == "APPROVED" && review.commit == facts.head_sha && review.approver
        })
        .map(|review| review.user.as_str())
        .collect();
    // Approvals that block nothing but are not an --approver's.
    let unlisted: Vec<&str> = last
        .values()
        .filter(|review| review.state == "APPROVED" && !review.approver)
        .map(|review| review.user.as_str())
        .collect();
    let enough = approvers.len() >= config.min_approvals as usize;
    let own = own_review(facts, owner);
    let own_ready = own
        .as_ref()
        .is_some_and(|review| review.ready() && review.must == 0);
    out.push(condition(
        "junees review",
        own.is_none() || own_ready,
        match &own {
            None => "none of the head commit".to_string(),
            Some(_) if own_ready => "머지 가능, no 필수 item".to_string(),
            Some(review) => format!(
                "{} 필수 item(s), verdict `{}`",
                review.must,
                crate::prompt_text::escape_hidden(review.verdict.as_deref().unwrap_or("none"))
            ),
        },
    ));
    let approved = enough || (config.allow_self_review && own_ready);
    let mut detail = format!(
        "{} of {} on the head commit",
        approvers.len(),
        config.min_approvals
    );
    if !approvers.is_empty() {
        detail.push_str(&format!(" ({})", approvers.join(", ")));
    }
    if !unlisted.is_empty() {
        detail.push_str(&format!(
            "; not counted (not an --approver): {}",
            unlisted.join(", ")
        ));
    }
    if !ignored.is_empty() {
        detail.push_str(&format!(
            "; not counted (no write access): {}",
            ignored.join(", ")
        ));
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
/// `pr`, the pull request as GitHub sent it just now; `access` decides
/// whose reviews count.
pub async fn facts(
    client: &Client,
    repo: &str,
    n: u64,
    pr: &Value,
    access: &Access<'_>,
) -> Result<Facts, String> {
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
    let mut facts = Vec::new();
    for review in &reviews.items {
        let user = text(&review["user"]["login"]);
        let state = text(&review["state"]).to_ascii_uppercase();
        let counts = match state.as_str() {
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED" => {
                access
                    .counts(
                        &user,
                        review["author_association"].as_str().unwrap_or_default(),
                    )
                    .await?
            }
            _ => false,
        };
        let approver = counts && access.approves(&user);
        facts.push(ReviewFact {
            user,
            state,
            commit: text(&review["commit_id"]),
            body: text(&review["body"]),
            counts,
            approver,
        });
    }
    let reviews = facts;
    let files = client
        .list(&format!("repos/{repo}/pulls/{n}/files"), None, MAX_ITEMS)
        .await?;
    let mut github_files: Vec<String> = Vec::new();
    for file in &files.items {
        for key in ["filename", "previous_filename"] {
            let name = text(&file[key]);
            if name.starts_with(GITHUB_DIR) && !github_files.contains(&name) {
                github_files.push(name);
            }
        }
    }
    Ok(Facts {
        open: pr["state"].as_str() == Some("open") && pr["merged"].as_bool() != Some(true),
        draft: pr["draft"].as_bool() == Some(true),
        head_sha,
        mergeable: pr["mergeable"].as_bool(),
        checks,
        reviews,
        github_files,
        more_files: files.more,
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
            counts: true,
            approver: true,
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
            github_files: Vec::new(),
            more_files: false,
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
        let detail = conditions(&facts, HEAD, &own, "junees-bot")
            .into_iter()
            .find(|condition| condition.name == "approvals")
            .unwrap()
            .detail;
        assert!(
            detail.contains("Junees's review says 머지 가능"),
            "{detail}"
        );
        // Must items or another verdict block the merge outright; an
        // older commit or someone else's review do not count.
        for (user, commit, body, wanted) in [
            (
                "junees-bot",
                HEAD,
                "판정: 머지 가능\n### 필수\n- a bug\n",
                vec!["junees review", "approvals"],
            ),
            (
                "junees-bot",
                HEAD,
                "판정: 수정 필요\n",
                vec!["junees review", "approvals"],
            ),
            ("junees-bot", OLD, "판정: 머지 가능\n", vec!["approvals"]),
            ("mallory", HEAD, "판정: 머지 가능\n", vec!["approvals"]),
        ] {
            facts.reviews = vec![review(user, "COMMENTED", commit, body)];
            assert_eq!(failing(&facts, &own), wanted, "{user} {body}");
        }
        // The last review of the head commit decides; a reply to a line
        // comment (no verdict, no items) is not a review in the form.
        facts.reviews = vec![
            review("junees-bot", "COMMENTED", HEAD, "판정: 머지 가능\n"),
            review("junees-bot", "COMMENTED", HEAD, "판정: 수정 필요\n"),
        ];
        assert_eq!(failing(&facts, &own), ["junees review", "approvals"]);
        facts.reviews = vec![
            review("junees-bot", "COMMENTED", HEAD, "판정: 머지 가능\n"),
            review("junees-bot", "COMMENTED", HEAD, "thanks, fixed"),
        ];
        assert!(failing(&facts, &own).is_empty());
    }

    /// The facts of the review's probe: a passing check, mergeable, the
    /// flow's own COMMENTED review asking for a fix, and an approval of
    /// the head commit by an account without write access.
    fn probe() -> Facts {
        let mut facts = green();
        facts.checks.truncate(1);
        let mut drive_by = review("drive-by-account", "APPROVED", HEAD, "");
        drive_by.counts = false;
        drive_by.approver = false;
        facts.reviews = vec![
            review(
                "junees-bot",
                "COMMENTED",
                HEAD,
                "판정: 수정 필요\n### 필수\n- src/a.rs:3 panics\n",
            ),
            drive_by,
        ];
        facts
    }

    #[test]
    fn only_reviews_of_people_with_write_access_count() {
        let config = super::super::config("o/n");
        let list = conditions(&probe(), HEAD, &config, "junees-bot");
        assert!(!ready(&list));
        assert_eq!(
            missing(&list),
            "junees review: 1 필수 item(s), verdict `수정 필요`; \
             approvals: 0 of 1 on the head commit; not counted (no write access): drive-by-account"
        );
        // Without the flow's review the drive-by approval still does not
        // count.
        let mut facts = probe();
        facts.reviews.remove(0);
        assert_eq!(failing(&facts, &config), ["approvals"]);
        // Nor does a drive-by change request block.
        let mut facts = green();
        let mut request = review("mallory", "CHANGES_REQUESTED", HEAD, "### 필수\n- x\n");
        request.counts = false;
        request.approver = false;
        facts.reviews.push(request);
        assert!(failing(&facts, &config).is_empty());
        // With --approver, the approval of someone with write access who
        // is not listed blocks nothing and does not count.
        let listed = FlowConfig {
            approvers: vec!["alice".into()],
            ..config.clone()
        };
        let mut facts = green();
        facts.reviews[0].approver = false;
        let detail = missing(&conditions(&facts, HEAD, &listed, "junees-bot"));
        assert_eq!(
            detail,
            "approvals: 0 of 1 on the head commit; not counted (not an --approver): alice"
        );
        // A change request of such a reviewer blocks: --approver narrows
        // only whose approvals count.
        let mut request = review("bob", "CHANGES_REQUESTED", HEAD, "");
        request.approver = false;
        facts.reviews = vec![review("alice", "APPROVED", HEAD, ""), request];
        assert_eq!(failing(&facts, &listed), ["changes requested"]);
    }

    #[test]
    fn required_checks_must_be_there_and_succeed() {
        let config = FlowConfig {
            required_checks: vec!["ci".into(), "test (ubuntu, stable)".into()],
            ..super::super::config("o/n")
        };
        let mut facts = green();
        // `test (ubuntu, stable)` is not there; `lint` (skipped) is not
        // required.
        let list = conditions(&facts, HEAD, &config, "junees-bot");
        assert_eq!(
            missing(&list),
            "required checks: not succeeded: test (ubuntu, stable) missing"
        );
        facts.checks.push(Check {
            name: "test (ubuntu, stable)".into(),
            outcome: "success".into(),
        });
        assert!(failing(&facts, &config).is_empty());
        // Neutral and skipped pass as checks but not as required ones.
        for outcome in ["neutral", "skipped"] {
            facts.checks[0].outcome = outcome.into();
            let list = conditions(&facts, HEAD, &config, "junees-bot");
            assert_eq!(
                missing(&list),
                format!("required checks: not succeeded: ci {outcome}")
            );
        }
        // A second run of the same name that failed counts.
        facts.checks[0].outcome = "success".into();
        facts.checks.push(Check {
            name: "ci".into(),
            outcome: "skipped".into(),
        });
        assert_eq!(failing(&facts, &config), ["required checks"]);
        // Without --require-check the condition is not there.
        let plain = super::super::config("o/n");
        assert!(
            !conditions(&facts, HEAD, &plain, "junees-bot")
                .iter()
                .any(|condition| condition.name == "required checks")
        );
    }

    #[test]
    fn changes_under_dot_github_are_not_merged() {
        let config = super::super::config("o/n");
        let mut facts = green();
        facts.github_files = vec![".github/workflows/ci.yml".into()];
        let list = conditions(&facts, HEAD, &config, "junees-bot");
        assert_eq!(missing(&list), ".github: changes .github/workflows/ci.yml");
        facts.github_files.clear();
        facts.more_files = true;
        assert_eq!(failing(&facts, &config), [GITHUB_CONDITION]);
        facts.more_files = false;
        assert!(failing(&facts, &config).is_empty());
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
