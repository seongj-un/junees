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
    /// What it does, for messages to the model: "merges a pull request".
    pub does: &'static str,
    /// The same for the person's approval question, in Korean.
    pub asks: &'static str,
    /// Whether it merges, so it is asked about every time.
    pub merges: bool,
}

/// The approval kind of a write ([`super::Caution::kind`]).
pub const KIND: &str = "write to GitHub through gh";
/// The approval kind of a merge; its ending keeps an "always" answer from
/// covering the next one ([`crate::approval::ASKED_EVERY_TIME`]).
pub const MERGE_KIND: &str = "merge on GitHub through gh (asked every time)";

/// `gh <group> <subcommand>` pairs that write, with what they do.
const WRITES: &[(&str, &str, &str, &str)] = &[
    (
        "pr",
        "merge",
        "merges a pull request",
        "pull request를 merge합니다",
    ),
    (
        "pr",
        "review",
        "submits a review (approval, change request or comment)",
        "리뷰를 제출합니다(승인, 변경 요청 또는 코멘트)",
    ),
    (
        "pr",
        "close",
        "closes a pull request",
        "pull request를 닫습니다",
    ),
    ("pr", "comment", "posts a comment", "코멘트를 답니다"),
    (
        "pr",
        "create",
        "opens a pull request",
        "pull request를 엽니다",
    ),
    (
        "pr",
        "edit",
        "edits a pull request",
        "pull request를 고칩니다",
    ),
    (
        "pr",
        "reopen",
        "reopens a pull request",
        "pull request를 다시 엽니다",
    ),
    (
        "pr",
        "ready",
        "marks a pull request ready for review",
        "pull request를 리뷰 준비 상태로 바꿉니다",
    ),
    ("pr", "lock", "locks a conversation", "대화를 잠급니다"),
    (
        "pr",
        "unlock",
        "unlocks a conversation",
        "대화 잠금을 풉니다",
    ),
    ("issue", "create", "opens an issue", "이슈를 엽니다"),
    ("issue", "comment", "posts a comment", "코멘트를 답니다"),
    ("issue", "close", "closes an issue", "이슈를 닫습니다"),
    ("issue", "reopen", "reopens an issue", "이슈를 다시 엽니다"),
    ("issue", "edit", "edits an issue", "이슈를 고칩니다"),
    ("issue", "delete", "deletes an issue", "이슈를 지웁니다"),
    (
        "issue",
        "transfer",
        "moves an issue to another repository",
        "이슈를 다른 저장소로 옮깁니다",
    ),
    ("issue", "lock", "locks a conversation", "대화를 잠급니다"),
    (
        "issue",
        "unlock",
        "unlocks a conversation",
        "대화 잠금을 풉니다",
    ),
    ("issue", "pin", "pins an issue", "이슈를 고정합니다"),
    ("issue", "unpin", "unpins an issue", "이슈 고정을 풉니다"),
    (
        "release",
        "create",
        "publishes a release",
        "릴리스를 게시합니다",
    ),
    ("release", "edit", "edits a release", "릴리스를 고칩니다"),
    (
        "release",
        "delete",
        "deletes a release",
        "릴리스를 지웁니다",
    ),
    (
        "release",
        "upload",
        "uploads release assets",
        "릴리스 파일을 올립니다",
    ),
    (
        "release",
        "delete-asset",
        "deletes a release asset",
        "릴리스 파일을 지웁니다",
    ),
    (
        "repo",
        "create",
        "creates a repository",
        "저장소를 만듭니다",
    ),
    (
        "repo",
        "delete",
        "deletes a repository",
        "저장소를 지웁니다",
    ),
    (
        "repo",
        "edit",
        "changes repository settings",
        "저장소 설정을 바꿉니다",
    ),
    (
        "repo",
        "rename",
        "renames a repository",
        "저장소 이름을 바꿉니다",
    ),
    (
        "repo",
        "archive",
        "archives a repository",
        "저장소를 보관(archive)합니다",
    ),
    (
        "repo",
        "unarchive",
        "unarchives a repository",
        "저장소 보관을 풉니다",
    ),
    ("repo", "fork", "forks a repository", "저장소를 fork합니다"),
    (
        "repo",
        "sync",
        "updates a branch on GitHub",
        "GitHub의 브랜치를 갱신합니다",
    ),
    (
        "workflow",
        "run",
        "starts a workflow",
        "워크플로를 시작합니다",
    ),
    (
        "workflow",
        "enable",
        "enables a workflow",
        "워크플로를 켭니다",
    ),
    (
        "workflow",
        "disable",
        "disables a workflow",
        "워크플로를 끕니다",
    ),
    (
        "run",
        "rerun",
        "reruns a workflow run",
        "워크플로 실행을 다시 돌립니다",
    ),
    (
        "run",
        "cancel",
        "cancels a workflow run",
        "워크플로 실행을 취소합니다",
    ),
    (
        "run",
        "delete",
        "deletes a workflow run",
        "워크플로 실행 기록을 지웁니다",
    ),
    ("secret", "set", "sets a secret", "시크릿을 설정합니다"),
    ("secret", "delete", "deletes a secret", "시크릿을 지웁니다"),
    ("variable", "set", "sets a variable", "변수를 설정합니다"),
    (
        "variable",
        "delete",
        "deletes a variable",
        "변수를 지웁니다",
    ),
    ("label", "create", "creates a label", "라벨을 만듭니다"),
    ("label", "edit", "edits a label", "라벨을 고칩니다"),
    ("label", "delete", "deletes a label", "라벨을 지웁니다"),
    (
        "label",
        "clone",
        "copies labels into a repository",
        "라벨을 저장소로 복사합니다",
    ),
    ("gist", "create", "creates a gist", "gist를 만듭니다"),
    ("gist", "edit", "edits a gist", "gist를 고칩니다"),
    ("gist", "delete", "deletes a gist", "gist를 지웁니다"),
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
        format!("`{}`: 내 GitHub 계정으로 {}", self.command, self.asks)
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
    let &(_, _, does, asks) = WRITES
        .iter()
        .find(|(g, s, _, _)| *g == group && *s == subcommand)?;
    Some(GhWrite {
        command: format!("gh {group} {subcommand}"),
        does,
        asks,
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
        asks: if merges {
            "REST나 GraphQL API로 merge합니다"
        } else {
            "API에 쓰기 요청을 보냅니다"
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
            "`gh pr merge`: 내 GitHub 계정으로 pull request를 merge합니다"
        );
        assert!(MERGE_KIND.ends_with(crate::approval::ASKED_EVERY_TIME));
    }
}
