//! `grep`: searches file contents with a regular expression.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use super::{Ended, Search, Stop, Walk, bounded, opt_bool, opt_str, shown};
use crate::tools::{Tool, ToolOutput};

/// Files bigger than this are skipped.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Default caps: files listed (`files`, `count`), lines shown (`content`,
/// context lines included), characters of one line, characters of the
/// whole output.
pub const MAX_FILES: usize = 200;
pub const MAX_LINES: usize = 400;
pub const MAX_LINE_CHARS: usize = 500;
pub const MAX_OUTPUT_CHARS: usize = 30_000;
/// The most `max_results` may ask for.
const MAX_RESULTS: u64 = 1_000;
/// The longest pattern taken.
const MAX_PATTERN_CHARS: usize = 1_000;
/// The compiled pattern's size cap: a regex without backtracking runs in
/// time linear to the text, but a huge repetition still compiles big.
const REGEX_SIZE_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    Files,
    Content,
    Count,
}

/// Searches file contents with a regular expression.
#[derive(Debug, Clone)]
pub struct Grep {
    search: Search,
}

impl Grep {
    pub fn new(search: Search) -> Self {
        Self { search }
    }
}

/// One call's arguments, checked.
struct Query {
    regex: Regex,
    path: Option<String>,
    glob: Option<globset::GlobMatcher>,
    /// The glob matches file names only (it has no `/`).
    glob_name_only: bool,
    types: Option<ignore::types::Types>,
    output: Output,
    context: usize,
    max_results: Option<usize>,
    multiline: bool,
    hidden: bool,
}

impl Query {
    fn parse(input: &Value) -> Result<Self, String> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or("missing required string argument `pattern`")?;
        if pattern.chars().count() > MAX_PATTERN_CHARS {
            return Err(format!(
                "the pattern is longer than {MAX_PATTERN_CHARS} characters"
            ));
        }
        let case_insensitive = opt_bool(input, "case_insensitive")?;
        let multiline = opt_bool(input, "multiline")?;
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .dot_matches_new_line(multiline)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|err| format!("invalid regular expression: {err}"))?;
        let output = match opt_str(input, "output")? {
            None | Some("files") => Output::Files,
            Some("content") => Output::Content,
            Some("count") => Output::Count,
            Some(other) => {
                return Err(format!(
                    "`output` is `files`, `content` or `count`, not `{other}`"
                ));
            }
        };
        let (glob, glob_name_only) = match opt_str(input, "glob")? {
            None => (None, false),
            Some(glob) => {
                let matcher = globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .map_err(|err| format!("invalid glob `{glob}`: {err}"))?
                    .compile_matcher();
                (Some(matcher), !glob.contains('/'))
            }
        };
        let types = match opt_str(input, "type")? {
            None => None,
            Some(kind) => {
                let mut builder = ignore::types::TypesBuilder::new();
                builder.add_defaults();
                builder.select(kind);
                Some(builder.build().map_err(|_| {
                    format!("unknown file type `{kind}` (for example rust, py, js, ts, go, md)")
                })?)
            }
        };
        let max_results = match input.get("max_results") {
            None | Some(Value::Null) => None,
            Some(_) => Some(bounded(input, "max_results", 1..=MAX_RESULTS, 1)? as usize),
        };
        Ok(Self {
            regex,
            path: opt_str(input, "path")?.map(str::to_string),
            glob,
            glob_name_only,
            types,
            output,
            context: bounded(input, "context", 0..=10, 0)? as usize,
            max_results,
            multiline,
            hidden: opt_bool(input, "hidden")?,
        })
    }

    /// Whether the `glob` and `type` filters keep the file at `path`
    /// (`relative` to the start).
    fn keeps(&self, path: &Path, relative: &Path) -> bool {
        if let Some(glob) = &self.glob {
            let matched = if self.glob_name_only {
                path.file_name().is_some_and(|name| glob.is_match(name))
            } else {
                glob.is_match(relative)
            };
            if !matched {
                return false;
            }
        }
        match &self.types {
            Some(types) => types.matched(path, false).is_whitelist(),
            None => true,
        }
    }

    /// The lines (0-based) the pattern matches in `text`, in order: every
    /// line a match spans with `multiline`, else the lines holding one.
    fn matched_lines(&self, text: &str, starts: &[usize]) -> Vec<usize> {
        let mut lines = Vec::new();
        if self.multiline {
            for found in self.regex.find_iter(text) {
                let first = line_of(starts, found.start());
                let last = line_of(starts, found.end().saturating_sub(1).max(found.start()));
                for line in first..=last {
                    if lines.last() != Some(&line) {
                        lines.push(line);
                    }
                }
            }
        } else {
            for (index, line) in lines_of(text, starts).enumerate() {
                if self.regex.is_match(line) {
                    lines.push(index);
                }
            }
        }
        lines
    }
}

/// The byte offsets where the lines of `text` start.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .filter(|&at| at < text.len() || at == 0)
        .collect()
}

