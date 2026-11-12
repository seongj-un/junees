//! Recognizes text that looks like a credential, so Junees never saves
//! one where every later session shows it to the model: memory entries
//! and skill files are checked against the same rules.
//!
//! The rules look for well-known key prefixes, PEM private keys, AWS key
//! ids, and long random-looking runs. Paths, URLs, UUIDs, short commit ids
//! and full hashes named as such stay allowed.

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
    ("xapp-", "a Slack token"),
];

/// Key characters that must follow a [`PREFIXES`] entry. Every real
/// format is much longer; placeholders such as `ghp_EXAMPLE` or
/// `sk-ant-api03-...` stay allowed.
const PREFIX_BODY: usize = 12;

/// Prefixes of AWS access key ids (long-term and temporary), each
/// followed by 16 upper-case letters or digits.
const AWS_PREFIXES: [&str; 2] = ["AKIA", "ASIA"];

/// A run of hex digits at least this long reads as a token: API keys of
/// many services are 32 hex digits. Short commit ids stay allowed, a run
/// of one repeated digit (`ffff...`) is no key, and full hashes next to a
/// word that says so pass ([`is_named_hash`]). Length alone does not let
/// a 40-digit run pass: some services use 40-digit hex tokens.
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

/// A run of token characters (`A-Za-z0-9+=_-`; `/` and `.` end it) at
/// least this long with upper-case letters, lower-case letters and digits
/// reads as a token.
const MIXED_RUN: usize = 32;

/// What kind of secret `text` seems to hold and on which line (1-based),
/// if any. Lines are checked in order.
pub fn find_secret(text: &str) -> Option<(&'static str, usize)> {
    find(text, false)
}

/// Like [`find_secret`], for text Junees sends for others to read (GitHub
/// comments, reviews and pull requests): a run of exactly 40 or 64 hex
/// digits always passes, in any case and inside links
/// (`https://github.com/o/n/commit/<sha>`, `#diff-<sha256>R12`), since
/// such text names commits all the time. Key prefixes, private keys, AWS
/// key ids, Discord tokens and other long hex or random-looking runs are
/// still found.
pub fn find_outgoing_secret(text: &str) -> Option<(&'static str, usize)> {
    find(text, true)
}

/// `hashes`: whether 40- and 64-digit hex runs pass without a word naming
/// them ([`find_outgoing_secret`]).
fn find(text: &str, hashes: bool) -> Option<(&'static str, usize)> {
    let mut start = 0;
    for (index, line) in text.split('\n').enumerate() {
        if let Some(kind) = line_secret(text, start, line, hashes) {
            return Some((kind, index + 1));
        }
        start += line.len() + 1;
    }
    None
}

/// What kind of secret `text` seems to hold, if any.
pub fn secret_kind(text: &str) -> Option<&'static str> {
    find_secret(text).map(|(kind, _)| kind)
}

/// The secret in `line`, which starts at byte `start` of `text` (the words
/// around a hash may sit on the lines next to it). `hashes` as in [`find`].
fn line_secret(text: &str, start: usize, line: &str, hashes: bool) -> Option<&'static str> {
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
        return Some("a private key");
    }
    if let Some(&(_, kind)) = PREFIXES
        .iter()
        .find(|(prefix, _)| has_prefixed_token(line, prefix))
    {
        return Some(kind);
    }
    if has_aws_key_id(line) {
        return Some("an AWS access key id");
    }
    if has_discord_token(line) {
        return Some("a Discord bot token");
    }
    if has_telegram_token(line) {
        return Some("a Telegram bot token");
    }
    for (at, run) in runs(line) {
        if has_hex_token(text, start + at, run, hashes) {
            return Some("a long hex token");
        }
        // A hash glued to more characters (`diff-<sha256>R12`) is no
        // random-looking run when hashes pass.
        let pieces = if hashes {
            without_hashes(run)
        } else {
            vec![run]
        };
        if pieces.iter().any(|piece| {
            piece.len() >= MIXED_RUN
                && piece.bytes().any(|b| b.is_ascii_uppercase())
                && piece.bytes().any(|b| b.is_ascii_lowercase())
                && piece.bytes().any(|b| b.is_ascii_digit())
        }) {
            return Some("a long random-looking token");
        }
    }
    None
}

