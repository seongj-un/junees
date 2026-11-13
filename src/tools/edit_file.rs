use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::file_access::FileAccess;
use super::{
    MAX_FILE_BYTES, Tool, ToolOutput, ToolSettings, opt_bool_arg, str_arg, truncate_middle,
};

/// Lines of context shown above and below each change.
const CONTEXT_LINES: usize = 3;
/// Changes shown in the result; later ones are only counted.
const MAX_SHOWN_CHANGES: usize = 3;
/// Match positions listed when `old_string` is not unique.
const MAX_LISTED_MATCHES: usize = 20;
/// Similar lines suggested when `old_string` is not found.
const MAX_HINTS: usize = 3;
/// Characters of one line shown in a hint or a preview.
const MAX_LINE_CHARS: usize = 300;
const MAX_OUTPUT_CHARS: usize = 20_000;

/// Replaces an exact string in a file, so a small change does not need the
/// whole file written out again.
#[derive(Default)]
pub struct EditFile {
    access: FileAccess,
    /// The instruction file names it refuses ([`ToolSettings::protected_instruction_files`]).
    instruction_files: Vec<String>,
}

impl EditFile {
    /// Refuses the settings file of `settings`, in a webhook run the
    /// project instruction files, and in an unattended run paths outside
    /// its folders ([`FileAccess`]).
    pub fn with_settings(settings: &ToolSettings) -> Self {
        Self {
            access: FileAccess::new(settings),
            instruction_files: settings.protected_instruction_files(),
        }
    }

