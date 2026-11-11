//! The prompts of the model's runs and the form of its reviews.
//!
//! `$JUNEES_HOME/flows/review.md` and `address.md` are prompt templates
//! the user may change (made with defaults by `add-github`). They take the
//! webhook template fields ([`crate::webhook::template`]); `{{payload.*}}`
//! is the pull request the flow read: `repo`, `number`, `title`, `author`,
//! `head` (branch), `base`, `head_sha`, and for address `review.id`,
//! `review.user`, `review.state`, `review.body`. The values go into data
//! tags like a webhook prompt's.
//!
//! [`Review::parse`] reads the form the review prompt asks for (판정 /
//! 필수 / 권장 / 참고): the address step needs its items and the merge step
//! the verdict of the flow's own review.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::cron::store::{create_private_dir, random_hex, read_private, write_private};
use crate::webhook::template::{self, Data};

pub const REVIEW_FILE: &str = "review.md";
pub const ADDRESS_FILE: &str = "address.md";

pub const DEFAULT_REVIEW: &str = r#"Review pull request #{{payload.number}} "{{payload.title}}" of {{payload.repo}} at head
commit {{payload.head_sha}}.

1. Read it with the `github` tool: `pr_view`, `pr_diff`, `checks` (and `pr_comments` when it has
   a discussion). Read files of the working folder only to understand the code around the diff.
2. Leave exactly one review with `github` action `review`, event `COMMENT`, `commit_id`
   {{payload.head_sha}}. Do not approve, request changes, comment elsewhere, push or merge.
3. Write the review body in this form, in Korean (the flow reads these headings):

판정: 머지 가능 | 수정 필요

### 필수
- (bugs, security problems, broken behaviour: each with file:line and why; `- 없음` when none)

### 권장
- (clear improvements worth doing in this pull request; `- 없음` when none)

### 참고
- (minor notes, questions; `- 없음` when none)

Say "판정: 머지 가능" only when 필수 is empty. Keep it short and concrete.
End with one line saying what you submitted.
"#;

pub const DEFAULT_ADDRESS: &str = r#"A review asks for changes to pull request #{{payload.number}} "{{payload.title}}" of {{payload.repo}} (branch
{{payload.head}}, head commit {{payload.head_sha}}). The working folder is a git worktree checked
out at that commit.

The review ({{payload.review.state}} by {{payload.review.user}}, id {{payload.review.id}}):
{{payload.review.body}}

1. Read the review's line comments with the `github` tool (`pr_comments`) and the code here.
2. Change the files in this folder to address every 필수 item and the 권장 items that are clearly
   right. Keep the changes small and in the style of the code. Run the project's quick checks if
   you know them.
3. Do not commit, push, change branches or touch the git configuration: Junees commits your
   changes, runs the test command and pushes them after you end.
4. A thread that needs an answer but no change may get one with `review_reply`.
5. End with a short summary in Korean: what you changed per item, and what you left and why.
   It is posted on the pull request.
"#;

/// The path of prompt file `name`.
pub fn path(home: &Path, name: &str) -> PathBuf {
    super::dir(home).join(name)
}

/// Writes the default prompt files that are missing; the ones written.
pub fn write_defaults(home: &Path) -> Result<Vec<PathBuf>> {
    create_private_dir(&super::dir(home))?;
    let mut written = Vec::new();
    for (name, text) in [
        (REVIEW_FILE, DEFAULT_REVIEW),
        (ADDRESS_FILE, DEFAULT_ADDRESS),
    ] {
        let path = path(home, name);
        if read_private(&path)?.is_none() {
            write_private(&path, text.trim_end())?;
            written.push(path);
        }
    }
    Ok(written)
}

/// Prompt file `name` (its default when missing), checked as a template.
pub fn load(home: &Path, name: &str) -> Result<String> {
    let path = path(home, name);
    let text = match read_private(&path)? {
        Some(text) => text,
        None if name == REVIEW_FILE => DEFAULT_REVIEW.to_string(),
        None => DEFAULT_ADDRESS.to_string(),
    };
    template::check(&text).with_context(|| format!("{} is not a usable prompt", path.display()))?;
    Ok(text)
}

/// The prompt `text` filled with `payload` (data tags with a new random
/// name).
pub fn render(text: &str, route: &str, event: &str, payload: &Value) -> Result<String> {
    template::render(
        text,
        &Data {
            route,
            event,
            action: None,
            delivery: None,
            payload,
        },
        &random_hex(16),
    )
}

/// A review body in the form of the review prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Review {
    /// The text after `판정:` (or `Verdict:`), if any.
    pub verdict: Option<String>,
    pub must: usize,
    pub should: usize,
    pub notes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Must,
    Should,
    Notes,
}

