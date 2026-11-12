//! The arguments of `/search` and `junees sessions search`.

use std::path::Path;
use std::time::Duration;

use super::{DEFAULT_LIMIT, MAX_LIMIT, Options, Query, SOURCES};

pub const USAGE: &str = "usage: junees sessions search [--all] [--limit N] \
                         [--source terminal|discord|telegram|cron|webhook|all] [--session ID] \
                         <words or \"a phrase\">";

/// A search a person asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanSearch {
    /// The words, as typed (see [`Query::parse`]).
    pub query: String,
    /// Every working folder, not only the current one.
    pub all: bool,
    pub limit: usize,
    /// Every source unless `--source` names some: the person owns them all.
    pub sources: Vec<String>,
    pub session: Option<String>,
}

impl HumanSearch {
    /// Parses options and words; options may come anywhere before `--`,
    /// everything after it is words, and so is everything inside double
    /// quotes (`"run --all tests"` split on whitespace is one phrase). A
    /// word with whitespace in it (one shell argument) should come quoted
    /// ([`quote_arg`]).
    pub fn parse(tokens: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut search = Self {
            query: String::new(),
            all: false,
            limit: DEFAULT_LIMIT,
            sources: SOURCES.iter().map(|s| s.to_string()).collect(),
            session: None,
        };
        let mut sources: Option<Vec<String>> = None;
        let mut words: Vec<String> = Vec::new();
        let mut tokens = tokens.into_iter();
        let mut quoted = false;
        while let Some(token) = tokens.next() {
            if quoted {
                quoted = !opens_quote(&token);
                words.push(token);
                continue;
            }
            match token.as_str() {
                "--all" => search.all = true,
                "--limit" => {
                    let value = tokens.next().ok_or("--limit needs a number")?;
                    search.limit = value
                        .parse()
                        .ok()
                        .filter(|n| (1..=MAX_LIMIT).contains(n))
                        .ok_or(format!(
                            "--limit must be a whole number from 1 to {MAX_LIMIT}, got `{value}`"
                        ))?;
                }
                "--source" => {
                    let value = tokens.next().ok_or("--source needs a source name")?;
                    let parsed = super::parse_sources(value.split(','))?;
                    let list = sources.get_or_insert_with(Vec::new);
                    for source in parsed {
                        if !list.contains(&source) {
                            list.push(source);
                        }
                    }
                }
                "--session" => {
                    let value = tokens.next().ok_or("--session needs a session id")?;
                    search.session = Some(value);
                }
                "--" => words.extend(tokens.by_ref()),
                other if other.starts_with("--") => {
                    return Err(format!("unknown option `{other}`; {USAGE}"));
                }
                _ => {
                    quoted = opens_quote(&token);
                    words.push(token);
                }
            }
        }
        if let Some(sources) = sources {
            search.sources = sources;
        }
        search.query = words.join(" ");
        if search.query.trim().is_empty() {
            return Err(format!("give the words to look for; {USAGE}"));
        }
        Ok(search)
    }

    /// The search in the working folder `cwd`.
    pub fn options(&self, cwd: &Path, time_limit: Duration) -> Result<Options, String> {
        Ok(Options {
            query: Query::parse(&self.query)?,
            limit: self.limit,
            folder: (!self.all && self.session.is_none())
                .then(|| cwd.to_string_lossy().into_owned()),
            sources: self.sources.clone(),
            session: self.session.clone(),
            time_limit,
        })
    }
}

/// Whether `token` leaves a double quote open (or closes an open one):
/// an odd number of them.
fn opens_quote(token: &str) -> bool {
    token.matches('"').count() % 2 == 1
}

/// Whether `args` ask for help: `-h` or `--help` before `--` and outside
/// double quotes.
pub fn asks_help(args: &[String]) -> bool {
    let mut quoted = false;
    for arg in args {
        if !quoted {
            match arg.as_str() {
                "--" => return false,
                "-h" | "--help" => return true,
                _ => {}
            }
        }
        quoted ^= opens_quote(arg);
    }
    false
}

