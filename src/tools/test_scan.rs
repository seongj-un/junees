//! A scan of the crate's own test code for commands that must never run
//! through a real shell. A test that runs a home-folder delete or a push
//! to `origin` relies on one policy rule; a mutation run that removed it
//! once deleted or pushed for real. Policy-only checks (`policy.check`)
//! are fine.
//!
//! The scan splits each file into items (tests, helper functions,
//! constants) and follows the names an item uses within its file (and
//! `tests/common`), so a command kept in a helper counts for the test
//! that runs it. Only string literals count as commands; comments do not.
//! It is a backstop: the real guard is the test sandbox
//! ([`super::bash::testing::confine`]).
//!
//! This file holds the patterns and their samples as data, runs no
//! command, and is left out of the scan.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// This file, which the scan skips.
const SELF: &str = "src/tools/test_scan.rs";

/// Code that runs a command through a real shell: a bash tool in the
/// test process, a scripted model call of the bash tool, or the junees
/// binary (whose bash runs what the fake API asks for).
const RUNS_BASH: &[&str] = &[
    "Bash::",
    "with_defaults(",
    "session_tools(",
    "dispatch(\"bash\"",
    "\"name\": \"bash\"",
    "name: \"bash\"",
    "CARGO_BIN_EXE",
    "junees_binary(",
];

/// Code that makes a real bash tool in the test process.
const BUILDS_BASH: &[&str] = &[
    "Bash::default()",
    "Bash::with_settings(",
    "with_defaults(",
    "session_tools(",
];

/// Code that puts that bash tool in the test sandbox.
const SANDBOXED: &[&str] = &["Bash::for_test(", ".sandboxed("];

/// Code that runs a command (the tool's, or a command it built).
const EXECUTES: &[&str] = &[".run(", "dispatch(", ".output()", ".spawn()"];

/// Programs that reach real state when a test's bash runs them outside
/// the sandbox (`git push` is checked as two words).
const RISKY: &[&str] = &["rm", "gh", "curl", "junees"];

/// Tests that run commands through a real bash outside the sandbox, as
/// (file, test, its command). Each command is harmless should a policy
/// rule break; these tests may not run anything in [`RISKY`].
/// An empty command: the test builds the default tools but its model calls
/// no bash, or it calls bash without a command.
pub(super) const UNSANDBOXED: &[(&str, &str, &str)] = &[
    ("src/tools/bash.rs", "captures_stdout_and_exit_code", "echo"),
    (
        "src/tools/bash.rs",
        "nonzero_exit_is_an_error_with_stderr",
        "echo",
    ),
    ("src/tools/bash.rs", "timeout_keeps_partial_output", "sleep"),
    (
        "src/tools/bash.rs",
        "timeout_kills_the_whole_process_group",
        "sleep",
    ),
    (
        "src/tools/bash.rs",
        "dropping_the_call_kills_the_whole_process_group",
        "sleep",
    ),
    (
        "src/tools/bash.rs",
        "background_processes_do_not_block_the_call",
        "sleep",
    ),
    ("src/tools/bash.rs", "huge_output_is_bounded", "head -c"),
    (
        "src/tools/bash.rs",
        "hidden_env_vars_are_removed",
        "printenv",
    ),
    ("src/tools/bash.rs", "stdin_is_closed", "cat"),
    ("src/tools/bash.rs", "missing_command_is_an_error", ""),
    (
        "src/tools/mod.rs",
        "defaults_apply_the_command_settings",
        "shutdown() {",
    ),
    (
        "src/agent.rs",
        "cancelling_a_running_tool_stops_it_and_the_next_input_works",
        "sleep",
    ),
    (
        "tests/anthropic_client.rs",
        "agent_runs_bash_through_the_real_client",
        "echo junees-e2e",
    ),
    (
        "tests/anthropic_client.rs",
        "thinking_blocks_go_back_with_the_tool_results_unchanged",
        "echo junees-think",
    ),
    (
        "tests/context_management.rs",
        "clears_then_compacts_over_the_threshold_and_replays",
        "echo junees-a",
    ),
    (
        "tests/hidden_env.rs",
        "junees_hidden_env_hides_extra_variables_from_bash",
        "printenv",
    ),
    (
        "tests/secret_env.rs",
        "default_bash_cannot_read_the_api_key",
        "printenv",
    ),
    (
        "tests/task_git_env.rs",
        "git_dir_in_the_environment_does_not_redirect_task_mode",
        "git log",
    ),
    (
        "tests/web_search.rs",
        "a_paused_search_turn_goes_on_and_its_blocks_go_back_unchanged",
        "",
    ),
    (
        "tests/web_search.rs",
        "the_same_holds_for_a_streamed_turn",
        "",
    ),
    (
        "tests/web_search.rs",
        "going_on_after_the_pause_limit_with_search_off_sends_no_open_search",
        "",
    ),
];

