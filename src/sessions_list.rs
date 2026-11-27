//! `junees sessions list`: the recent sessions, to pick an id for
//! `junees --resume`.
//!
//! Reads what the `/sessions` list reads (the meta line and the first
//! entry of each journal, newest file first) and changes nothing. A
//! session's source is told the way the search tells it
//! ([`crate::search::origin`]), and its title is the dashboard's.

use std::path::Path;

use serde_json::{Value, json};

use crate::config_file::Settings;
use crate::prompt_text::escape_hidden;
use crate::search::TERMINAL;
use crate::session;

/// Sessions listed when no limit is given.
pub const DEFAULT_LIMIT: usize = 10;

/// The most sessions one list shows.
pub const MAX_LIMIT: usize = 50;

/// Characters of a session's title: the dashboard's `/api/sessions` cuts
/// its titles here too.
pub const TITLE_CHARS: usize = 80;

/// Characters of a working folder shown in the text list.
const FOLDER_CHARS: usize = 200;

/// The version of the `--json` object.
pub const SCHEMA_VERSION: u32 = 1;

pub const USAGE: &str = "usage: junees sessions list [--all] [--limit N] \
                         [--source terminal|discord|telegram|slack|cron|webhook|mcp|all] [--json]";

/// What `junees sessions list` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListArgs {
    /// Every working folder, not only the current one.
    pub all: bool,
    pub limit: usize,
    /// The terminal's sessions unless `--source` names others: those are
    /// the ones `junees --resume` continues.
    pub sources: Vec<String>,
    pub json: bool,
}

impl Default for ListArgs {
    fn default() -> Self {
        Self {
            all: false,
            limit: DEFAULT_LIMIT,
            sources: vec![TERMINAL.to_string()],
            json: false,
        }
    }
}

impl ListArgs {
    /// Parses the options; the command takes no words.
    pub fn parse(tokens: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = Self::default();
        let mut sources: Option<Vec<String>> = None;
        let mut tokens = tokens.into_iter();
        while let Some(token) = tokens.next() {
            match token.as_str() {
                "--all" => args.all = true,
                "--json" => args.json = true,
                "--limit" => {
                    let value = tokens.next().ok_or("--limit needs a number")?;
                    args.limit = value
                        .parse()
                        .ok()
                        .filter(|n| (1..=MAX_LIMIT).contains(n))
                        .ok_or(format!(
                            "--limit must be a whole number from 1 to {MAX_LIMIT}, got `{}`",
                            escape_hidden(&value)
                        ))?;
                }
                "--source" => {
                    let value = tokens.next().ok_or("--source needs a source name")?;
                    let parsed = crate::search::parse_sources(value.split(','))?;
                    let list = sources.get_or_insert_with(Vec::new);
                    for source in parsed {
                        if !list.contains(&source) {
                            list.push(source);
                        }
                    }
                }
                other if other.starts_with('-') => {
                    return Err(format!(
                        "unknown option `{}`; {USAGE}",
                        escape_hidden(other)
                    ));
                }
                other => {
                    return Err(format!(
                        "`junees sessions list` takes no words (got `{}`); to look for words \
                         use `junees sessions search`; {USAGE}",
                        escape_hidden(other)
                    ));
                }
            }
        }
        if let Some(sources) = sources {
            args.sources = sources;
        }
        Ok(args)
    }
}

/// One session of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: String,
    /// Last write of the journal, in Unix seconds.
    pub updated: u64,
    pub cwd: String,
    /// [`session::Meta::backend`].
    pub backend: Option<String>,
    /// [`TERMINAL`] or where else the session took place.
    pub source: String,
    /// [`title`] of the first input.
    pub title: Option<String>,
}

