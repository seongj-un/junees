//! Whose reviews count. GitHub lets any account review a public
//! repository's pull requests, and branch protection ignores reviews from
//! people without write access; the flow must ignore them too, or a
//! drive-by approval merges and a drive-by "changes requested" starts the
//! address step.
//!
//! A review counts when its author is one of the route's `--approver`
//! logins, or, without that list, when the review's `author_association`
//! is OWNER, MEMBER or COLLABORATOR and
//! `GET /repos/{repo}/collaborators/{user}/permission` says write,
//! maintain or admin now (the association is the one at review time). The
//! answer is kept for the run.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::github::Client;

/// `author_association` values of people who may have write access.
pub const WRITE_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// Permissions (`permission` or `role_name`) that may write.
const WRITE_PERMISSIONS: &[&str] = &["admin", "maintain", "write"];

/// Decides whose reviews count, for one run.
pub struct Access<'a> {
    client: &'a Client,
    repo: String,
    approvers: &'a [String],
    /// Lowercase login -> write access.
    known: RefCell<HashMap<String, bool>>,
}

impl<'a> Access<'a> {
    /// For `repo` (`owner/name`); `approvers` is the route's `--approver`
    /// list (empty: everyone with write access).
    pub fn new(client: &'a Client, repo: &str, approvers: &'a [String]) -> Self {
        Self {
            client,
            repo: repo.to_string(),
            approvers,
            known: RefCell::new(HashMap::new()),
        }
    }

    /// Whether the reviews of `login` count; `association` is the
    /// review's `author_association`. `Err` when GitHub cannot say.
    pub async fn counts(&self, login: &str, association: &str) -> Result<bool, String> {
        if login.is_empty() {
            return Ok(false);
        }
        if !self.approvers.is_empty() {
            return Ok(self
                .approvers
                .iter()
                .any(|approver| approver.eq_ignore_ascii_case(login)));
        }
        if !WRITE_ASSOCIATIONS.contains(&association.to_ascii_uppercase().as_str()) {
            return Ok(false);
        }
        let key = login.to_ascii_lowercase();
        let cached = self.known.borrow().get(&key).copied();
        if let Some(known) = cached {
            return Ok(known);
        }
        let writes = self.writes(login).await?;
        self.known.borrow_mut().insert(key, writes);
        Ok(writes)
    }

    /// Asks GitHub whether `login` may write to the repository.
    async fn writes(&self, login: &str) -> Result<bool, String> {
        // GitHub logins are letters, digits and `-`; anything else (an
        // app's `name[bot]`) is no collaborator and must not reach a path.
        if !is_login(login) {
            return Ok(false);
        }
        let path = format!("repos/{}/collaborators/{login}/permission", self.repo);
        match self.client.get(&path).await {
            Ok(answer) => Ok(["permission", "role_name"].iter().any(|key| {
                answer[key]
                    .as_str()
                    .is_some_and(|value| WRITE_PERMISSIONS.contains(&value))
            })),
            Err(err) if err.starts_with("GitHub answered 404") => Ok(false),
            Err(err) => Err(format!(
                "cannot read the permission of {login} on {}: {err}",
                self.repo
            )),
        }
    }
}

/// Whether `text` has the form of a GitHub login.
pub fn is_login(text: &str) -> bool {
    (1..=39).contains(&text.len())
        && !text.starts_with('-')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}
