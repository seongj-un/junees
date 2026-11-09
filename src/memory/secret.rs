//! Recognizes text that looks like a credential, so memory never stores
//! one: memory is plain text that every later session shows the model.

/// Prefixes of well-known credential formats, with what they are.
const PREFIXES: &[(&str, &str)] = &[
    ("sk-ant-", "an Anthropic API key"),
    ("ghp_", "a GitHub token"),
    ("gho_", "a GitHub token"),
    ("ghu_", "a GitHub token"),
    ("ghs_", "a GitHub token"),
    ("ghr_", "a GitHub token"),
    ("github_pat_", "a GitHub token"),
    ("glpat-", "a GitLab token"),
    ("xoxa-", "a Slack token"),
    ("xoxb-", "a Slack token"),
    ("xoxe-", "a Slack token"),
    ("xoxp-", "a Slack token"),
    ("xoxr-", "a Slack token"),
    ("xoxs-", "a Slack token"),
];

/// A run of hex digits at least this long reads as a token (API keys of
/// many services are 32 hex digits). Short commit ids stay allowed.
const HEX_RUN: usize = 32;

/// A run of base64/token characters at least this long with upper-case
/// letters, lower-case letters and digits reads as a token.
const MIXED_RUN: usize = 32;

/// What kind of secret `text` seems to hold, if any.
pub fn secret_kind(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return Some("a private key");
    }
    if let Some(&(_, kind)) = PREFIXES
        .iter()
        .find(|(prefix, _)| has_prefixed_token(text, prefix))
    {
        return Some(kind);
    }
    for run in runs(text) {
        if is_aws_key_id(run) {
            return Some("an AWS access key id");
        }
        if run.len() >= HEX_RUN && run.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some("a long hex token");
        }
        if run.len() >= MIXED_RUN
            && run.bytes().any(|b| b.is_ascii_uppercase())
            && run.bytes().any(|b| b.is_ascii_lowercase())
            && run.bytes().any(|b| b.is_ascii_digit())
        {
            return Some("a long random-looking token");
        }
    }
    None
}

/// The maximal runs of token characters (`A-Za-z0-9+=_-`) in `text`.
/// Every other character, `/` and `.` included, ends a run, so paths and
/// file names split into short pieces.
fn runs(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-')))
        .filter(|run| !run.is_empty())
}

/// Whether `prefix` starts a word of `text` (so `task-ant-` is no key)
/// and at least 8 token characters follow it.
fn has_prefixed_token(text: &str, prefix: &str) -> bool {
    text.match_indices(prefix).any(|(at, _)| {
        let starts_word = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let rest = text[at + prefix.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .count();
        starts_word && rest >= 8
    })
}

/// `AKIA` followed by 16 upper-case letters or digits.
fn is_aws_key_id(run: &str) -> bool {
    run.len() == 20
        && run.starts_with("AKIA")
        && run[4..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_credentials() {
        // Built at run time, so no complete fake key sits in the source.
        let filler = "a1B2c3".repeat(4);
        let cases = [
            (
                format!("key: sk-ant-api03-{filler}"),
                "an Anthropic API key",
            ),
            (format!("token=ghp_{filler}"), "a GitHub token"),
            (format!("github_pat_{filler}"), "a GitHub token"),
            (format!("slack xoxb-1234-{filler}"), "a Slack token"),
            (
                format!("AKIA{}", "ABCD1234EFGH5678"),
                "an AWS access key id",
            ),
            (
                format!("-----BEGIN OPENSSH {} KEY-----\nabc", "PRIVATE"),
                "a private key",
            ),
            ("deadbeef".repeat(4), "a long hex token"),
            (
                format!("Bearer {}", "Zm9vYmFyQmF6".repeat(3)),
                "a long random-looking token",
            ),
        ];
        for (text, kind) in cases {
            assert_eq!(secret_kind(&text), Some(kind), "{text}");
        }
    }

    #[test]
    fn ordinary_notes_pass() {
        for text in [
            "The user is 성준; answer in Korean, no emoji.",
            "Use `cargo test -q --lib memory` while working; CI runs clippy with -D warnings.",
            "Repo at /home/hermes/work/Junees2024/src/memory/mod.rs, branch feat/memory.",
            "Last good commit cf302d7, see https://github.com/seongj-un/junees/pull/29.",
            "Run with CARGO_BUILD_JOBS=1 on the 2 vCPU server.",
            "task-ant-colony and desk-top are just words",
            "uuid 123e4567-e89b-12d3-a456-426614174000 names the bucket",
        ] {
            assert_eq!(secret_kind(text), None, "{text}");
        }
    }
}