    /// A tool with the rules of `access` (tests).
    #[cfg(test)]
    pub(crate) fn with_access(access: FileAccess) -> Self {
        Self {
            access,
            ..Self::default()
        }
    }
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string. old_string must match the file \
         exactly (whitespace and indentation included, without read_file's line \
         numbers) and occur exactly once, unless replace_all is true. Include enough \
         surrounding lines to make it unique. Line endings and the final newline are \
         kept. Use write_file for new files or full rewrites."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                "old_string": {"type": "string", "description": "The exact text to replace"},
                "new_string": {"type": "string", "description": "The text to put in its place"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of exactly one (overlapping ones left to right, skipping those that overlap a replaced one; default false)"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn run(&self, input: Value) -> ToolOutput {
        let path = match str_arg(&input, "path") {
            Ok(path) => path,
            Err(err) => return err,
        };
        let old = match str_arg(&input, "old_string") {
            Ok(old) => old,
            Err(err) => return err,
        };
        let new = match str_arg(&input, "new_string") {
            Ok(new) => new,
            Err(err) => return err,
        };
        let replace_all = match opt_bool_arg(&input, "replace_all") {
            Ok(replace_all) => replace_all.unwrap_or(false),
            Err(err) => return err,
        };
        if let Some(file) = self.access.settings_file()
            && file.is(Path::new(path))
        {
            return ToolOutput::error(file.refusal(path));
        }
        if let Some(folder) = self
            .access
            .settings_file()
            .and_then(|file| file.data_folder(Path::new(path)))
        {
            return ToolOutput::error(folder.refusal(path));
        }
        if let Some(name) =
            super::instruction_files::path_named(Path::new(path), &self.instruction_files)
        {
            return ToolOutput::error(super::instruction_files::refusal(path, name));
        }
        let access = self.access.clone();
        let names = self.instruction_files.clone();
        let (owned, old, new) = (path.to_string(), old.to_string(), new.to_string());
        // Task-local: taken here, not on the blocking thread.
        let turn = crate::checkpoints::current();
        // One blocking call from the checks to the rename: dropping this
        // future (a cancelled turn) cannot stop it between creating the
        // temporary file and renaming or removing it.
        let edited = tokio::task::spawn_blocking(move || {
            edit(
                &access,
                &names,
                &owned,
                &old,
                &new,
                replace_all,
                turn.as_deref(),
            )
        })
        .await;
        match edited {
            Ok(Ok(output) | Err(output)) => output,
            Err(err) => ToolOutput::error(format!("cannot edit {path}: {err}")),
        }
    }
}

/// Makes the edit: the file found through its folder, opened and checked
/// by `access` (a link leads to its target, which is replaced and the link
/// kept), and the result written to a temporary file renamed over it.
/// With `turn`, the content before is recorded first, and the hash after.
fn edit(
    access: &FileAccess,
    names: &[String],
    path: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    turn: Option<&crate::checkpoints::Turn>,
) -> Result<ToolOutput, ToolOutput> {
    if old.is_empty() {
        return Err(ToolOutput::error(
            "old_string is empty; use write_file to create or overwrite a file",
        ));
    }
    if old == new {
        return Err(ToolOutput::error(
            "old_string and new_string are the same; nothing to change",
        ));
    }

    let refused =
        |refusal: super::file_access::Refusal| ToolOutput::error(refusal.message(path, "edit"));
    let target = access.open_write(path, false).map_err(refused)?;
    if let Some(name) = super::instruction_files::path_named(target.real(), names) {
        return Err(ToolOutput::error(super::instruction_files::refusal(
            path, name,
        )));
    }
    let opened = target.open_existing().map_err(refused)?;
    let too_large = |len: u64| {
        ToolOutput::error(format!(
            "{path} is {len} bytes; edit_file edits files up to {} MB. \
             Use bash (sed, a script) to change it.",
            MAX_FILE_BYTES / (1024 * 1024)
        ))
    };
    if opened.meta.len() > MAX_FILE_BYTES {
        return Err(too_large(opened.meta.len()));
    }
    let mut bytes = Vec::new();
    opened
        .file
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| ToolOutput::error(format!("cannot read {path}: {err}")))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    // Copied only when a turn records it.
    let before = turn.map(|_| crate::checkpoints::Before::from_content(&bytes, &opened.meta));
    // Before any hint shows lines of it: such files are not text
    // (`/proc/<pid>/environ` is NUL-separated).
    if bytes.contains(&0) {
        return Err(ToolOutput::error(format!(
            "{path} looks like a binary file (it holds NUL bytes); edit_file edits text files"
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolOutput::error(format!("{path} is not valid UTF-8 text")))?;

    let edit = plan_edit(&text, old, new, replace_all).map_err(ToolOutput::error)?;
    if let (Some(turn), Some(before)) = (turn, before) {
        turn.before(target.real(), before);
    }
    let written = target.replace(edit.text.as_bytes(), false);
    if let Some(turn) = turn {
        match &written {
            Ok(()) => turn.after(target.real(), Some(edit.text.as_bytes())),
            Err(_) => turn.after_disk(target.real()),
        }
    }
    written.map_err(|err| ToolOutput::error(format!("cannot write {path}: {err}")))?;
    Ok(ToolOutput::ok(edit.summary(path)))
}

/// The edited text and where its changes are.
#[derive(Debug)]
struct Edit {
    text: String,
    /// Byte ranges of the inserted text in `text`.
    changes: Vec<(usize, usize)>,
    trailing_newline_kept: bool,
}

impl Edit {
    fn summary(&self, path: &str) -> String {
        let count = self.changes.len();
        let mut out = format!(
            "edited {path}: replaced {count} occurrence{}",
            if count == 1 { "" } else { "s" }
        );
        if self.trailing_newline_kept {
            out.push_str(" (kept the final newline)");
        }
        out.push('\n');
        out.push_str(&preview(&self.text, &self.changes));
        truncate_middle(&out, MAX_OUTPUT_CHARS)
    }
}

/// Works out the edit, or why it cannot be made.
fn plan_edit(text: &str, old: &str, new: &str, replace_all: bool) -> Result<Edit, String> {
    let crlf = uses_crlf(text);
    // read_file shows lines without `\r`, so where the file has `\r\n` the
    // model's `\n` usually stands for it, also in the few CRLF lines of a
    // mostly LF file. An exact match still wins.
    let old = if find_all(text, old).is_empty() && has_bare_lf(old) {
        to_crlf(old)
    } else {
        old.to_string()
    };
    let new_crlf = to_crlf(new);

    let starts = find_all(text, &old);
    if starts.is_empty() {
        return Err(not_found(text, &old));
    }
    if starts.len() > 1 && !replace_all {
        let lines: Vec<String> = starts
            .iter()
            .take(MAX_LISTED_MATCHES)
            .map(|&start| line_of(text, start).to_string())
            .collect();
        let more = if starts.len() > MAX_LISTED_MATCHES {
            ", ..."
        } else {
            ""
        };
        // replace_all replaces matches left to right, so of overlapping ones
        // only those that do not overlap an earlier replaced one.
        let disjoint = text.matches(old.as_str()).count();
        let which = if disjoint == starts.len() {
            "every one".to_string()
        } else {
            format!("{disjoint} of them that do not overlap, from the start")
        };
        return Err(format!(
            "old_string occurs {} times (at lines {}{more}). Include more surrounding \
             lines to make it unique, or set replace_all to true to replace {which}.",
            starts.len(),
            lines.join(", ")
        ));
    }

    let mut out = String::with_capacity(text.len());
    let mut changes = Vec::new();
    let mut last = 0;
    for (mut start, _) in text.match_indices(old.as_str()) {
        let end = start + old.len();
        let new = if crlf_match(text, start, end, crlf) {
            // A match starting at the `\n` of a `\r\n` takes the `\r` too:
            // that line break is what the model's `\n` stands for.
            if old.starts_with('\n') && start > last && text.as_bytes()[start - 1] == b'\r' {
                start -= 1;
            }
            new_crlf.as_str()
        } else {
            new
        };
        out.push_str(&text[last..start]);
        changes.push((out.len(), out.len() + new.len()));
        out.push_str(new);
        last = end;
    }
    out.push_str(&text[last..]);

    let mut trailing_newline_kept = false;
    if text.ends_with('\n') && !out.is_empty() && !out.ends_with('\n') {
        out.push_str(if crlf { "\r\n" } else { "\n" });
        trailing_newline_kept = true;
    }
    Ok(Edit {
        text: out,
        changes,
        trailing_newline_kept,
    })
}

/// Start offsets of every occurrence of `needle`, overlapping ones included:
/// `aa` occurs twice in `aaa`, so replacing "the" one would be a guess.
fn find_all(text: &str, needle: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(needle) {
        let start = from + found;
        starts.push(start);
        let step = text[start..].chars().next().map_or(1, char::len_utf8);
        from = start + step;
    }
    starts
}

/// Whether most line breaks in `text` are `\r\n`.
fn uses_crlf(text: &str) -> bool {
    let crlf = text.matches("\r\n").count();
    crlf > 0 && crlf * 2 >= text.matches('\n').count()
}

/// Whether the text replacing `text[start..end]` gets `\r\n` line breaks:
/// when every line break in the match is a `\r\n` in the file (a match can
/// start at the `\n` of a `\r\n`), or, for a match without line breaks,
/// when most of the file's line breaks are.
fn crlf_match(text: &str, start: usize, end: usize, crlf_file: bool) -> bool {
    let bytes = text.as_bytes();
    let mut breaks = (start..end).filter(|&i| bytes[i] == b'\n').peekable();
    if breaks.peek().is_none() {
        return crlf_file;
    }
    breaks.all(|i| i > 0 && bytes[i - 1] == b'\r')
}

fn has_bare_lf(text: &str) -> bool {
    text.match_indices('\n')
        .any(|(i, _)| i == 0 || text.as_bytes()[i - 1] != b'\r')
}

/// Turns every `\n` not already preceded by `\r` into `\r\n`.
fn to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev = None;
    for ch in text.chars() {
        if ch == '\n' && prev != Some('\r') {
            out.push('\r');
        }
        out.push(ch);
        prev = Some(ch);
    }
    out
}

/// 1-based line number of the byte offset `at`.
fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

fn cut_line(line: &str) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => format!("{} [...]", &line[..end]),
        None => line.to_string(),
    }
}