/// The line (0-based) byte `at` is on.
fn line_of(starts: &[usize], at: usize) -> usize {
    starts
        .partition_point(|&start| start <= at)
        .saturating_sub(1)
}

/// The lines of `text`, without their line ends.
fn lines_of<'a>(text: &'a str, starts: &'a [usize]) -> impl Iterator<Item = &'a str> + 'a {
    starts.iter().enumerate().map(move |(index, &start)| {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        text[start..end].trim_end_matches(['\n', '\r'])
    })
}

/// A file with matches.
struct Found {
    path: PathBuf,
    modified: SystemTime,
    /// Matching lines.
    count: usize,
}

/// What the walk passed over.
#[derive(Default)]
struct Skipped {
    large: usize,
    binary: usize,
    unreadable: usize,
}

/// The text of the file at `path`, read through the file rules; `Err`
/// says why it was skipped.
fn read_text(search: &Search, path: &Path, skipped: &mut Skipped) -> Option<String> {
    let Some(name) = path.to_str() else {
        skipped.unreadable += 1;
        return None;
    };
    let Ok(opened) = search.access.open_read(name) else {
        skipped.unreadable += 1;
        return None;
    };
    if opened.meta.len() > MAX_FILE_BYTES {
        skipped.large += 1;
        return None;
    }
    let mut bytes = Vec::new();
    if opened
        .file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        skipped.unreadable += 1;
        return None;
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        skipped.large += 1;
        return None;
    }
    if bytes.contains(&0) {
        skipped.binary += 1;
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The search itself, on a blocking thread.
fn search(search: &Search, query: &Query, stop: &Stop) -> Result<String, String> {
    let start = search.start(query.path.as_deref())?;
    let mut found: Vec<Found> = Vec::new();
    let mut skipped = Skipped::default();
    let mut look = |path: &Path, relative: &Path, modified: SystemTime, skipped: &mut Skipped| {
        if !query.keeps(path, relative) {
            return;
        }
        let Some(text) = read_text(search, path, skipped) else {
            return;
        };
        let starts = line_starts(&text);
        let count = query.matched_lines(&text, &starts).len();
        if count > 0 {
            found.push(Found {
                path: path.to_path_buf(),
                modified,
                count,
            });
        }
    };
    let ended = if start.is_dir {
        search.walk(
            &start,
            Walk {
                hidden: query.hidden,
                depth: None,
            },
            stop,
            |entry| {
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    let relative = entry
                        .path()
                        .strip_prefix(&start.root)
                        .unwrap_or(entry.path());
                    look(entry.path(), relative, super::modified(entry), &mut skipped);
                }
                true
            },
        )
    } else {
        let modified = std::fs::metadata(&start.root)
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let name = start
            .root
            .file_name()
            .map(Path::new)
            .unwrap_or(Path::new(""));
        look(&start.root, name, modified, &mut skipped);
        stop.ended().unwrap_or(Ended::Done)
    };
    // Newest first, then by path.
    found.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut out = match query.output {
        Output::Files => list(&found, query, |file| shown(&file.path)),
        Output::Count => list(&found, query, |file| {
            format!("{}:{}", shown(&file.path), file.count)
        }),
        Output::Content => content(search, &found, query, stop, &mut skipped),
    };
    if found.is_empty() {
        out = format!(
            "No matches for `{}` in {}.\n",
            query.regex.as_str(),
            shown(&start.root)
        );
    }
    let mut notes = Vec::new();
    if skipped.large + skipped.binary > 0 {
        notes.push(format!(
            "[skipped {} file(s) over 1 MB and {} binary file(s)]",
            skipped.large, skipped.binary
        ));
    }
    if let Some(note) = ended.note(search.time_limit) {
        notes.push(note);
    }
    for note in notes {
        out.push_str(&note);
        out.push('\n');
    }
    Ok(out)
}

/// One line per file, at most the cap, then how many more there are.
fn list(found: &[Found], query: &Query, line: impl Fn(&Found) -> String) -> String {
    let cap = query.max_results.unwrap_or(MAX_FILES);
    let mut out = String::new();
    let mut shown_files = 0;
    for file in found.iter().take(cap) {
        let line = line(file);
        if out.chars().count() + line.chars().count() + 1 > MAX_OUTPUT_CHARS {
            break;
        }
        out.push_str(&line);
        out.push('\n');
        shown_files += 1;
    }
    if found.len() > shown_files {
        let _ = writeln!(
            out,
            "[{} more file(s) not shown; narrow the pattern, path, glob or type]",
            found.len() - shown_files
        );
    }
    out
}