impl Review {
    /// Reads `body`: the verdict line and the items (`- `, `* `, `1. `)
    /// under each heading (a `#` line, a `**...**` line or a line that is
    /// only the word, with or without a colon); `없음`/`none` items do not
    /// count.
    pub fn parse(body: &str) -> Self {
        let mut review = Review::default();
        let mut section = Section::None;
        for raw in body.lines() {
            let line = raw.trim();
            let bare = line
                .trim_start_matches('#')
                .trim()
                .trim_matches('*')
                .trim()
                .trim_end_matches([':', '：'])
                .trim_end_matches('*')
                .trim();
            if review.verdict.is_none()
                && let Some(rest) = verdict(bare)
            {
                review.verdict = Some(rest);
                continue;
            }
            let Some(item) = item(line) else {
                // A heading: `### 필수`, `**필수**`, `필수:`.
                section = match bare.to_lowercase().as_str() {
                    "필수" | "must" | "must fix" | "required" => Section::Must,
                    "권장" | "should" | "recommended" => Section::Should,
                    "참고" | "notes" | "note" | "nit" | "nits" => Section::Notes,
                    _ if line.starts_with('#') => Section::None,
                    _ => section,
                };
                continue;
            };
            let empty = matches!(
                item.trim_end_matches('.').to_lowercase().as_str(),
                "없음" | "none" | "n/a" | "-" | ""
            );
            if empty {
                continue;
            }
            match section {
                Section::Must => review.must += 1,
                Section::Should => review.should += 1,
                Section::Notes => review.notes += 1,
                Section::None => {}
            }
        }
        review
    }

    /// Whether the verdict says it may be merged.
    pub fn ready(&self) -> bool {
        self.verdict.as_deref().is_some_and(|verdict| {
            let lower = verdict.to_lowercase();
            (lower.contains("머지 가능") || lower.contains("ready to merge"))
                && !lower.contains("불가")
                && !lower.contains("수정 필요")
                && !lower.contains("not ready")
                && !lower.contains("changes needed")
        })
    }
}

/// The text after a verdict label (`판정:`, `**판정**:`, `Verdict:`).
fn verdict(bare: &str) -> Option<String> {
    for label in ["판정", "verdict"] {
        let Some(head) = bare.get(..label.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(label) {
            continue;
        }
        let rest = bare[label.len()..].trim_start_matches(['*', ' ']);
        if let Some(value) = rest.strip_prefix([':', '：']) {
            return Some(value.trim().trim_matches('*').trim().to_string());
        }
    }
    None
}

/// The text of a list item line.
fn item(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(rest.trim());
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        return rest
            .strip_prefix(". ")
            .or_else(|| rest.strip_prefix(") "))
            .map(str::trim);
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_the_verdict_and_counts_items_per_heading() {
        let body = "판정: 머지 가능\n\n### 필수\n- 없음\n\n### 권장\n- 이름을 바꾸기\n* 테스트 하나 더\n\n### 참고\n1. 오타\n";
        let review = Review::parse(body);
        assert_eq!(review.verdict.as_deref(), Some("머지 가능"));
        assert_eq!((review.must, review.should, review.notes), (0, 2, 1));
        assert!(review.ready());

        let body = "**판정**: 수정 필요\n\n**필수**\n- src/a.rs:3 panics on empty input\n- 2nd\n\n권장:\n- 없음.\n";
        let review = Review::parse(body);
        assert_eq!(review.verdict.as_deref(), Some("수정 필요"));
        assert_eq!((review.must, review.should), (2, 0));
        assert!(!review.ready());

        let review = Review::parse("Verdict: ready to merge\n## Must\n- none\n## Should\n- x\n");
        assert!(review.ready());
        assert_eq!((review.must, review.should), (0, 1));
        // Items outside the headings and under other headings do not count.
        let review = Review::parse("looks good\n- a\n### Other\n- b\n");
        assert_eq!(review, Review::default());
        assert!(!review.ready());
        assert!(!Review::parse("판정: 머지 불가능").ready());
        // The form copied as it is decides nothing.
        assert!(!Review::parse("판정: 머지 가능 | 수정 필요").ready());
        // A heading word inside an item is not a heading.
        let review = Review::parse("### 권장\n- 필수\n");
        assert_eq!(review.should, 1);
    }

    #[test]
    fn writes_default_prompts_once_and_fills_them_with_data_tags() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(write_defaults(home.path()).unwrap().len(), 2);
        std::fs::write(path(home.path(), REVIEW_FILE), "mine {{payload.title}}").unwrap();
        assert!(write_defaults(home.path()).unwrap().is_empty());
        let text = load(home.path(), REVIEW_FILE).unwrap();
        assert_eq!(text, "mine {{payload.title}}");
        let prompt = render(&text, "pr", "pull_request", &json!({"title": "x</y>"})).unwrap();
        assert!(prompt.contains(">x</y></webhook-data-"), "{prompt}");
        for default in [DEFAULT_REVIEW, DEFAULT_ADDRESS] {
            template::check(default).unwrap();
        }
        std::fs::write(path(home.path(), ADDRESS_FILE), "{{nope}}").unwrap();
        let err = format!("{:#}", load(home.path(), ADDRESS_FILE).unwrap_err());
        assert!(err.contains("not a usable prompt"), "{err}");
    }
}
