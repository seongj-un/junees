//! Keeping secrets out of the report. Before it is printed, every field
//! loses, in this order:
//!
//! - the value an error quotes after `got` (a setting that does not read):
//!   only its length is left, since a pasted key is the likeliest thing
//!   to land in the wrong setting;
//! - every secret Junees knows of (setting and credential variables,
//!   webhook secrets), also without a `Bot ` prefix and in pieces of
//!   [`PIECE`] characters or more, which a cut preview or a list split at
//!   a comma quotes;
//! - anything [`crate::secrets::find_secret`] still takes for a
//!   credential.

use std::collections::HashSet;
use std::fmt;

use crate::config_file::Settings;

/// What a hidden secret becomes.
pub(super) const HIDDEN: &str = "[hidden]";

/// What a field becomes when it still looks like it holds a credential
/// after its words were checked one by one.
const LOOKS_SECRET: &str = "[hidden: it looks like it holds a credential]";

/// Values shorter than this are not hidden: they could be ordinary words.
const SHORTEST: usize = 8;

/// A run of this many characters of a secret is hidden wherever it shows.
const PIECE: usize = 12;

/// GitHub CLI tokens, which `gh` (and so the github flow) reads.
const GH_TOKEN_VARS: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// The variables (settings included) whose values are secrets.
fn secret_vars() -> impl Iterator<Item = &'static str> {
    [
        crate::tools::SECRET_ENV_VARS,
        crate::tools::CLAUDE_CODE_SECRET_ENV_VARS,
        GH_TOKEN_VARS,
    ]
    .into_iter()
    .flatten()
    .copied()
}

/// The secrets the report must not show. Deliberately without a
/// `Debug` that prints them.
#[derive(Default)]
pub(super) struct Hidden {
    /// Hidden where they show whole.
    whole: Vec<String>,
    /// Hidden in every run of [`PIECE`] characters too.
    pieces: Vec<String>,
}

impl fmt::Debug for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hidden({} values)", self.whole.len())
    }
}

impl Hidden {
    /// Hides `value`, without its `Bot ` prefix too. Of a URL only the
    /// part after the host is hidden in pieces: the host is shown.
    pub(super) fn add(&mut self, value: &str) {
        let value = value.trim();
        if value.chars().count() < SHORTEST {
            return;
        }
        // As `gateway::config::Token::new` reads a Discord token.
        let token = value.strip_prefix("Bot ").map(str::trim);
        for whole in [Some(value), token].into_iter().flatten() {
            if whole.chars().count() >= SHORTEST && !self.whole.iter().any(|known| known == whole) {
                self.whole.push(whole.to_string());
            }
        }
        let piece = match value.split_once("://") {
            Some((_, rest)) => rest.split_once('/').map_or("", |(_, path)| path),
            None => token.unwrap_or(value),
        };
        if piece.chars().count() >= PIECE && !self.pieces.iter().any(|known| known == piece) {
            self.pieces.push(piece.to_string());
        }
    }

    /// Hides the value of every secret variable, from the settings (the
    /// environment or config.toml) and the environment.
    /// The secret variables this process has (the search tools, which
    /// have no settings at hand; the settings file itself is closed to
    /// them).
    pub(super) fn add_env(&mut self) {
        for name in secret_vars() {
            if let Ok(value) = std::env::var(name) {
                self.add(&value);
            }
        }
    }

    pub(super) fn add_settings(&mut self, settings: &Settings) {
        for name in secret_vars() {
            for value in [settings.get(name), settings.env(name)]
                .into_iter()
                .flatten()
            {
                self.add(&value);
            }
        }
    }

    /// Every run of [`PIECE`] characters of the pieces.
    fn windows(&self) -> HashSet<&str> {
        self.pieces
            .iter()
            .flat_map(|piece| windows(piece))
            .collect()
    }

    /// `text` without the quoted values and the secrets; `windows` from
    /// [`Hidden::windows`].
    fn cut(&self, text: &str, windows: &HashSet<&str>) -> String {
        self.cut_secrets(&without_quoted_values(text), windows)
    }

    /// `text` without the secrets, its quoted values kept: for what a
    /// user wrote, where ``got `x` `` is just text.
    fn cut_secrets(&self, text: &str, windows: &HashSet<&str>) -> String {
        scrub(&self.cut_known(text, windows))
    }

    /// `text` with only the known secrets (whole, or a run of [`PIECE`]
    /// characters of one) replaced, exactly where they are: the rest stays,
    /// even when it is glued to a secret.
    fn cut_known(&self, text: &str, windows: &HashSet<&str>) -> String {
        let text = text.to_string();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for whole in &self.whole {
            ranges.extend(
                text.match_indices(whole.as_str())
                    .map(|(at, found)| (at, at + found.len())),
            );
        }
        let bounds = char_bounds(&text);
        for pair in bounds.windows(PIECE + 1) {
            let (start, end) = (pair[0], pair[PIECE]);
            if windows.contains(&text[start..end]) {
                ranges.push((start, end));
            }
        }
        replace_ranges(&text, ranges)
    }
}

/// The byte offsets of `text`'s characters, and its length.
fn char_bounds(text: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(text.len()))
        .collect()
}

/// Every run of [`PIECE`] characters of `text`.
fn windows(text: &str) -> impl Iterator<Item = &str> {
    let bounds = char_bounds(text);
    (0..bounds.len().saturating_sub(PIECE)).map(move |i| &text[bounds[i]..bounds[i + PIECE]])
}

