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
/// many services are 32 hex digits). Short commit ids stay allowed, and
/// so do full hashes next to a word that says so ([`is_named_hash`]).
const HEX_RUN: usize = 32;

/// The lengths of a full git SHA-1 (40) and a SHA-256 (64) in hex.
const HASH_LENGTHS: [usize; 2] = [40, 64];

/// Words that, right before or after a 40- or 64-digit hex run, say it is
/// a hash and not a token: `commit 1a2b...`, `sha256:1a2b...`,
/// `1a2b... 커밋`. A word counts when it starts with one of these, case
/// ignored (`commits`, `sha1`, `sha-256`, `커밋을`).
const HASH_WORDS: &[&str] = &[
    "commit",
    "sha",
    "hash",
    "checksum",
    "digest",
    "rev",
    "커밋",
    "해시",
    "체크섬",
];

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
    for (at, run) in runs(text) {
        if is_aws_key_id(run) {
            return Some("an AWS access key id");
        }
        if run.len() >= HEX_RUN
            && run.bytes().all(|b| b.is_ascii_hexdigit())
            && !is_named_hash(text, at, run)
        {
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

/// The maximal runs of token characters (`A-Za-z0-9+=_-`) in `text`, with
/// their byte offsets. Every other character, `/` and `.` included, ends a
/// run, so paths and file names split into short pieces.
fn runs(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let is_token = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '_' | '-');
    let mut rest = text.char_indices().peekable();
    std::iter::from_fn(move || {
        while rest.next_if(|&(_, c)| !is_token(c)).is_some() {}
        let (start, _) = *rest.peek()?;
        let mut end = start;
        while let Some((at, c)) = rest.next_if(|&(_, c)| is_token(c)) {
            end = at + c.len_utf8();
        }
        Some((start, &text[start..end]))
    })
}

/// Whether the hex run `run` at byte `at` of `text` is a full git SHA-1 or
/// SHA-256 named as such: 40 or 64 lower-case hex digits (as git and
/// `sha256sum` print them) with a [`HASH_WORDS`] word right before or after
/// it. Everything else that long still reads as a token.
fn is_named_hash(text: &str, at: usize, run: &str) -> bool {
    if !HASH_LENGTHS.contains(&run.len()) || run.bytes().any(|b| b.is_ascii_uppercase()) {
        return false;
    }
    // The words around it, without punctuation (`sha256:`, `(commit`).
    let before = text[..at].split_whitespace().next_back();
    let after = text[at + run.len()..].split_whitespace().next();
    [before, after].into_iter().flatten().any(|word| {
        let word = word
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        HASH_WORDS
            .iter()
            .any(|hash_word| word.starts_with(hash_word))
    })
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

    #[test]
    fn full_hashes_pass_when_named() {
        let sha1 = "cf302d7c3b6a4f0e9d1a2b3c4d5e6f708192a3b4";
        let sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        for text in [
            format!("main은 {sha1} 커밋 이후 PR 규칙이 바뀜"),
            format!("Rules changed after commit {sha1}."),
            format!("(commit: {sha1})"),
            format!("Pinned to git rev {sha1}"),
            format!("{sha1} commit fixed the build"),
            format!("release.tar.gz sha256 {sha256}"),
            format!("image digest sha256:{sha256}"),
            format!("SHA-256 checksum: {sha256}"),
            format!("{sha256} 체크섬"),
        ] {
            assert_eq!(secret_kind(&text), None, "{text}");
        }
        // Without such a word, in upper case, or at other lengths it still
        // reads as a token.
        for text in [
            format!("token {sha1}"),
            sha1.to_string(),
            format!("api key: {sha256}"),
            format!("commit {}", sha1.to_uppercase()),
            format!("commit {}", &sha256[..48]),
            format!("commit {sha1}0"),
            format!("committer x {sha1}"),
            format!("hash this: {}", "0123456789abcdef".repeat(2)),
        ] {
            assert_eq!(secret_kind(&text), Some("a long hex token"), "{text}");
        }
    }

    #[test]
    fn runs_keep_their_offsets() {
        assert_eq!(
            runs("가 ab-c/d.e  f").collect::<Vec<_>>(),
            [(4, "ab-c"), (9, "d"), (11, "e"), (14, "f")]
        );
        assert_eq!(runs("").count(), 0);
        assert_eq!(runs("../").count(), 0);
    }
}
