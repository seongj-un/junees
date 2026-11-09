//! Refuses text that looks like it holds a credential, so a skill never
//! keeps an API key or a private key on disk and in later prompts.

/// Hex runs at least this long count as a token (a git SHA-1 is 40).
const HEX_TOKEN_CHARS: usize = 48;
/// Runs of base64/base64url characters at least this long, mixing upper
/// and lower case letters and digits, count as a token.
const BASE64_TOKEN_CHARS: usize = 64;
/// ... unless they are paths: some `/`-free piece must be this long.
const BASE64_PIECE_CHARS: usize = 32;

/// Prefixes of well-known credentials, with what they are.
const PREFIXES: &[(&str, &str)] = &[
    ("sk-ant-", "an Anthropic API key"),
    ("ghp_", "a GitHub token"),
    ("gho_", "a GitHub token"),
    ("ghs_", "a GitHub token"),
    ("ghu_", "a GitHub token"),
    ("github_pat_", "a GitHub token"),
    ("glpat-", "a GitLab token"),
    ("xoxb-", "a Slack token"),
    ("xoxp-", "a Slack token"),
    ("xoxa-", "a Slack token"),
    ("xoxr-", "a Slack token"),
    ("xoxs-", "a Slack token"),
    ("AKIA", "an AWS access key"),
];

/// What `text` seems to hold and on which line (1-based), if it looks like
/// a secret: a known key prefix followed by key characters, a PEM private
/// key, or a long random-looking hex or base64 token.
pub fn find_secret(text: &str) -> Option<(&'static str, usize)> {
    for (index, line) in text.lines().enumerate() {
        if let Some(kind) = line_secret(line) {
            return Some((kind, index + 1));
        }
    }
    None
}

/// An error message for the model when `text` looks like it holds a
/// secret; the secret itself is not repeated.
pub fn check(text: &str, what: &str) -> Result<(), String> {
    match find_secret(text) {
        None => Ok(()),
        Some((kind, line)) => Err(format!(
            "refused: {what} looks like it holds {kind} (line {line}). Skills are kept as \
             plain files and shown to later sessions, so they must not hold credentials; \
             name the environment variable or file the secret comes from instead."
        )),
    }
}

fn line_secret(line: &str) -> Option<&'static str> {
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    for (prefix, kind) in PREFIXES {
        for (at, _) in line.match_indices(prefix) {
            // A word of its own: `task-ant-` or `MAKIA` are not keys.
            let before = line[..at].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            let rest = &line[at + prefix.len()..];
            let body = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .count();
            if body >= 12 {
                return Some(kind);
            }
        }
    }
    let is_token_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-');
    for run in line.split(|c: char| !is_token_char(c)) {
        if run.len() >= HEX_TOKEN_CHARS
            && run.split(|c: char| !c.is_ascii_hexdigit()).any(|hex| {
                // Random hex mixes digits and letters; `fff...` is not a key.
                hex.len() >= HEX_TOKEN_CHARS
                    && hex.bytes().any(|b| b.is_ascii_digit())
                    && hex.bytes().any(|b| b.is_ascii_alphabetic())
            })
        {
            return Some("a long hex token");
        }
        if run.len() >= BASE64_TOKEN_CHARS
            && run.chars().any(|c| c.is_ascii_uppercase())
            && run.chars().any(|c| c.is_ascii_lowercase())
            && run.chars().any(|c| c.is_ascii_digit())
            && run
                .split('/')
                .any(|piece| piece.len() >= BASE64_PIECE_CHARS)
        {
            return Some("a long base64 token");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_keys_and_tokens() {
        let hex = "0123456789abcdef".repeat(4);
        let base64 = "aZ09".repeat(20);
        let cases = [
            (
                "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnop",
                "an Anthropic API key",
            ),
            (
                "token: ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "a GitHub token",
            ),
            ("SLACK=xoxb-1234567890-abcdefghij", "a Slack token"),
            ("aws AKIAIOSFODNN7EXAMPLE", "an AWS access key"),
            ("-----BEGIN OPENSSH PRIVATE KEY-----", "a private key"),
            ("-----BEGIN RSA PRIVATE KEY-----", "a private key"),
        ];
        for (text, kind) in cases {
            assert_eq!(find_secret(text), Some((kind, 1)), "{text}");
        }
        assert_eq!(
            find_secret(&format!("a\nb\nsecret={hex}")),
            Some(("a long hex token", 3))
        );
        assert_eq!(
            find_secret(&format!("key: \"{base64}\"")),
            Some(("a long base64 token", 1))
        );
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for text in [
            "Run `cargo test -q` and check the output.",
            "commit 0123456789abcdef0123456789abcdef01234567",
            "/home/user/work/project/src/some_module/with_a_long_name/and_more_parts/File9.rs",
            "Set ANTHROPIC_API_KEY in the environment, never in the skill.",
            "keys start with sk-ant- and GitHub tokens with ghp_",
            "task-ant-something-quite-long-here",
            "가나다라마바사아자차카타파하 한국어 설명도 그대로",
            &"f".repeat(80),
            &"0".repeat(80),
            "https://example.com/a/b/c?x=1&y=2",
        ] {
            assert_eq!(find_secret(text), None, "{text}");
        }
    }

    #[test]
    fn check_names_the_kind_without_the_secret() {
        let err = check("x\nsk-ant-api03-abcdefghijklmnop", "the content").unwrap_err();
        assert!(err.contains("an Anthropic API key (line 2)"), "{err}");
        assert!(!err.contains("abcdefghijklmnop"), "{err}");
        assert_eq!(check("plain", "the content"), Ok(()));
    }
}