/// The title of a session whose first input is `first_input`: its first
/// line with text, with secrets cut out before the cut to [`TITLE_CHARS`]
/// characters so that no piece of one is left there. `None` when there is
/// nothing to show. The dashboard's `/api/sessions` and
/// `junees sessions list` share it.
pub fn title(settings: &Settings, first_input: &str) -> Option<String> {
    let first_line = first_input
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let mut text = first_line.trim().to_string();
    crate::doctor::hide_secrets_in_user_text(settings, std::iter::once(&mut text));
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(TITLE_CHARS).collect())
}

/// The sessions of the state folder `home` that `args` ask for, most
/// recently written first: those started in `cwd` unless `args.all`.
pub fn list(
    home: &Path,
    cwd: &Path,
    settings: &Settings,
    args: &ListArgs,
) -> anyhow::Result<Vec<Listed>> {
    let folder = (!args.all).then(|| cwd.to_string_lossy().into_owned());
    let old_channels = crate::search::gateway_channels(home);
    let found = session::list_tagged(home, args.limit, |meta| {
        if folder.as_ref().is_some_and(|folder| *folder != meta.cwd) {
            return None;
        }
        let (source, _) = crate::search::origin(meta, &old_channels);
        args.sources.contains(&source).then_some(source)
    })?;
    Ok(found
        .into_iter()
        .map(|(source, summary)| Listed {
            title: title(settings, &summary.first_input),
            id: summary.id,
            updated: summary.updated,
            cwd: summary.cwd,
            backend: summary.backend,
            source,
        })
        .collect())
}