/// The not-found error, with the lines of the file most like the first
/// non-blank line of `old_string` as a hint.
fn not_found(text: &str, old: &str) -> String {
    let mut out = String::from(
        "old_string was not found in the file. It must match exactly, \
         whitespace and indentation included.",
    );
    if text.contains("\r\n") && has_bare_lf(text) {
        out.push_str(
            " Some lines in the file end in \\r\\n and others in \\n (read_file \
             shows neither); old_string must spell out each one it spans.",
        );
    }
    let Some(needle) = old.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return out;
    };
    let wanted = bigrams(needle);
    let mut scored: Vec<(f64, usize, &str)> = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let score = if trimmed == needle {
                1.0
            } else {
                dice(&wanted, &bigrams(trimmed))
            };
            (score >= 0.5).then_some((score, index + 1, line))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(MAX_HINTS);
    if scored.is_empty() {
        return out;
    }
    out.push_str(" Most similar lines:\n");
    for (_, number, line) in scored {
        let _ = writeln!(out, "{number:>6}\t{}", cut_line(line));
    }
    out
}

fn bigrams(text: &str) -> HashMap<(char, char), usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut counts = HashMap::new();
    if chars.len() == 1 {
        *counts.entry((chars[0], '\0')).or_insert(0) += 1;
    }
    for pair in chars.windows(2) {
        *counts.entry((pair[0], pair[1])).or_insert(0) += 1;
    }
    counts
}