/// One item of a source file: a test, function or constant with the
/// lines up to the next item at its level.
struct Item {
    line: usize,
    indent: usize,
    name: Option<String>,
    test: bool,
    text: String,
}

impl Item {
    fn new(line: usize, indent: usize, name: Option<String>, test: bool) -> Self {
        Self {
            line,
            indent,
            name,
            test,
            text: String::new(),
        }
    }

    fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("?")
    }
}

enum Start {
    Test,
    Named(String),
    Other,
}

/// What a line starts, if anything.
fn start(trimmed: &str) -> Option<Start> {
    if trimmed.starts_with("#[test]") || trimmed.starts_with("#[tokio::test") {
        return Some(Start::Test);
    }
    let mut rest = trimmed;
    for visibility in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(after) = rest.strip_prefix(visibility) {
            rest = after;
        }
    }
    for keyword in ["async fn ", "const fn ", "fn ", "const ", "static "] {
        if let Some(after) = rest.strip_prefix(keyword) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return Some(if name.is_empty() {
                Start::Other
            } else {
                Start::Named(name)
            });
        }
    }
    let other = [
        "mod ", "impl ", "impl<", "trait ", "struct ", "enum ", "type ", "use ",
    ];
    other
        .iter()
        .any(|keyword| rest.starts_with(keyword))
        .then_some(Start::Other)
}

/// Splits source text into items. Items nested deeper than the current
/// one (a function inside a test) stay part of it.
fn items(text: &str) -> Vec<Item> {
    let mut done = Vec::new();
    let mut current: Option<Item> = None;
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let nested = current.as_ref().is_some_and(|item| indent > item.indent);
        if !nested && let Some(kind) = start(trimmed) {
            match kind {
                Start::Test => {
                    done.extend(current.take());
                    current = Some(Item::new(number + 1, indent, None, true));
                }
                Start::Named(name) => match current.as_mut() {
                    // The function under a test attribute.
                    Some(item) if item.test && item.name.is_none() && item.indent == indent => {
                        item.name = Some(name)
                    }
                    _ => {
                        done.extend(current.take());
                        current = Some(Item::new(number + 1, indent, Some(name), false));
                    }
                },
                Start::Other => done.extend(current.take()),
            }
        }
        if let Some(item) = current.as_mut() {
            item.text.push_str(line);
            item.text.push('\n');
        }
    }
    done.extend(current);
    done
}