/// `text` as one line that shows what it holds: hidden characters
/// escaped ([`escape_hidden`]), and the tab, line end and carriage return
/// it lets through written as `\t`, `\n` and `\r` too, so a value cannot
/// split a session's line or pass for another one.
fn one_line(text: &str) -> String {
    escape_hidden(text)
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// [`one_line`] cut to `max` characters.
fn shown(text: &str, max: usize) -> String {
    let text = one_line(text);
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{cut}...")
}

/// Where the list looked, in words.
fn scope(cwd: &Path, args: &ListArgs) -> String {
    let mut out = if args.all {
        "in every folder".to_string()
    } else {
        format!(
            "in this folder ({})",
            shown(&cwd.to_string_lossy(), FOLDER_CHARS)
        )
    };
    out.push_str(&format!(" from {}", args.sources.join(", ")));
    out
}

/// What `junees sessions list` prints: a line per session (id, last write
/// in UTC, backend and source tags, the working folder when every folder
/// is listed, the start of the title), then how to continue one.
pub fn for_terminal(sessions: &[Listed], cwd: &Path, args: &ListArgs) -> String {
    if sessions.is_empty() {
        let mut out = format!("No saved sessions {}.", scope(cwd, args));
        if !args.all {
            out.push_str(" --all lists every folder.");
        }
        out.push('\n');
        return out;
    }
    let mut out = String::new();
    for listed in sessions {
        let mut parts = vec![
            one_line(&listed.id),
            format!("{} UTC", session::format_utc(listed.updated)),
        ];
        if let Some(backend) = &listed.backend {
            parts.push(format!("[{}]", one_line(backend)));
        }
        if listed.source != TERMINAL {
            parts.push(format!("[{}]", one_line(&listed.source)));
        }
        if args.all {
            parts.push(shown(&listed.cwd, FOLDER_CHARS));
        }
        parts.push(match &listed.title {
            Some(title) => crate::cli::preview(&one_line(title), crate::cli::SESSION_PREVIEW_CHARS),
            None => "(empty)".to_string(),
        });
        out.push_str(&parts.join("  "));
        out.push('\n');
    }
    out.push_str("To continue a terminal session, run `junees --resume <id>`.\n");
    out
}

/// What `junees sessions list --json` prints: one object and a line end.
pub fn for_json(sessions: &[Listed], cwd: &Path, args: &ListArgs) -> String {
    let sessions: Vec<Value> = sessions
        .iter()
        .map(|listed| {
            json!({
                "id": listed.id,
                "updated_at": session::format_utc_seconds(listed.updated),
                "cwd": listed.cwd,
                "backend": listed.backend.as_deref().unwrap_or(API_BACKEND),
                "source": listed.source,
                "title": listed.title,
            })
        })
        .collect();
    let folder = (!args.all).then(|| cwd.to_string_lossy().into_owned());
    let mut out = json!({
        "schema_version": SCHEMA_VERSION,
        "scope": { "all": args.all, "folder": folder },
        "sessions": sessions,
    })
    .to_string();
    out = escape_hidden_json(&out);
    out.push('\n');
    out
}

/// `json` with every hidden character ([`crate::prompt_text::is_hidden`])
/// that `serde_json` leaves as it is (U+007F, the C1 controls, the
/// bidirectional formatting characters, U+2028 and U+2029) written as a
/// `\uXXXX` escape, so that the output cannot act on a terminal. They are
/// all in the Basic Multilingual Plane, and outside a string value they
/// cannot occur, so the parsed value stays the same.
fn escape_hidden_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if crate::prompt_text::is_hidden(c) {
            debug_assert!(u32::from(c) <= 0xFFFF);
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

/// The `backend` of a session whose meta line names none: Junees's own
/// agent on the API ([`session::Meta::backend`]).
const API_BACKEND: &str = "api";

/// What `junees sessions list` prints for `args`.
pub fn run(
    home: &Path,
    cwd: &Path,
    settings: &Settings,
    args: &ListArgs,
) -> anyhow::Result<String> {
    let sessions = list(home, cwd, settings, args)?;
    Ok(if args.json {
        for_json(&sessions, cwd, args)
    } else {
        for_terminal(&sessions, cwd, args)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SOURCES;

    fn parse(tokens: &[&str]) -> Result<ListArgs, String> {
        ListArgs::parse(tokens.iter().map(|t| t.to_string()))
    }

    fn settings() -> Settings {
        Settings::new(
            |_: &str| None,
            crate::config_file::ConfigFile::default(),
            None,
            false,
        )
    }

    fn listed(id: &str, source: &str, title: Option<&str>) -> Listed {
        Listed {
            id: id.to_string(),
            updated: 1_763_000_000,
            cwd: "/w/a b".to_string(),
            backend: None,
            source: source.to_string(),
            title: title.map(Into::into),
        }
    }

    #[test]
    fn defaults_are_this_folder_terminal_ten() {
        assert_eq!(
            parse(&[]).unwrap(),
            ListArgs {
                all: false,
                limit: 10,
                sources: vec!["terminal".into()],
                json: false,
            }
        );
    }

    #[test]
    fn parses_every_option() {
        let args = parse(&[
            "--json",
            "--source",
            "discord,cron",
            "--all",
            "--limit",
            "50",
            "--source",
            "cron",
            "--source",
            "terminal",
        ])
        .unwrap();
        assert_eq!(
            args,
            ListArgs {
                all: true,
                limit: 50,
                sources: vec!["discord".into(), "cron".into(), "terminal".into()],
                json: true,
            }
        );
        assert_eq!(parse(&["--limit", "1"]).unwrap().limit, 1);
        assert_eq!(parse(&["--source", "all"]).unwrap().sources, SOURCES);
    }

    #[test]
    fn refuses_what_it_does_not_take() {
        for (tokens, expected) in [
            (&["--limit"][..], "--limit needs a number"),
            (
                &["--limit", "0"],
                "--limit must be a whole number from 1 to 50, got `0`",
            ),
            (
                &["--limit", "51"],
                "--limit must be a whole number from 1 to 50, got `51`",
            ),
            (&["--limit", "-1"], "got `-1`"),
            (&["--limit", "2.5"], "got `2.5`"),
            (&["--limit", "many"], "got `many`"),
            (&["--source"], "--source needs a source name"),
            (&["--source", "irc"], "unknown session source `irc`"),
            (&["--source", ""], "unknown session source ``"),
            (
                &["--session", "x"],
                "unknown option `--session`; usage: junees sessions list",
            ),
            (&["-a"], "unknown option `-a`"),
            (&["빌드"], "takes no words (got `빌드`)"),
        ] {
            let err = parse(tokens).unwrap_err();
            assert!(err.contains(expected), "{tokens:?}: {err}");
        }
        // An escape in a refused word shows instead of acting.
        let err = parse(&["\u{1b}[2J"]).unwrap_err();
        assert!(!err.contains('\u{1b}') && err.contains("\\u{1b}"), "{err}");
    }

    #[test]
    fn usage_names_every_source() {
        for source in SOURCES {
            assert!(USAGE.contains(source), "{source}");
        }
    }

    #[test]
    fn a_title_is_the_first_line_with_text_cut_on_characters() {
        let settings = settings();
        assert_eq!(title(&settings, ""), None);
        assert_eq!(title(&settings, " \n\t\n"), None);
        assert_eq!(
            title(&settings, "\n  빌드 실패 원인  \n둘째 줄").as_deref(),
            Some("빌드 실패 원인")
        );
        let long = "가".repeat(TITLE_CHARS + 5);
        assert_eq!(title(&settings, &long).unwrap(), "가".repeat(TITLE_CHARS));
        assert_eq!(
            title(&settings, &"가".repeat(TITLE_CHARS)).unwrap(),
            "가".repeat(TITLE_CHARS)
        );
    }

    #[test]
    fn a_title_shows_no_secret() {
        let key = format!("sk-ant-api03-{}", "aB3dE6gH9".repeat(6));
        let shown = title(&settings(), &format!("키는 {key} 입니다")).unwrap();
        assert!(!shown.contains(&key[..16]), "{shown}");
        assert!(shown.starts_with("키는 "), "{shown}");
    }

    #[test]
    fn text_lines_carry_tags_and_folders_only_when_asked() {
        let cwd = Path::new("/w/a b");
        let mut claude = listed("20261101-101010-aaaa", "terminal", Some("첫 입력"));
        claude.backend = Some("claude-code".into());
        let discord = listed("20261102-101010-bbbb", "discord", None);
        let here = ListArgs::default();
        assert_eq!(
            for_terminal(&[claude.clone(), discord.clone()], cwd, &here),
            "20261101-101010-aaaa  2025-11-13 02:13 UTC  [claude-code]  첫 입력\n\
             20261102-101010-bbbb  2025-11-13 02:13 UTC  [discord]  (empty)\n\
             To continue a terminal session, run `junees --resume <id>`.\n"
        );
        let all = ListArgs {
            all: true,
            ..ListArgs::default()
        };
        assert_eq!(
            for_terminal(&[claude], cwd, &all),
            "20261101-101010-aaaa  2025-11-13 02:13 UTC  [claude-code]  /w/a b  첫 입력\n\
             To continue a terminal session, run `junees --resume <id>`.\n"
        );
    }

    #[test]
    fn a_long_title_is_cut_like_the_sessions_list_and_escapes_show() {
        let cwd = Path::new("/w");
        let long = listed("a", "terminal", Some(&"가".repeat(80)));
        let out = for_terminal(&[long], cwd, &ListArgs::default());
        let line = out.lines().next().unwrap();
        assert!(
            line.ends_with(&format!("  {}...", "가".repeat(57))),
            "{line}"
        );
        let hidden = listed("a", "terminal", Some("x\u{1b}[2Jy\u{202e}z"));
        let out = for_terminal(&[hidden], cwd, &ListArgs::default());
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{202e}'),
            "{out}"
        );
    }

    #[test]
    fn values_never_split_a_line() {
        assert_eq!(
            one_line("a\nb\r\nc\td\re\u{1b}f"),
            "a\\nb\\r\\nc\\td\\re\\u{1b}f"
        );
        let mut one = listed("x\n2026-11-01 00:00 UTC  fake", "terminal", Some("t"));
        one.cwd = "/w\r\n/x\ty".to_string();
        one.backend = Some("b\nc".to_string());
        let all = ListArgs {
            all: true,
            ..ListArgs::default()
        };
        let out = for_terminal(&[one], Path::new("/w"), &all);
        // One session line and the closing hint.
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(!out.contains('\t') && !out.contains('\r'), "{out:?}");
        assert!(out.contains("/w\\r\\n/x\\ty"), "{out}");
        let empty = for_terminal(&[], Path::new("/w\n/x\ty"), &ListArgs::default());
        assert_eq!(empty.lines().count(), 1, "{empty}");
        assert!(empty.contains("/w\\n/x\\ty"), "{empty}");
    }

    #[test]
    fn json_writes_hidden_characters_as_escapes() {
        let mut one = listed(
            "a",
            "terminal",
            Some("t\u{9b}1\u{202e}2\u{2028}3\u{7f}4\u{200e}5"),
        );
        one.cwd = "/w\u{85}\u{2066}x\n".to_string();
        let out = for_json(&[one.clone()], Path::new("/w\u{9b}"), &ListArgs::default());
        for c in [
            '\u{9b}', '\u{202e}', '\u{2028}', '\u{7f}', '\u{200e}', '\u{85}', '\u{2066}',
        ] {
            assert!(!out.contains(c), "{c:?} in {out:?}");
        }
        assert!(out.contains("\\u009b") && out.contains("\\u202e") && out.contains("\\u2028"));
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["sessions"][0]["title"], one.title.unwrap());
        assert_eq!(value["sessions"][0]["cwd"], one.cwd);
        assert_eq!(value["scope"]["folder"], "/w\u{9b}");
        // Hangul and other text stay as written.
        let plain = for_json(
            &[listed("a", "terminal", Some("한글 é"))],
            Path::new("/w"),
            &ListArgs::default(),
        );
        assert!(plain.contains("한글 é"), "{plain}");
    }

    #[test]
    fn an_empty_list_says_where_it_looked() {
        let cwd = Path::new("/w/a b");
        assert_eq!(
            for_terminal(&[], cwd, &ListArgs::default()),
            "No saved sessions in this folder (/w/a b) from terminal. --all lists every folder.\n"
        );
        let all = ListArgs {
            all: true,
            sources: vec!["discord".into(), "cron".into()],
            ..ListArgs::default()
        };
        assert_eq!(
            for_terminal(&[], cwd, &all),
            "No saved sessions in every folder from discord, cron.\n"
        );
    }

    #[test]
    fn json_is_one_object_with_the_scope() {
        let cwd = Path::new("/w/a b");
        let mut claude = listed("20261101-101010-aaaa", "terminal", Some("첫 \"입력\""));
        claude.backend = Some("claude-code".into());
        let discord = listed("20261102-101010-bbbb", "discord", None);
        let out = for_json(&[claude, discord], cwd, &ListArgs::default());
        assert!(
            out.ends_with('\n') && out.matches('\n').count() == 1,
            "{out}"
        );
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "scope": { "all": false, "folder": "/w/a b" },
                "sessions": [
                    {
                        "id": "20261101-101010-aaaa",
                        "updated_at": "2025-11-13T02:13:20Z",
                        "cwd": "/w/a b",
                        "backend": "claude-code",
                        "source": "terminal",
                        "title": "첫 \"입력\"",
                    },
                    {
                        "id": "20261102-101010-bbbb",
                        "updated_at": "2025-11-13T02:13:20Z",
                        "cwd": "/w/a b",
                        "backend": "api",
                        "source": "discord",
                        "title": null,
                    },
                ],
            })
        );
        let all = ListArgs {
            all: true,
            ..ListArgs::default()
        };
        let value: Value = serde_json::from_str(&for_json(&[], cwd, &all)).unwrap();
        assert_eq!(
            value,
            json!({
                "schema_version": 1,
                "scope": { "all": true, "folder": null },
                "sessions": [],
            })
        );
    }
}