/// Sørensen-Dice similarity of two bigram multisets, from 0 to 1.
fn dice(a: &HashMap<(char, char), usize>, b: &HashMap<(char, char), usize>) -> f64 {
    let total: usize = a.values().sum::<usize>() + b.values().sum::<usize>();
    if total == 0 {
        return 0.0;
    }
    let shared: usize = a
        .iter()
        .map(|(pair, count)| (*count).min(b.get(pair).copied().unwrap_or(0)))
        .sum();
    2.0 * shared as f64 / total as f64
}

/// The changed lines with a few lines around them, numbered like read_file.
fn preview(text: &str, changes: &[(usize, usize)]) -> String {
    if text.is_empty() {
        return "[the file is now empty]\n".to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    // A final newline does not start another line.
    let total = if text.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    };

    // 0-based inclusive line windows, merged when they touch.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for &(start, end) in changes.iter().take(MAX_SHOWN_CHANGES) {
        let first = (line_of(text, start) - 1).min(total - 1);
        // Inserted text ends on the line holding its last byte; a deletion
        // shows the line it happened on.
        let last_byte = if end > start { end - 1 } else { start };
        let last = (line_of(text, last_byte) - 1).clamp(first, total - 1);
        let from = first.saturating_sub(CONTEXT_LINES);
        let to = (last + CONTEXT_LINES).min(total - 1);
        match windows.last_mut() {
            Some(window) if from <= window.1 + 1 => window.1 = window.1.max(to),
            _ => windows.push((from, to)),
        }
    }

    let mut out = String::new();
    for (i, &(from, to)) in windows.iter().enumerate() {
        if i > 0 {
            out.push_str("   ...\n");
        }
        for (index, line) in lines.iter().enumerate().take(to + 1).skip(from) {
            let _ = writeln!(out, "{:>6}\t{}", index + 1, cut_line(line));
        }
    }
    if changes.len() > MAX_SHOWN_CHANGES {
        let _ = writeln!(
            out,
            "[{} more changes not shown]",
            changes.len() - MAX_SHOWN_CHANGES
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    fn temp_file(dir: &tempfile::TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("file.txt");
        std::fs::write(&path, content).unwrap();
        path
    }

    async fn run(path: &Path, old: &str, new: &str) -> ToolOutput {
        EditFile::default()
            .run(json!({"path": path, "old_string": old, "new_string": new}))
            .await
    }

    #[tokio::test]
    async fn refuses_the_settings_file() {
        let (_dir, root, file) = crate::tools::settings_file::testing::home();
        let config = root.join(".junees/config.toml");
        std::os::unix::fs::symlink(&config, root.join("link")).unwrap();
        let tool = EditFile::with_settings(&ToolSettings {
            settings_file: Some(file),
            ..ToolSettings::default()
        });
        for path in [
            config.clone(),
            root.join(".junees/sessions/../config.toml"),
            root.join("link"),
            crate::tools::settings_file::testing::relative(&config),
        ] {
            let output = tool
                .run(json!({"path": path, "old_string": "secret-key", "new_string": "x"}))
                .await;
            assert!(output.is_error, "{}: {}", path.display(), output.content);
            assert!(output.content.contains("private Junees settings file"));
            assert!(!output.content.contains("secret-key"));
        }
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "api_key = \"secret-key\"\n"
        );
        let session = root.join(".junees/sessions/s.jsonl");
        let output = tool
            .run(json!({"path": session, "old_string": "{}", "new_string": "[]"}))
            .await;
        assert!(!output.is_error, "{}", output.content);
    }

    #[tokio::test]
    async fn refuses_junees_data_folders() {
        let (_dir, root, file) = crate::tools::settings_file::testing::home();
        let user = root.join(".junees/memory/user.md");
        std::os::unix::fs::symlink(root.join(".junees/memory"), root.join("dirlink")).unwrap();
        let tool = EditFile::with_settings(&ToolSettings {
            settings_file: Some(file),
            ..ToolSettings::default()
        });
        for path in [
            user.clone(),
            root.join("dirlink/user.md"),
            crate::tools::settings_file::testing::relative(&user),
        ] {
            let output = tool
                .run(json!({"path": path, "old_string": "J", "new_string": "K"}))
                .await;
            assert!(output.is_error, "{}: {}", path.display(), output.content);
            assert!(
                output.content.contains("use the `memory` tool instead"),
                "{}",
                output.content
            );
        }
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "Name: J\n");
    }

    fn entries(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn replaces_a_unique_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "fn main() {\n    old();\n}\n");
        let output = run(&path, "old();", "new();").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        // No temporary file is left behind.
        assert_eq!(entries(&dir), ["file.txt"]);
    }

    #[tokio::test]
    async fn not_found_suggests_similar_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(
            &dir,
            "alpha\nlet total = count + 1;\nbeta\n    let totals = count + 2;\n",
        );
        let output = run(&path, "  let total = count + 1;\nbeta", "x").await;
        assert!(output.is_error);
        assert_eq!(
            output.content,
            "old_string was not found in the file. It must match exactly, \
             whitespace and indentation included. Most similar lines:\n\
             \x20    2\tlet total = count + 1;\n\
             \x20    4\t    let totals = count + 2;\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "alpha\nlet total = count + 1;\nbeta\n    let totals = count + 2;\n"
        );
    }

    #[tokio::test]
    async fn not_found_without_similar_lines_has_no_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "alpha\nbeta\n");
        let output = run(&path, "zzz", "x").await;
        assert!(output.is_error);
        assert!(output.content.ends_with("indentation included."));
    }

    #[tokio::test]
    async fn several_matches_are_listed_and_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "x = 1\ny = 2\nx = 1\n\nx = 1\n");
        let output = run(&path, "x = 1", "x = 9").await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .starts_with("old_string occurs 3 times (at lines 1, 3, 5)."),
            "{}",
            output.content
        );
        assert!(output.content.contains("replace_all"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "x = 1\ny = 2\nx = 1\n\nx = 1\n"
        );
    }

    #[tokio::test]
    async fn overlapping_matches_are_not_unique() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "aaa\n");
        let output = run(&path, "aa", "b").await;
        assert!(output.is_error);
        assert!(output.content.starts_with("old_string occurs 2 times"));
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "x = 1\ny = 2\nx = 1\n\nx = 1\n");
        let output = EditFile::default()
            .run(json!({"path": path, "old_string": "x = 1", "new_string": "x = 9", "replace_all": true}))
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.starts_with("edited "));
        assert!(output.content.contains(": replaced 3 occurrences\n"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "x = 9\ny = 2\nx = 9\n\nx = 9\n"
        );
    }

    #[tokio::test]
    async fn replace_all_must_be_a_boolean() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "a\n");
        let output = EditFile::default()
            .run(json!({"path": path, "old_string": "a", "new_string": "b", "replace_all": "yes"}))
            .await;
        assert_eq!(output, ToolOutput::error("`replace_all` must be a boolean"));
    }

    #[tokio::test]
    async fn rejects_empty_and_unchanged_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "a\n");
        let output = run(&path, "", "b").await;
        assert!(output.is_error);
        assert!(output.content.starts_with("old_string is empty"));
        let output = run(&path, "a", "a").await;
        assert!(output.is_error);
        assert!(output.content.contains("are the same"));
        assert!(run(&path, "a", "").await.content.starts_with("edited"));
        let output = EditFile::default()
            .run(json!({"path": path, "old_string": "a"}))
            .await;
        assert!(output.content.contains("`new_string`"));
    }

    #[tokio::test]
    async fn keeps_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "one\r\ntwo\r\nthree\r\n");
        // The model writes `\n`, as read_file shows the lines.
        let output = run(&path, "one\ntwo\n", "one\n2a\n2b\n").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "one\r\n2a\r\n2b\r\nthree\r\n"
        );
        // A one-line match with new lines in the replacement.
        let output = run(&path, "three", "3a\n3b").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "one\r\n2a\r\n2b\r\n3a\r\n3b\r\n"
        );
        // The preview shows lines without `\r`.
        assert!(!output.content.contains('\r'), "{:?}", output.content);
    }

    #[tokio::test]
    async fn exact_lf_matches_in_crlf_files_win() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "a\r\nb\r\nc\r\nx\ny\n");
        let output = run(&path, "x\ny", "x\nz").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\r\nb\r\nc\r\nx\nz\n"
        );
    }

    #[tokio::test]
    async fn keeps_the_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "a\nlast\n");
        let output = run(&path, "last\n", "end").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("(kept the final newline)"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nend\n");

        let path = dir.path().join("crlf.txt");
        std::fs::write(&path, "a\r\nlast\r\n").unwrap();
        run(&path, "last\n", "end").await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nend\r\n");

        // A file without one does not get one.
        let path = dir.path().join("bare.txt");
        std::fs::write(&path, "a\nlast").unwrap();
        let output = run(&path, "last", "end").await;
        assert!(!output.content.contains("kept the final newline"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nend");
    }

    #[tokio::test]
    async fn edits_hangul() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "첫째 줄\n안녕 세계\n셋째 줄\n");
        let output = run(&path, "안녕", "반가워").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "첫째 줄\n반가워 세계\n셋째 줄\n"
        );
        assert!(output.content.contains("     2\t반가워 세계\n"));

        // Overlapping Hangul matches step over whole characters.
        let path = temp_file(&dir, "가가가\n");
        let output = run(&path, "가가", "나").await;
        assert!(output.content.starts_with("old_string occurs 2 times"));
        // Hints and previews cut long Hangul lines on char boundaries.
        let long = "한".repeat(MAX_LINE_CHARS + 10);
        let path = temp_file(&dir, &format!("{long}\n"));
        let output = run(
            &path,
            &format!("{}글", "한".repeat(MAX_LINE_CHARS + 5)),
            "x",
        )
        .await;
        assert!(
            output
                .content
                .ends_with(&format!("\t{} [...]\n", "한".repeat(MAX_LINE_CHARS))),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn keeps_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "#!/bin/sh\necho old\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let output = run(&path, "old", "new").await;
        assert!(!output.is_error, "{}", output.content);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o751);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho new\n"
        );
    }

    #[tokio::test]
    async fn edits_the_target_of_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = temp_file(&dir, "old\n");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let output = run(&link, "old", "new").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn refuses_files_over_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let output = run(&path, "a", "b").await;
        assert!(output.is_error);
        assert!(output.content.contains("up to 10 MB"), "{}", output.content);
    }

    #[tokio::test]
    async fn refuses_directories_devices_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let output = run(dir.path(), "a", "b").await;
        assert!(output.is_error);
        assert!(output.content.ends_with("is not a regular file"));
        let output = run(Path::new("/dev/null"), "a", "b").await;
        assert!(
            output.content.contains("leads into /dev"),
            "{}",
            output.content
        );
        let output = run(Path::new("/definitely/not/here"), "a", "b").await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .starts_with("cannot edit /definitely/not/here:")
        );
    }

    #[tokio::test]
    async fn refuses_non_utf8_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latin1.txt");
        std::fs::write(&path, b"caf\xe9\n").unwrap();
        let output = run(&path, "caf", "bar").await;
        assert!(output.is_error);
        assert!(output.content.ends_with("is not valid UTF-8 text"));
        assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9\n");
    }

    #[tokio::test]
    async fn previews_the_change_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let path = temp_file(&dir, &content);
        let output = run(&path, "line 10\n", "line 10\ninserted\n").await;
        assert!(!output.is_error, "{}", output.content);
        let expected = format!(
            "edited {}: replaced 1 occurrence\n\
             \x20    7\tline 7\n\
             \x20    8\tline 8\n\
             \x20    9\tline 9\n\
             \x20   10\tline 10\n\
             \x20   11\tinserted\n\
             \x20   12\tline 11\n\
             \x20   13\tline 12\n\
             \x20   14\tline 13\n",
            path.display()
        );
        assert_eq!(output.content, expected);
    }

    #[tokio::test]
    async fn previews_separate_changes_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let content: String = (1..=40)
            .map(|n| {
                if n % 10 == 0 {
                    "hit\n".into()
                } else {
                    format!("{n}\n")
                }
            })
            .collect();
        let path = temp_file(&dir, &content);
        let output = EditFile::default()
            .run(json!({"path": path, "old_string": "hit", "new_string": "HIT", "replace_all": true}))
            .await;
        let body = output.content.split_once('\n').unwrap().1;
        let expected = "\
\x20    7\t7\n\x20    8\t8\n\x20    9\t9\n\x20   10\tHIT\n\x20   11\t11\n\x20   12\t12\n\x20   13\t13\n   ...\n\
\x20   17\t17\n\x20   18\t18\n\x20   19\t19\n\x20   20\tHIT\n\x20   21\t21\n\x20   22\t22\n\x20   23\t23\n   ...\n\
\x20   27\t27\n\x20   28\t28\n\x20   29\t29\n\x20   30\tHIT\n\x20   31\t31\n\x20   32\t32\n\x20   33\t33\n\
[1 more changes not shown]\n";
        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn previews_deletions_at_the_end_and_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "a\nb\nc\n");
        let output = run(&path, "\nc\n", "\n").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
        assert!(
            output.content.ends_with("     1\ta\n     2\tb\n"),
            "{}",
            output.content
        );

        let output = run(&path, "a\nb\n", "").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert!(output.content.ends_with("[the file is now empty]\n"));
    }

    #[tokio::test]
    async fn cancelling_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = format!("{}\nold\n", "x".repeat(4 << 20));
        let path = temp_file(&dir, &content);
        let edit = run(&path, "old", "new");
        // Drop the edit as soon as its temporary file exists, as a cancelled
        // turn drops the tool call.
        let temp_exists = async {
            while entries(&dir).len() < 2 {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            biased;
            () = temp_exists => {}
            _ = edit => {}
        }
        for _ in 0..500 {
            if entries(&dir) == ["file.txt"] {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(entries(&dir), ["file.txt"]);
        let now = std::fs::read_to_string(&path).unwrap();
        assert!(now == content || now == content.replace("old", "new"));
    }

    #[tokio::test]
    async fn crlf_matches_starting_with_a_newline_keep_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "x\r\ny\r\n");
        let output = run(&path, "\ny", "\nz\nw").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\r\nz\r\nw\r\n");
        // Without a line break of its own, the replacement joins the lines.
        let output = run(&path, "\nz", "Z").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xZ\r\nw\r\n");
    }

    #[tokio::test]
    async fn replace_all_takes_line_endings_from_each_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "p\r\nk\r\nq\r\nr\nk\ns\n");
        let output = EditFile::default()
            .run(json!({"path": path, "old_string": "\nk", "new_string": "\nK\nK", "replace_all": true}))
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "p\r\nK\r\nK\r\nq\r\nr\nK\nK\ns\n"
        );
    }

    #[tokio::test]
    async fn finds_crlf_lines_in_mostly_lf_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "a\nb\r\nc\nd\ne\n");
        // As read_file shows it: without the `\r`.
        let output = run(&path, "b\nc", "B\nC").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nB\r\nC\nd\ne\n");
        // A match across both kinds of line end still fails, and says why.
        let output = run(&path, "a\nB\nC", "x").await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .contains("Some lines in the file end in \\r\\n"),
            "{}",
            output.content
        );
        let path = temp_file(&dir, "a\nb\n");
        let output = run(&path, "zz", "x").await;
        assert!(!output.content.contains("\\r\\n"), "{}", output.content);
    }

    #[tokio::test]
    async fn overlapping_matches_say_what_replace_all_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "aaaa\n");
        let output = run(&path, "aa", "b").await;
        assert!(output.is_error);
        assert!(
            output.content.ends_with(
                "Include more surrounding lines to make it unique, or set replace_all \
                 to true to replace 2 of them that do not overlap, from the start."
            ),
            "{}",
            output.content
        );
        let output = EditFile::default()
            .run(json!({"path": path, "old_string": "aa", "new_string": "b", "replace_all": true}))
            .await;
        assert!(output.content.contains(": replaced 2 occurrences\n"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bb\n");
    }

    #[tokio::test]
    async fn a_read_only_directory_is_named_in_the_error() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes anywhere
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let path = locked.join("file.txt");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let output = run(&path, "old", "new").await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(output.is_error);
        let real_dir = locked.canonicalize().unwrap();
        assert!(
            output.content.contains(&format!(
                "cannot create a temporary file in {}",
                real_dir.display()
            )),
            "{}",
            output.content
        );
        assert!(output.content.contains("write_file"), "{}", output.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
    }

    #[tokio::test]
    async fn edits_files_with_long_names() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a".repeat(255), "한".repeat(85)] {
            let path = dir.path().join(&name);
            std::fs::write(&path, "old\n").unwrap();
            let output = run(&path, "old", "new").await;
            assert!(!output.is_error, "{}", output.content);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
            std::fs::remove_file(&path).unwrap();
        }
        assert!(entries(&dir).is_empty());
    }

    #[test]
    fn crlf_helpers() {
        assert!(uses_crlf("a\r\nb\r\nc\n"));
        assert!(!uses_crlf("a\nb\nc\r\n"));
        assert!(!uses_crlf("no breaks"));
        assert_eq!(to_crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
        assert!(has_bare_lf("\n"));
        assert!(!has_bare_lf("a\r\nb"));
        assert!(crlf_match("x\r\ny", 2, 4, false));
        assert!(!crlf_match("x\ny\r\n", 1, 3, true));
        assert!(crlf_match("xy\n", 0, 1, true));
        assert!(!crlf_match("\ny", 0, 2, true));
    }
}
