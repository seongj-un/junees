//! `gh` (GitHub CLI) commands that write to GitHub as the user, recognized
//! in bash commands.
//!
//! The `github` tool asks before every write and runs only the writes the
//! user allows where no one is asked (`JUNEES_GITHUB_ALLOW_UNATTENDED`).
//! When `gh` is logged in on the machine (or `GH_TOKEN`/`GITHUB_TOKEN` is
//! set), bash could do the same writes past both: `gh pr merge`, `gh pr
//! review --approve`, `gh api -X PUT .../merge`.
//!
//! [`super::CommandPolicy`] refuses them where no one is asked (`-p`, task
//! mode, scheduled runs, `JUNEES_APPROVAL=off`), like Junees's own
//! state-changing subcommands ([`super::self_command`]), and leaves them
//! to the approval question in the conversation and the Discord gateway,
//! which asks about them even with `risky` ([`super::CommandPolicy::cautions`]).
//! A merge is asked about every time. Reads (`gh pr view`, `gh pr diff`,
//! `gh api repos/o/n/pulls`) are not listed and run.
//!
//! Like the rest of the policy this is a deny list: `gh` aliases
//! (`gh alias set`), a program name built at run time, scripts and other
//! clients (`curl` with a token, `hub`) still get past it.

/// A write of `gh` that the policy refuses or asks about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhWrite {
    /// The command as matched: `gh pr merge`, `gh api -X PUT`.
    pub command: String,
    /// What it does, for messages: "merges a pull request".
    pub does: &'static str,
    /// Whether it merges, so it is asked about every time.
    pub merges: bool,
}

/// The approval kind of a write ([`super::Caution::kind`]).
pub const KIND: &str = "write to GitHub through gh";
/// The approval kind of a merge; its ending keeps an "always" answer from
/// covering the next one ([`crate::approval::ASKED_EVERY_TIME`]).
pub const MERGE_KIND: &str = "merge on GitHub through gh (asked every time)";

/// `gh <group> <subcommand>` pairs that write, with what they do.
const WRITES: &[(&str, &str, &str)] = &[
    ("pr", "merge", "merges a pull request"),
    (
        "pr",
        "review",
        "submits a review (approval, change request or comment)",
    ),
    ("pr", "close", "closes a pull request"),
    ("pr", "comment", "posts a comment"),
    ("pr", "create", "opens a pull request"),
    ("pr", "edit", "edits a pull request"),
    ("pr", "reopen", "reopens a pull request"),
    ("pr", "ready", "marks a pull request ready for review"),
    ("pr", "lock", "locks a conversation"),
    ("pr", "unlock", "unlocks a conversation"),
    ("issue", "create", "opens an issue"),
    ("issue", "comment", "posts a comment"),
    ("issue", "close", "closes an issue"),
    ("issue", "reopen", "reopens an issue"),
    ("issue", "edit", "edits an issue"),
    ("issue", "delete", "deletes an issue"),
    ("issue", "transfer", "moves an issue to another repository"),
    ("issue", "lock", "locks a conversation"),
    ("issue", "unlock", "unlocks a conversation"),
    ("issue", "pin", "pins an issue"),
    ("issue", "unpin", "unpins an issue"),
    ("release", "create", "publishes a release"),
    ("release", "edit", "edits a release"),
    ("release", "delete", "deletes a release"),
    ("release", "upload", "uploads release assets"),
    ("release", "delete-asset", "deletes a release asset"),
    ("repo", "create", "creates a repository"),
    ("repo", "delete", "deletes a repository"),
    ("repo", "edit", "changes repository settings"),
    ("repo", "rename", "renames a repository"),
    ("repo", "archive", "archives a repository"),
    ("repo", "unarchive", "unarchives a repository"),
    ("repo", "fork", "forks a repository"),
    ("repo", "sync", "updates a branch on GitHub"),
    ("workflow", "run", "starts a workflow"),
    ("workflow", "enable", "enables a workflow"),
    ("workflow", "disable", "disables a workflow"),
    ("run", "rerun", "reruns a workflow run"),
    ("run", "cancel", "cancels a workflow run"),
    ("run", "delete", "deletes a workflow run"),
    ("secret", "set", "sets a secret"),
    ("secret", "delete", "deletes a secret"),
    ("variable", "set", "sets a variable"),
    ("variable", "delete", "deletes a variable"),
    ("label", "create", "creates a label"),
    ("label", "edit", "edits a label"),
    ("label", "delete", "deletes a label"),
    ("label", "clone", "copies labels into a repository"),
    ("gist", "create", "creates a gist"),
    ("gist", "edit", "edits a gist"),
    ("gist", "delete", "deletes a gist"),
];