/// The parts of the token run `run` (ASCII) around its stretches of
/// exactly 40 or 64 hex digits.
fn without_hashes(run: &str) -> Vec<&str> {
    let bytes = run.as_bytes();
    let mut pieces = Vec::new();
    let mut piece_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let begin = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if HASH_LENGTHS.contains(&(i - begin)) {
            pieces.push(&run[piece_start..begin]);
            piece_start = i;
        }
    }
    pieces.push(&run[piece_start..]);
    pieces
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

/// Whether the token run `run` at byte `at` of `text` holds a hex token:
/// a stretch of at least [`HEX_RUN`] hex digits (`KEY=<hex>` too), not one
/// repeated digit, and not a named hash (with `hashes`, not any full hash).
fn has_hex_token(text: &str, at: usize, run: &str, hashes: bool) -> bool {
    let mut offset = 0;
    run.split(|c: char| !c.is_ascii_hexdigit()).any(|hex| {
        let here = at + offset;
        offset += hex.len() + 1;
        hex.len() >= HEX_RUN
            && hex.bytes().any(|b| b != hex.as_bytes()[0])
            && !(hashes && HASH_LENGTHS.contains(&hex.len()))
            && !is_named_hash(text, here, hex)
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
    // A run glued to more token characters (`commit 1a2b...0`) is no hash.
    let before = text[..at].chars().next_back();
    let after = text[at + run.len()..].chars().next();
    if [before, after]
        .into_iter()
        .flatten()
        .any(|c| c.is_ascii_alphanumeric())
    {
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
/// and at least [`PREFIX_BODY`] key characters follow it.
fn has_prefixed_token(text: &str, prefix: &str) -> bool {
    text.match_indices(prefix).any(|(at, _)| {
        let starts_word = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let body = text[at + prefix.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .count();
        starts_word && body >= PREFIX_BODY
    })
}

/// Whether `text` holds an AWS access key id as a word of its own: an
/// [`AWS_PREFIXES`] entry and 16 upper-case letters or digits, also after
/// `=` or `_` (`AWS_ACCESS_KEY_ID=AKIA...`).
fn has_aws_key_id(text: &str) -> bool {
    AWS_PREFIXES.iter().any(|prefix| {
        text.match_indices(prefix).any(|(at, _)| {
            let starts_word = text[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            let body = &text.as_bytes()[at + prefix.len()..];
            starts_word
                && body.len() >= 16
                && body[..16]
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                && body.get(16).is_none_or(|b| !b.is_ascii_alphanumeric())
        })
    })
}

/// The lengths a Discord bot token's first part (the bot's user id, a
/// snowflake of 17 to 20 digits, in unpadded base64) can have.
const DISCORD_ID_PART: std::ops::RangeInclusive<usize> = 23..=28;
/// The length of its second part (a timestamp).
const DISCORD_TIME_PART: std::ops::RangeInclusive<usize> = 6..=7;
/// The shortest third part (an HMAC; 27 characters in old tokens, 38 now).
const DISCORD_HMAC_PART: usize = 27;

/// Whether `text` holds a Discord bot token: three base64 parts joined by
/// dots, the first of which decodes to the bot's numeric user id. The id
/// check keeps dotted names (`a.b.c`) and JWT-like strings out.
fn has_discord_token(text: &str) -> bool {
    let is_token = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    text.split(|c: char| !is_token(c)).any(|run| {
        let parts: Vec<&str> = run.split('.').collect();
        parts.windows(3).any(|window| {
            let [id, time, hmac] = window else {
                return false;
            };
            DISCORD_ID_PART.contains(&id.len())
                && DISCORD_TIME_PART.contains(&time.len())
                && hmac.len() >= DISCORD_HMAC_PART
                && base64_digits(id)
        })
    })
}

/// The digits of a Telegram bot token's first part (the bot's user id).
const TELEGRAM_ID_PART: std::ops::RangeInclusive<usize> = 6..=12;
/// The length of its secret part, after the colon, as @BotFather issues
/// it (Bot API docs: `123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11`).
const TELEGRAM_SECRET_PART: usize = 35;

/// Whether `text` holds a Telegram bot token: a bot id, a colon and 35
/// characters of letters, digits, `_` and `-`, standing apart from the
/// words around it.
fn has_telegram_token(text: &str) -> bool {
    let bytes = text.as_bytes();
    let secret_char = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-');
    text.match_indices(':').any(|(at, _)| {
        let id = bytes[..at]
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_digit())
            .count();
        let secret = bytes[at + 1..]
            .iter()
            .take_while(|b| secret_char(b))
            .count();
        let start = at - id;
        let apart = |start: usize| start == 0 || !secret_char(&bytes[start - 1]);
        // In a request URL it follows `/bot`.
        let starts_apart =
            apart(start) || (start >= 3 && &bytes[start - 3..start] == b"bot" && apart(start - 3));
        TELEGRAM_ID_PART.contains(&id) && secret == TELEGRAM_SECRET_PART && starts_apart
    })
}

/// Whether `text` is unpadded base64 (standard or URL alphabet) of a
/// string of ASCII digits.
fn base64_digits(text: &str) -> bool {
    let value = |b: u8| -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut decoded = Vec::with_capacity(text.len());
    for chunk in text.as_bytes().chunks(4) {
        let mut bits = 0u32;
        for (index, &b) in chunk.iter().enumerate() {
            let Some(v) = value(b) else {
                return false;
            };
            bits |= v << (18 - 6 * index);
        }
        let bytes = bits.to_be_bytes();
        // 2, 3 or 4 characters carry 1, 2 or 3 bytes.
        let count = match chunk.len() {
            2 => 1,
            3 => 2,
            4 => 3,
            _ => return false,
        };
        decoded.extend_from_slice(&bytes[1..1 + count]);
    }
    !decoded.is_empty() && decoded.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token of the Discord shape, built at run time so no complete fake
    /// token sits in the source: the id `id` in base64, then a timestamp
    /// and an HMAC part.
    fn discord_token(id: &str) -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut encoded = String::new();
        for chunk in id.as_bytes().chunks(3) {
            let mut bits = 0u32;
            for (index, &b) in chunk.iter().enumerate() {
                bits |= u32::from(b) << (16 - 8 * index);
            }
            for index in 0..=chunk.len() {
                encoded.push(alphabet[(bits >> (18 - 6 * index) & 63) as usize] as char);
            }
        }
        format!("{encoded}.Gx{}.{}", "Ab1x", "h7Kq_2-Lm".repeat(4))
    }

    #[test]
    fn finds_discord_bot_tokens() {
        for id in [
            "81384788765712384",
            "1234567890123456789",
            "12345678901234567890",
        ] {
            let token = discord_token(id);
            for text in [
                token.clone(),
                format!("JUNEES_DISCORD_TOKEN={token}"),
                format!("discord_token = \"{token}\""),
                format!("Authorization: Bot {token}"),
            ] {
                assert_eq!(secret_kind(&text), Some("a Discord bot token"), "{text}");
            }
        }
        assert!(base64_digits("MTIzNDU2Nzg5MDEyMzQ1Njc4OQ"));
        assert!(!base64_digits("aGVsbG8gd29ybGQgaGVsbG8"));
    }

    #[test]
    fn finds_telegram_bot_tokens() {
        // Built at run time, so no complete fake token sits in the source.
        let secret = format!("AA{}Hq3_zZ", "x7_Kq-2Lm".repeat(3));
        assert_eq!(secret.len(), TELEGRAM_SECRET_PART);
        let token = format!("{}:{secret}", "1234567890");
        for text in [
            token.clone(),
            format!("JUNEES_TELEGRAM_TOKEN={token}"),
            format!("telegram_token = \"{token}\""),
            format!("https://api.telegram.org/bot{token}/getMe"),
        ] {
            assert_eq!(secret_kind(&text), Some("a Telegram bot token"), "{text}");
        }
        for text in [
            "at 12:34:56 the build ran".to_string(),
            format!("12345:{secret}"),
            format!("id{}:{secret}", "1234567890"),
            format!("{}:{secret}x", "1234567890"),
            "ratio 1234567:12".to_string(),
        ] {
            assert_ne!(secret_kind(&text), Some("a Telegram bot token"), "{text}");
        }
    }

    #[test]
    fn dotted_names_are_no_discord_token() {
        let hmac = "h7Kq_2-Lm".repeat(4);
        for text in [
            "com.example.service.module".to_string(),
            "README.md and src/gateway/mod.rs".to_string(),
            // Base64 of letters, not of a user id.
            format!("aGVsbG8gd29ybGQgaGVsbG8gd29y.Gx1234.{hmac}"),
            // Parts of the wrong length.
            format!("MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.G.{hmac}"),
            "MTIzNDU2Nzg5MDEyMzQ1Njc4OQ.Gx1234.short".to_string(),
        ] {
            assert_ne!(secret_kind(&text), Some("a Discord bot token"), "{text}");
        }
    }

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
                format!("SLACK_APP_TOKEN=xapp-1-A1-{filler}"),
                "a Slack token",
            ),
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
            ("aws AKIAIOSFODNN7EXAMPLE", "an AWS access key id"),
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
            Some(("a long random-looking token", 1))
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
    fn reconciled_rules() {
        // A hex token after `=` or `:` (one run of token characters).
        let hex = "0123456789abcdef".repeat(2);
        for text in [format!("API_TOKEN={hex}"), format!("token:{hex}")] {
            assert_eq!(secret_kind(&text), Some("a long hex token"), "{text}");
        }
        // AWS key ids after `=`, temporary ones, but not inside a word.
        let id = "IOSFODNN7EXAMPLE";
        for text in [
            format!("AWS_ACCESS_KEY_ID=AKIA{id}"),
            format!("ASIA{id}"),
            format!("key \"AKIA{id}\""),
        ] {
            assert_eq!(secret_kind(&text), Some("an AWS access key id"), "{text}");
        }
        for text in [
            format!("MAKIA{id}"),
            format!("AKIA{id}X"),
            format!("AKIA{}", &id[..15]),
        ] {
            assert_eq!(secret_kind(&text), None, "{text}");
        }
        // A prefix needs 12 key characters after it.
        assert_eq!(secret_kind("ghp_abcdefghijk"), None);
        assert_eq!(secret_kind("ghp_abcdefghijkl"), Some("a GitHub token"));
        // One repeated digit is no token; a named hash glued to more text is.
        assert_eq!(secret_kind(&"a".repeat(64)), None);
        let sha1 = "cf302d7c3b6a4f0e9d1a2b3c4d5e6f708192a3b4";
        assert_eq!(
            secret_kind(&format!("commit x{sha1}")),
            Some("a long hex token")
        );
        // A named hash may have its word on the line before.
        assert_eq!(secret_kind(&format!("the commit\n{sha1}")), None);
    }

    #[test]
    fn names_the_line() {
        let hex = "0123456789abcdef".repeat(2);
        assert_eq!(
            find_secret(&format!("first\r\nsecond\n\nkey {hex}\n")),
            Some(("a long hex token", 4))
        );
        assert_eq!(find_secret("one\ntwo"), None);
    }

    #[test]
    fn outgoing_text_may_name_commits_but_not_keys() {
        let sha1 = "cf302d7c3b6a4f0e9d1a2b3c4d5e6f708192a3b4";
        let sha256 = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        // The PR #42 review's examples, which the stored-text rule refuses.
        let review = [
            format!("fixed in https://github.com/o/n/commit/{sha1}"),
            format!("rebased onto {sha1}"),
            format!("https://github.com/o/n/pull/7/commits/{sha1}"),
            format!("commit {}", sha1.to_uppercase()),
        ];
        for text in &review {
            assert_eq!(find_outgoing_secret(text), None, "{text}");
            assert_eq!(secret_kind(text), Some("a long hex token"), "{text}");
        }
        for text in [
            format!("see https://github.com/o/n/blob/{sha1}/src/main.rs#L10"),
            format!("https://github.com/o/n/pull/7/files#diff-{sha256}R12"),
            format!("https://github.com/o/n/compare/{sha1}...{sha1}"),
            format!("image sha256:{sha256}, base {}", sha1.to_uppercase()),
        ] {
            assert_eq!(find_outgoing_secret(&text), None, "{text}");
        }
        // Keys are still found, also next to a commit id.
        let filler = "a1B2c3".repeat(4);
        for (text, kind) in [
            (format!("{sha1} ghp_{filler}"), "a GitHub token"),
            (format!("github_pat_{filler}"), "a GitHub token"),
            (format!("key sk-ant-{filler}"), "an Anthropic API key"),
            (
                format!("-----BEGIN RSA {} KEY-----", "PRIVATE"),
                "a private key",
            ),
            (
                format!("AKIA{}", "ABCD1234EFGH5678"),
                "an AWS access key id",
            ),
            (
                format!("token {}", "0123456789abcdef".repeat(2)),
                "a long hex token",
            ),
            (format!("token {}", &sha256[..48]), "a long hex token"),
            (format!("commit {sha1}0"), "a long hex token"),
            (
                format!("Bearer {}", "Zm9vYmFyQmF6".repeat(3)),
                "a long random-looking token",
            ),
            (
                format!("diff-{sha1}{}", "Zm9vYmFyQmF6".repeat(3)),
                "a long random-looking token",
            ),
        ] {
            assert_eq!(
                find_outgoing_secret(&text).map(|(k, _)| k),
                Some(kind),
                "{text}"
            );
        }
    }
}