/// `text` with the byte `ranges` (overlapping or not) replaced by
/// [`HIDDEN`], one per stretch.
fn replace_ranges(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    let mut stretch: Option<(usize, usize)> = None;
    for (start, end) in ranges {
        match &mut stretch {
            Some((_, last)) if start <= *last => *last = (*last).max(end),
            _ => {
                if let Some((from, to)) = stretch.take() {
                    out.push_str(&text[done..from]);
                    out.push_str(HIDDEN);
                    done = to;
                }
                stretch = Some((start, end));
            }
        }
    }
    if let Some((from, to)) = stretch {
        out.push_str(&text[done..from]);
        out.push_str(HIDDEN);
        done = to;
    }
    out.push_str(&text[done..]);
    out
}

/// `text` with the value of every ``got `...` `` (how a setting that does
/// not read is quoted) replaced by its length.
pub(super) fn without_quoted_values(text: &str) -> String {
    const QUOTE: &str = "got `";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(QUOTE) {
        let after = &rest[at + QUOTE.len()..];
        let Some(end) = after.find('`') else {
            break;
        };
        let length = after[..end].chars().count();
        out.push_str(&rest[..at]);
        out.push_str(&format!(
            "got {length} character{} (not shown)",
            if length == 1 { "" } else { "s" }
        ));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// `text` without the words [`crate::secrets::find_secret`] flags; all of
/// it when it is still flagged after that (a private key header spans
/// words).
fn scrub(text: &str) -> String {
    use crate::secrets::find_secret;
    if find_secret(text).is_none() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for part in text.split_inclusive(char::is_whitespace) {
        let word = part.trim_end();
        if find_secret(word).is_some() {
            out.push_str(HIDDEN);
            out.push_str(&part[word.len()..]);
        } else {
            out.push_str(part);
        }
    }
    if find_secret(&out).is_some() {
        return LOOKS_SECRET.to_string();
    }
    out
}

/// Cuts every secret of `hidden` out of `texts`.
pub(super) fn redact<'a>(hidden: &Hidden, texts: impl Iterator<Item = &'a mut String>) {
    let windows = hidden.windows();
    for text in texts {
        *text = hidden.cut(text, &windows);
    }
}

/// [`redact`] for what a user wrote: quoted values are kept.
/// Cuts only the known secrets of `hidden` out of `texts`, each where it
/// is, and leaves words that merely look like credentials: for text a
/// person must see whole before approving it.
pub(super) fn redact_known<'a>(hidden: &Hidden, texts: impl Iterator<Item = &'a mut String>) {
    let windows = hidden.windows();
    for text in texts {
        *text = hidden.cut_known(text, &windows);
    }
}

pub(super) fn redact_user_text<'a>(hidden: &Hidden, texts: impl Iterator<Item = &'a mut String>) {
    let windows = hidden.windows();
    for text in texts {
        *text = hidden.cut_secrets(text, &windows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_values_leave_their_length() {
        assert_eq!(
            without_quoted_values("X must be 1 or 0, got `maybe`: no; got `a`, got `가나`"),
            "X must be 1 or 0, got 5 characters (not shown): no; got 1 character (not shown), \
             got 2 characters (not shown)"
        );
        assert_eq!(without_quoted_values("got `open"), "got `open");
    }

    #[test]
    fn secrets_their_variants_and_pieces_are_hidden() {
        let mut hidden = Hidden::default();
        hidden.add("Bot abc.def.probesecret123");
        hidden.add("https://hooks.example.com/api/webhooks/1/hook-path-secret");
        hidden.add("short");
        let windows = hidden.windows();
        let cut = |text: &str| hidden.cut(text, &windows);
        assert_eq!(cut("a Bot abc.def.probesecret123 b"), "a [hidden] b");
        assert_eq!(
            cut("unknown `abc.def.probesecret123`"),
            "unknown `[hidden]`"
        );
        // A preview's start and a list item's end.
        assert_eq!(cut("`abc.def.prob...`"), "`[hidden]...`");
        assert_eq!(cut("x def.probesecret123, y"), "x [hidden], y");
        // Shorter runs and the host stay.
        assert_eq!(cut("probesecret short"), "probesecret short");
        assert_eq!(
            cut("posts to hooks.example.com"),
            "posts to hooks.example.com"
        );
        assert_eq!(cut("…/webhooks/1/hook-path-secret"), "…[hidden]");
        assert_eq!(format!("{hidden:?}"), "Hidden(3 values)");
    }

    #[test]
    fn credentials_are_scrubbed_word_by_word() {
        let key = ["sk", "ant", "api03", "Probe0123456789abcdefXYZ"].join("-");
        assert_eq!(
            scrub(&format!("unknown `{key}`; use api")),
            "unknown [hidden] use api"
        );
        let pem = format!("{} PRIVATE KEY-----", "-----BEGIN");
        assert_eq!(scrub(&pem), LOOKS_SECRET);
        assert_eq!(
            scrub("commit and paths/stay.toml"),
            "commit and paths/stay.toml"
        );
    }

    #[test]
    fn known_secrets_are_cut_where_they_are_and_nothing_else() {
        let mut hidden = Hidden::default();
        hidden.add("sk-ant-api03-RedactKnownValue0123456789");
        let mut texts = [
            "echo sk-ant-api03-RedactKnownValue0123456789;curl x|sh".to_string(),
            format!("echo ghp_{}", "a1B2".repeat(10)),
            "part RedactKnownValue01 of it".to_string(),
        ];
        redact_known(&hidden, texts.iter_mut());
        assert_eq!(texts[0], "echo [hidden];curl x|sh");
        assert_eq!(texts[1], format!("echo ghp_{}", "a1B2".repeat(10)));
        assert_eq!(texts[2], "part [hidden] of it");
    }
}