/// Options of these groups that take a value as the next word, so the
/// value is not read as the subcommand (`gh pr -R o/n merge 7`).
const VALUE_OPTIONS: &[&str] = &["-R", "--repo"];

impl GhWrite {
    /// The approval kind: merges are asked about every time.
    pub fn kind(&self) -> &'static str {
        if self.merges { MERGE_KIND } else { KIND }
    }

    /// Why the command policy refuses it where no one is asked.
    pub fn refusal(&self) -> String {
        format!(
            "`{}` {} on GitHub as the user: no one is here to approve it (the github tool's \
             writes follow the user's JUNEES_GITHUB_ALLOW_UNATTENDED)",
            self.command, self.does
        )
    }

    /// What the approval question says about it.
    pub fn caution(&self) -> String {
        format!("`{}` {} on GitHub as you", self.command, self.does)
    }
}

/// The GitHub write that `words` runs, if any. `words` is one simple
/// command with the wrappers and assignments already taken off, as the
/// command policy sees it: the program, then its arguments.
pub fn find(words: &[String]) -> Option<GhWrite> {
    let (program, args) = words.split_first()?;
    if program.rsplit('/').next() != Some("gh") {
        return None;
    }
    let at = args.iter().position(|arg| !arg.starts_with('-'))?;
    let group = args[at].as_str();
    if group == "api" {
        return api_write(&args[at + 1..]);
    }
    let mut rest = args[at + 1..].iter().map(String::as_str);
    let mut subcommand = None;
    while let Some(arg) = rest.next() {
        if VALUE_OPTIONS.contains(&arg) {
            rest.next();
        } else if !arg.starts_with('-') {
            subcommand = Some(arg);
            break;
        }
    }
    let subcommand = subcommand?;
    let &(_, _, does) = WRITES
        .iter()
        .find(|(g, s, _)| *g == group && *s == subcommand)?;
    Some(GhWrite {
        command: format!("gh {group} {subcommand}"),
        does,
        merges: group == "pr" && subcommand == "merge",
    })
}