/// The contents of the string literals in Rust source text, unescaped,
/// one per line. Comments and code are left out.
fn literals(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '/' if at(i + 1) == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            // A char literal; a lifetime has no closing quote.
            '\'' => {
                if at(i + 1) == Some('\\') {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if at(i + 2) == Some('\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            'r' if matches!(at(i + 1), Some('"' | '#'))
                && !i
                    .checked_sub(1)
                    .and_then(at)
                    .is_some_and(|c| (c.is_alphanumeric() || c == '_') && c != 'b') =>
            {
                let mut j = i + 1;
                let mut hashes = 0;
                while at(j) == Some('#') {
                    hashes += 1;
                    j += 1;
                }
                if at(j) != Some('"') {
                    i += 1;
                    continue;
                }
                j += 1;
                while j < chars.len()
                    && !(chars[j] == '"' && (1..=hashes).all(|k| at(j + k) == Some('#')))
                {
                    out.push(chars[j]);
                    j += 1;
                }
                out.push('\n');
                i = j + 1 + hashes;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        match at(i + 1) {
                            // A line continuation drops the next line's
                            // leading space.
                            Some('\n') => {
                                i += 2;
                                while at(i).is_some_and(char::is_whitespace) {
                                    i += 1;
                                }
                                continue;
                            }
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some(c) => out.push(c),
                            None => {}
                        }
                        i += 2;
                        continue;
                    }
                    out.push(chars[i]);
                    i += 1;
                }
                out.push('\n');
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// The simple commands of command text: lines cut at `;`, `&`, `|`, `(`,
/// `)` and backquotes.
fn simple_commands(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\n', ';', '&', '|', '(', ')', '`'])
        .map(str::trim)
        .filter(|command| !command.is_empty())
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Where `word` stands as a word of its own in `text`.
fn word_at(text: &str, word: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(found) = text[from..].find(word) {
        let start = from + found;
        let end = start + word.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if !before.is_some_and(word_char) && !after.is_some_and(word_char) {
            return Some(start);
        }
        from = end;
    }
    None
}

/// Whether a simple command is a recursive `rm` of the home folder (`~`,
/// `$HOME`, `${HOME}`, quoted or not, with or without a path after it),
/// in any spelling of the options (`-rf`, `-fr`, `-r -f`, `-R`,
/// `--recursive`).
fn deletes_home(command: &str) -> bool {
    let Some(at) = word_at(command, "rm") else {
        return false;
    };
    let mut recursive = false;
    let mut home = false;
    for token in command[at + 2..].split_whitespace() {
        if let Some(long) = token.strip_prefix("--") {
            recursive |= long == "recursive";
        } else if let Some(short) = token.strip_prefix('-') {
            recursive |= short.contains(['r', 'R']);
        } else {
            let target = token.trim_start_matches(['"', '\'', '\\']);
            let after_home = target
                .strip_prefix("${HOME}")
                .or_else(|| target.strip_prefix("$HOME"));
            home |= target.starts_with('~')
                || after_home.is_some_and(|rest| {
                    !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                });
        }
    }
    recursive && home
}

/// Whether a simple command is a `git push` naming `origin`, with any
/// options between (`git push -u origin`, `git -C repo push --force
/// origin main`).
fn pushes_to_origin(command: &str) -> bool {
    let Some(git) = word_at(command, "git") else {
        return false;
    };
    let rest = &command[git + 3..];
    let Some(push) = word_at(rest, "push") else {
        return false;
    };
    word_at(&rest[push + 4..], "origin").is_some()
}

/// The first command in `text` that must not run for real.
fn forbidden(text: &str) -> Option<&str> {
    simple_commands(text).find(|command| deletes_home(command) || pushes_to_origin(command))
}

/// The first program of [`RISKY`] (or `git push`) named in `text`.
fn risky(text: &str) -> Option<String> {
    simple_commands(text).find_map(|command| {
        if let Some(git) = word_at(command, "git")
            && word_at(&command[git + 3..], "push").is_some()
        {
            return Some("git push".to_string());
        }
        RISKY
            .iter()
            .find(|word| word_at(command, word).is_some())
            .map(|word| word.to_string())
    })
}

/// The identifiers used in `text`.
fn identifiers(text: &str) -> HashSet<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Each item's text together with the text of every item it uses by name,
/// directly or through other items.
fn closures(items: &[Item]) -> Vec<String> {
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, item) in items.iter().enumerate() {
        if let Some(name) = &item.name {
            by_name.entry(name.as_str()).or_default().push(index);
        }
    }
    let uses: Vec<Vec<usize>> = items
        .iter()
        .map(|item| {
            identifiers(&item.text)
                .into_iter()
                .filter_map(|word| by_name.get(word))
                .flatten()
                .copied()
                .collect()
        })
        .collect();
    (0..items.len())
        .map(|start| {
            let mut seen = HashSet::from([start]);
            let mut todo = vec![start];
            let mut text = String::new();
            while let Some(index) = todo.pop() {
                text.push_str(&items[index].text);
                for &next in &uses[index] {
                    if seen.insert(next) {
                        todo.push(next);
                    }
                }
            }
            text
        })
        .collect()
}

/// The test code of a source file: a file under `src/` from its first
/// `#[cfg(test)]` module on (lines before it left empty, so the line
/// numbers stay), a test file whole.
fn test_code(path: &str, text: &str) -> String {
    if !path.starts_with("src/") {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some(first) = lines.windows(2).position(|pair| {
        pair[0] == "#[cfg(test)]"
            && ["mod ", "pub mod ", "pub(crate) mod "]
                .iter()
                .any(|start| pair[1].starts_with(start))
    }) else {
        return String::new();
    };
    let mut code = "\n".repeat(first);
    for line in &lines[first..] {
        code.push_str(line);
        code.push('\n');
    }
    code
}

/// Scans `files` (path relative to the crate, text) and returns what it
/// found. `allowed` lists the tests that may run a real bash outside the
/// sandbox ([`UNSANDBOXED`]).
fn scan(files: &[(String, String)], allowed: &[(&str, &str, &str)]) -> Vec<String> {
    let common: Vec<Item> = files
        .iter()
        .find(|(path, _)| path == "tests/common/mod.rs")
        .map(|(_, text)| items(text))
        .unwrap_or_default();
    let mut found = Vec::new();
    let mut unsandboxed = HashSet::new();
    for (path, text) in files {
        if path == SELF {
            continue;
        }
        let mut all = items(&test_code(path, text));
        let own = all.len();
        if path.starts_with("tests/") && path != "tests/common/mod.rs" {
            all.extend(items(
                &common
                    .iter()
                    .map(|item| item.text.as_str())
                    .collect::<String>(),
            ));
        }
        let closures = closures(&all);
        for (item, code) in all[..own].iter().zip(&closures) {
            let has = |marks: &[&str]| marks.iter().any(|mark| code.contains(mark));
            let strings = literals(code);
            let at = format!("{path}:{} `{}`", item.line, item.label());
            if has(RUNS_BASH)
                && let Some(command) = forbidden(&strings)
            {
                found.push(format!("{at} runs `{command}` through a real shell"));
            }
            if !(item.test && has(BUILDS_BASH) && has(EXECUTES) && !has(SANDBOXED)) {
                continue;
            }
            if let Some(program) = risky(&strings) {
                found.push(format!(
                    "{at} runs `{program}` through a real bash outside the sandbox \
                     (use `Bash::for_test` or `.sandboxed`)"
                ));
            }
            let listed = allowed
                .iter()
                .find(|(file, test, _)| file == path && Some(*test) == item.name.as_deref());
            match listed {
                Some((_, _, command)) if strings.contains(command) => {}
                Some((_, _, command)) => found.push(format!(
                    "{at} is listed as running `{command}`, which it no longer does"
                )),
                None => found.push(format!(
                    "{at} runs a real bash outside the sandbox: use `Bash::for_test` \
                     or `.sandboxed`, or list it in UNSANDBOXED with its harmless command"
                )),
            }
            unsandboxed.insert((path.clone(), item.label().to_string()));
        }
    }
    for (file, test, _) in allowed {
        if !unsandboxed.contains(&(file.to_string(), test.to_string())) {
            found.push(format!(
                "UNSANDBOXED lists {file} `{test}`, which runs no real bash outside the sandbox"
            ));
        }
    }
    found
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn no_test_runs_a_home_delete_or_a_push_to_origin_for_real() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    rust_files(&root.join("src"), &mut paths);
    rust_files(&root.join("tests"), &mut paths);
    let files: Vec<(String, String)> = paths
        .iter()
        .map(|path| {
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            (relative, std::fs::read_to_string(path).unwrap())
        })
        .collect();
    assert!(files.len() > 50, "{paths:?}");
    assert!(files.iter().any(|(path, _)| path == SELF));
    let found = scan(&files, UNSANDBOXED);
    assert!(found.is_empty(), "{found:#?}");
}

/// The samples below are in-memory text only; nothing runs them.
#[test]
fn every_spelling_of_a_home_delete_is_found() {
    for command in [
        "rm -rf ~",
        "rm -fr ~",
        "rm -r -f ~",
        "rm -f -r ~",
        "rm -Rf ~",
        "rm --recursive --force ~",
        "rm -rf -- ~",
        "rm -rf ~/",
        "rm -rf ~/*",
        "rm -rf $HOME",
        "rm -rf ${HOME}",
        "rm -rf \"$HOME\"",
        "rm -rf '$HOME'",
        "rm -rf \"${HOME}/\"",
        "rm -rf /tmp/x ~",
        "/bin/rm -rf ~",
        "sudo rm -rf ~",
        "cd /tmp && rm -rf ~",
    ] {
        assert!(forbidden(command).is_some(), "missed: {command}");
    }
    for command in [
        "rm -rf ..",
        "rm -rf ./x",
        "rm -f ~/file",
        "rm ~/file",
        "rm -rf $HOMEDIR",
        "echo rm -rf; ls ~",
        "chmod -R 700 ~",
        "form -rf ~",
    ] {
        assert!(forbidden(command).is_none(), "wrongly found: {command}");
    }
}

#[test]
fn every_spelling_of_a_push_to_origin_is_found() {
    for command in [
        "git push origin",
        "git push origin HEAD:main",
        "git push -u origin main",
        "git push --force origin main",
        "git -C repo push origin",
        "cd x && git push --force-with-lease origin HEAD",
    ] {
        assert!(forbidden(command).is_some(), "missed: {command}");
    }
    for command in [
        "git push nowhere HEAD:main",
        "git pull origin main",
        "git push",
        "git push; echo origin",
        "git config remote.origin.url",
    ] {
        assert!(forbidden(command).is_none(), "wrongly found: {command}");
    }
}

#[test]
fn literals_are_read_like_rust_reads_them() {
    let source = r##"
        // A comment: rm -rf ~
        let a = "first \"quoted\" \
                 continued";
        let b = r#"raw "inner" text"#;
        let c = '"';
        let d = format!("{x}", x = 'x');
        fn f<'a>(s: &'a str) -> &'a str { "after a lifetime" }
    "##;
    assert_eq!(
        literals(source),
        "first \"quoted\" continued\nraw \"inner\" text\n{x}\nafter a lifetime\n"
    );
}

/// One sample file for [`scan`].
fn sample(path: &str, text: &str) -> Vec<(String, String)> {
    vec![(path.to_string(), text.to_string())]
}

#[test]
fn a_command_in_a_helper_constant_or_function_counts_for_its_test() {
    // The command sits outside the test, as `WALK` in proc_environ.rs.
    let constant = sample(
        "tests/sample.rs",
        r#"
const WALK: &str = "cd /tmp; rm -fr ~/";

#[tokio::test]
async fn runs_the_binary() {
    let reply = json!({"command": WALK});
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
}
"#,
    );
    let found = scan(&constant, &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/sample.rs:4 `runs_the_binary`"));

    // Through a helper function and `tests/common`.
    let helper = vec![
        (
            "tests/common/mod.rs".to_string(),
            "pub fn junees_binary() -> Command {\n    todo!()\n}\n".to_string(),
        ),
        (
            "tests/sample.rs".to_string(),
            r#"
fn publish(dir: &Path) -> String {
    format!("cd {} && git push -u origin main", dir.display())
}

#[tokio::test]
async fn pushes() {
    let command = publish(dir);
    common::junees_binary().arg(command);
}
"#
            .to_string(),
        ),
    ];
    let found = scan(&helper, &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("tests/sample.rs:6 `pushes`"));

    // A helper that runs the command itself.
    let runner = sample(
        "src/sample.rs",
        r#"
#[cfg(test)]
mod tests {
    async fn wipe() {
        Bash::for_test(dir).run(json!({"command": "rm -r -f \"$HOME\""})).await;
    }
}
"#,
    );
    let found = scan(&runner, &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("src/sample.rs:4 `wipe`"));

    // The same command checked by the policy alone, or only in a comment.
    let policy_only = sample(
        "src/sample.rs",
        r#"
#[cfg(test)]
mod tests {
    const WIPE: &str = "rm -rf ~";

    #[test]
    fn the_policy_refuses_it() {
        assert!(CommandPolicy::default().check(WIPE).is_deny());
    }

    #[tokio::test]
    async fn something_else() {
        // Never run `rm -rf ~` here.
        Bash::for_test(dir).run(json!({"command": "echo hi"})).await;
    }
}
"#,
    );
    assert_eq!(scan(&policy_only, &[]), Vec::<String>::new());
}

#[test]
fn a_real_bash_outside_the_sandbox_must_be_listed_and_harmless() {
    let file = |command: &str, build: &str| {
        sample(
            "src/sample.rs",
            &format!(
                r#"
#[cfg(test)]
mod tests {{
    #[tokio::test]
    async fn runs_it() {{
        let output = {build}.run(json!({{"command": "{command}"}})).await;
    }}
}}
"#
            ),
        )
    };
    let listed = [("src/sample.rs", "runs_it", "echo")];
    // Sandboxed: anything goes, as far as this rule is concerned.
    for build in ["Bash::for_test(dir)", "Bash::default().sandboxed(dir)"] {
        assert_eq!(scan(&file("curl https://x.invalid", build), &[]).len(), 0);
    }
    // Outside the sandbox, a listed harmless command passes.
    assert_eq!(scan(&file("echo hi", "Bash::default()"), &listed).len(), 0);
    // An unlisted one does not.
    let found = scan(&file("echo hi", "Bash::default()"), &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("outside the sandbox"), "{found:?}");
    // Nor does a risky program, listed or not.
    for command in [
        "rm -f x",
        "git push nowhere",
        "gh pr list",
        "curl https://x.invalid",
        "echo a && junees cron add x",
    ] {
        for build in [
            "Bash::default()",
            "Bash::with_settings(&settings)",
            "ToolRegistry::with_defaults(&settings)",
        ] {
            let found = scan(&file(command, build), &listed);
            assert!(
                found
                    .iter()
                    .any(|line| line.contains("through a real bash")),
                "{command} with {build}: {found:?}"
            );
        }
    }
    // A listed test that stopped running its command, or a stale entry.
    let found = scan(&file("printenv HOME", "Bash::default()"), &listed);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("no longer"), "{found:?}");
    let stale = [("src/sample.rs", "gone", "echo")];
    let found = scan(&file("echo hi", "Bash::for_test(dir)"), &stale);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("UNSANDBOXED lists"), "{found:?}");
}

#[test]
fn a_helper_building_an_unsandboxed_bash_counts_for_its_test() {
    let files = sample(
        "src/sample.rs",
        r#"
#[cfg(test)]
mod tests {
    fn tools() -> ToolRegistry {
        let mut tools = ToolRegistry::default();
        tools.register(Bash::default());
        tools
    }

    #[tokio::test]
    async fn schedules() {
        tools().dispatch("bash", json!({"command": "junees cron add x y"})).await;
    }
}
"#,
    );
    let found = scan(&files, &[]);
    assert!(
        found
            .iter()
            .any(|line| line.contains("`schedules` runs `junees`")),
        "{found:?}"
    );
}