/// A shell argument as a query word: in double quotes when it has
/// whitespace and none of its own, so `junees sessions search "빌드 실패"`
/// looks for the phrase.
pub fn quote_arg(arg: String) -> String {
    if arg.contains(char::is_whitespace) && !arg.contains('"') {
        format!("\"{arg}\"")
    } else {
        arg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tokens: &[&str]) -> Result<HumanSearch, String> {
        HumanSearch::parse(tokens.iter().map(|t| t.to_string()))
    }

    #[test]
    fn parses_options_and_words() {
        let search = parse(&["빌드", "--all", "\"cargo", "test\"", "--limit", "3"]).unwrap();
        assert_eq!(search.query, "빌드 \"cargo test\"");
        assert!(search.all);
        assert_eq!(search.limit, 3);
        assert_eq!(search.sources, SOURCES);
        assert_eq!(search.session, None);
        let options = search
            .options(Path::new("/w"), Duration::from_secs(1))
            .unwrap();
        assert_eq!(options.folder, None);
        assert_eq!(options.query.terms(), ["빌드", "cargo test"]);

        let search = parse(&[
            "--source",
            "discord,cron",
            "--source",
            "cron",
            "--session",
            "2026",
            "--",
            "--all",
        ])
        .unwrap();
        assert_eq!(search.sources, ["discord", "cron"]);
        assert_eq!(search.query, "--all");
        assert!(!search.all);
        let options = search
            .options(Path::new("/w"), Duration::from_secs(1))
            .unwrap();
        // A named session is searched wherever it ran.
        assert_eq!(options.folder, None);
        assert_eq!(options.session.as_deref(), Some("2026"));

        let options = parse(&["x"])
            .unwrap()
            .options(Path::new("/w"), Duration::from_secs(1))
            .unwrap();
        assert_eq!(options.folder.as_deref(), Some("/w"));
        assert_eq!(options.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn mistakes_are_explained() {
        for (tokens, expected) in [
            (&[][..], "give the words"),
            (&["--all"], "give the words"),
            (&["--limit"], "--limit needs a number"),
            (&["--limit", "0", "x"], "from 1 to 20"),
            (&["--limit", "21", "x"], "from 1 to 20"),
            (
                &["--source", "slack", "x"],
                "unknown session source `slack`",
            ),
            (&["--sources", "x"], "unknown option `--sources`"),
            (&["--session"], "--session needs a session id"),
        ] {
            let err = parse(tokens).unwrap_err();
            assert!(err.contains(expected), "{tokens:?}: {err}");
        }
    }

    /// Words inside double quotes stay words even when they look like
    /// options, as `/search` splits them (PR #49 review, item 3).
    #[test]
    fn quoted_words_are_not_options() {
        let line = |text: &str| HumanSearch::parse(text.split_whitespace().map(str::to_string));
        let search = line("\"run --all tests\"").unwrap();
        assert!(!search.all);
        let options = search
            .options(Path::new("/w"), Duration::from_secs(1))
            .unwrap();
        assert_eq!(options.query.terms(), ["run --all tests"]);
        assert_eq!(options.folder.as_deref(), Some("/w"));

        let search = line("\"fix --limit bug\"").unwrap();
        assert_eq!(search.limit, DEFAULT_LIMIT);
        assert_eq!(search.query, "\"fix --limit bug\"");

        // Options outside the quotes still count; an unclosed quote runs
        // to the end.
        let search = line("--limit 2 \"a --session b\" --all c \"open --source x").unwrap();
        assert_eq!((search.limit, search.all, search.session), (2, true, None));
        assert_eq!(search.sources, SOURCES);
        assert_eq!(search.query, "\"a --session b\" c \"open --source x");
    }

    #[test]
    fn quotes_shell_arguments_with_spaces() {
        assert_eq!(quote_arg("빌드 실패".into()), "\"빌드 실패\"");
        assert_eq!(quote_arg("one".into()), "one");
        assert_eq!(quote_arg("\"a b\" c".into()), "\"a b\" c");
    }
}