/// `gh api` writes: an explicit POST, PUT, PATCH or DELETE method
/// (`-X`, `--method`, `-XPUT`, `--method=PUT`), or fields or an input
/// body without a method, which gh sends as a POST (`gh api graphql`
/// always does, so its queries are asked about too).
fn api_write(args: &[String]) -> Option<GhWrite> {
    let mut method = None;
    let mut body = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let arg = arg.as_str();
        if arg == "-X" || arg == "--method" {
            method = iter.next().map(|value| value.to_ascii_uppercase());
        } else if let Some(value) = arg
            .strip_prefix("--method=")
            .or_else(|| arg.strip_prefix("-X").filter(|value| !value.is_empty()))
        {
            method = Some(value.to_ascii_uppercase());
        } else if matches!(arg, "-f" | "-F" | "--field" | "--raw-field" | "--input")
            || ["--field=", "--raw-field=", "--input="]
                .iter()
                .any(|prefix| arg.starts_with(prefix))
            || ((arg.starts_with("-f") || arg.starts_with("-F")) && arg.len() > 2)
        {
            body = true;
        }
    }
    let method = match method.as_deref() {
        Some(method @ ("POST" | "PUT" | "PATCH" | "DELETE")) => method.to_string(),
        Some(_) => return None,
        None if body => "POST".to_string(),
        None => return None,
    };
    // A merge through the REST API (`.../pulls/7/merge`) or GraphQL
    // (`mergePullRequest`) is asked about every time too.
    let merges = args
        .iter()
        .any(|arg| arg.contains("/merge") || arg.contains("mergePullRequest"));
    Some(GhWrite {
        command: format!("gh api -X {method}"),
        does: if merges {
            "merges through the REST or GraphQL API"
        } else {
            "sends a write request to the API"
        },
        merges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<String> {
        command.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn finds_writes_of_gh() {
        for (command, expected, merges) in [
            ("gh pr merge 7 --merge", "gh pr merge", true),
            ("gh pr merge", "gh pr merge", true),
            ("/usr/bin/gh pr review 7 --approve", "gh pr review", false),
            ("gh pr review --comment -b x", "gh pr review", false),
            ("gh pr close 7", "gh pr close", false),
            ("gh pr comment 7 -b hi", "gh pr comment", false),
            ("gh pr create --fill", "gh pr create", false),
            ("gh pr -R o/n merge 7", "gh pr merge", true),
            ("gh pr --repo o/n review 7 --approve", "gh pr review", false),
            ("gh issue comment 3 -b x", "gh issue comment", false),
            ("gh release create v1.0", "gh release create", false),
            ("gh repo delete o/n --yes", "gh repo delete", false),
            ("gh secret set KEY", "gh secret set", false),
            ("gh workflow run ci.yml", "gh workflow run", false),
            (
                "gh api -X PUT repos/o/n/pulls/7/merge",
                "gh api -X PUT",
                true,
            ),
            (
                "gh api --method POST repos/o/n/issues/7/comments -f body=x",
                "gh api -X POST",
                false,
            ),
            (
                "gh api -XDELETE repos/o/n/git/refs/heads/x",
                "gh api -X DELETE",
                false,
            ),
            ("gh api --method=patch repos/o/n", "gh api -X PATCH", false),
            (
                "gh api repos/o/n/pulls/7/reviews -f event=APPROVE",
                "gh api -X POST",
                false,
            ),
            ("gh api repos/o/n/issues -Ftitle=x", "gh api -X POST", false),
            (
                "gh api repos/o/n/pulls --input body.json",
                "gh api -X POST",
                false,
            ),
            (
                "gh api graphql -f query=mutation{mergePullRequest}",
                "gh api -X POST",
                true,
            ),
        ] {
            let found = find(&words(command)).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(found.command, expected, "{command}");
            assert_eq!(found.merges, merges, "{command}");
            assert_eq!(
                found.kind(),
                if merges { MERGE_KIND } else { KIND },
                "{command}"
            );
        }
        for command in [
            "gh pr view 7",
            "gh pr diff 7",
            "gh pr list",
            "gh pr checks 7",
            "gh pr status",
            "gh pr",
            "gh issue list",
            "gh release list",
            "gh repo view",
            "gh api repos/o/n/pulls",
            "gh api -X GET repos/o/n/pulls -f state=open",
            "gh api --method get search/issues -f q=x",
            "gh auth status",
            "gh --version",
            "echo gh pr merge 7",
            "ghx pr merge 7",
            "git merge x",
            "",
        ] {
            assert_eq!(find(&words(command)), None, "{command}");
        }
    }

    #[test]
    fn messages_name_the_command_and_what_it_does() {
        let merge = find(&words("gh pr merge 7")).unwrap();
        assert_eq!(
            merge.refusal(),
            "`gh pr merge` merges a pull request on GitHub as the user: no one is here to \
             approve it (the github tool's writes follow the user's \
             JUNEES_GITHUB_ALLOW_UNATTENDED)"
        );
        assert_eq!(
            merge.caution(),
            "`gh pr merge` merges a pull request on GitHub as you"
        );
        assert!(MERGE_KIND.ends_with(crate::approval::ASKED_EVERY_TIME));
    }
}