/// `path:line:text` for matching lines and `path-line-text` for the lines
/// around them, `--` between groups, secrets hidden, within the caps.
fn content(
    search: &Search,
    found: &[Found],
    query: &Query,
    stop: &Stop,
    skipped: &mut Skipped,
) -> String {
    let cap = query.max_results.unwrap_or(MAX_LINES);
    let total: usize = found.iter().map(|file| file.count).sum();
    let mut out = String::new();
    let mut chars = 0;
    let mut lines_out = 0;
    let mut matches_out = 0;
    let mut full = false;
    for file in found {
        if full || stop.cancelled() {
            break;
        }
        // Read again: only the files shown are kept in full.
        let Some(text) = read_text(search, &file.path, skipped) else {
            continue;
        };
        let starts = line_starts(&text);
        let lines: Vec<&str> = lines_of(&text, &starts).collect();
        let matched = query.matched_lines(&text, &starts);
        let name = shown(&file.path);
        // The last line shown of this file.
        let mut last: Option<usize> = None;
        for &line in &matched {
            let to = (line + query.context).min(lines.len().saturating_sub(1));
            let from = match last {
                Some(last) => line.saturating_sub(query.context).max(last + 1),
                None => line.saturating_sub(query.context),
            };
            if from > to {
                continue;
            }
            // With context, `--` between groups that do not touch, as rg.
            let apart = last.is_none_or(|last| from > last + 1);
            if query.context > 0 && lines_out > 0 && apart {
                out.push_str("--\n");
            }
            for (index, line) in lines.iter().enumerate().take(to + 1).skip(from) {
                let is_match = matched.binary_search(&index).is_ok();
                let mut text = cut_line(line);
                crate::doctor::hide_credentials(std::iter::once(&mut text));
                let mark = if is_match { ':' } else { '-' };
                let row = format!("{name}{mark}{}{mark}{text}\n", index + 1);
                let row_chars = row.chars().count();
                if lines_out >= cap || chars + row_chars > MAX_OUTPUT_CHARS {
                    full = true;
                    break;
                }
                out.push_str(&row);
                chars += row_chars;
                lines_out += 1;
                if is_match {
                    matches_out += 1;
                }
                last = Some(index);
            }
            if full {
                break;
            }
        }
    }
    if total > matches_out {
        let _ = writeln!(
            out,
            "[{} more matching line(s) not shown; narrow the search or use output \"files\" \
             or \"count\"]",
            total - matches_out
        );
    }
    out
}

/// `line` cut to [`MAX_LINE_CHARS`] characters, saying how much was left.
fn cut_line(line: &str) -> String {
    let count = line.chars().count();
    if count <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let end = line
        .char_indices()
        .nth(MAX_LINE_CHARS)
        .map_or(line.len(), |(at, _)| at);
    format!(
        "{} [... {} more characters]",
        &line[..end],
        count - MAX_LINE_CHARS
    )
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression (Rust regex syntax, like ripgrep). \
         Follows .gitignore; skips .git, target, node_modules, dot files (unless hidden), \
         binary files and files over 1 MB. output \"files\" (default) lists matching files \
         newest first; \"content\" shows `path:line:text` with optional context lines; \
         \"count\" gives matching lines per file. Use this instead of grep or rg in bash."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression to search for"},
                "path": {"type": "string", "description": "File or folder to search (default: the working folder)"},
                "glob": {"type": "string", "description": "Only files matching this glob, e.g. `*.rs` (file name) or `src/**/*.ts` (path)"},
                "type": {"type": "string", "description": "Only files of this type, e.g. rust, py, js, ts, go, md"},
                "case_insensitive": {"type": "boolean"},
                "output": {"type": "string", "enum": ["files", "content", "count"]},
                "context": {"type": "integer", "minimum": 0, "maximum": 10, "description": "Lines before and after each match (content)"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "Most files (files, count) or lines (content) to return"},
                "multiline": {"type": "boolean", "description": "Let the pattern span lines (`.` matches newlines)"},
                "hidden": {"type": "boolean", "description": "Also search dot files and folders"}
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let query = match Query::parse(&input) {
            Ok(query) => query,
            Err(err) => return ToolOutput::error(err),
        };
        let search_rules = self.search.clone();
        let limit = self.search.time_limit;
        match super::run_blocking(limit, move |stop| search(&search_rules, &query, stop)).await {
            Ok(Ok(out)) => ToolOutput::ok(out),
            Ok(Err(err)) | Err(err) => ToolOutput::error(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tools::file_access::FileAccess;

    #[test]
    fn lines_found_before_the_time_limit_are_still_shown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\nhit\n").unwrap();
        let rules = Search::with_access(FileAccess::default(), Duration::ZERO);
        let query = Query::parse(&json!({"pattern": "hit", "output": "content"})).unwrap();
        let found = vec![Found {
            path: path.clone(),
            modified: SystemTime::now(),
            count: 1,
        }];
        // The deadline has passed: the walk stopped, its results stand.
        let stop = Stop::new(Duration::ZERO);
        assert!(stop.ended().is_some());
        let out = content(&rules, &found, &query, &stop, &mut Skipped::default());
        assert_eq!(out, format!("{}:2:hit\n", path.display()));
        // A dropped call shows nothing more.
        drop(stop.guard());
        let out = content(&rules, &found, &query, &stop, &mut Skipped::default());
        assert!(
            out.starts_with("[1 more matching line(s) not shown"),
            "{out}"
        );
    }
}
