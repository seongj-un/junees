//! System prompts and inputs of the task sessions.

use std::path::Path;

use super::features::Feature;
use crate::prompt::system_prompt;

/// The first input of every work session.
pub const WORK_INPUT: &str = "Pick one feature that does not pass yet, implement it, verify it, \
                              commit it, and update .junees/progress.md.";

/// Characters of progress.md a work session sees in its system prompt.
const PROGRESS_TAIL_CHARS: usize = 4000;

/// What the init session must do, after the system prompt of its backend.
const INIT_RULES: &str = "\
         You are the initializer of a long-running task. Later work sessions, each \
         starting with a fresh context, will build it one feature at a time from the \
         files you write now.\n\
         \n\
         In this session:\n\
         1. Look at the repository to see what already exists.\n\
         2. Split the goal into 5 to 50 features, each small enough for one session \
         and checkable by a shell command. Order them so that no feature depends on a \
         later one.\n\
         3. Write .junees/features.json: a JSON array of objects \
         {\"id\": \"F1\", \"description\": \"...\", \"verify\": \"<shell command>\", \
         \"passes\": false}. Ids are unique. `verify` runs with bash (and `set -o pipefail`) from the \
         repository root and must exit 0 exactly when the feature works (for example \
         a test command). Every feature starts with \"passes\": false. The harness \
         runs every verify command before it accepts the list and rejects the ones \
         that already pass, so each must fail until its feature is built (check that \
         a test filter matches a test that exists by then).\n\
         4. Write .junees/progress.md: the goal, the plan, and what the first work \
         session needs to know.\n\
         5. If every session needs setup steps (installing dependencies, starting a \
         server), write them to .junees/init.sh.\n\
         \n\
         Do not implement any feature yet and do not commit: the harness checks the \
         feature list and commits it.\n";

pub fn init_system(root: &Path) -> String {
    format!("{}\n{INIT_RULES}", system_prompt(root))
}

/// The system prompt Junees appends to Claude Code's own for the init
/// session (the `claude-code` backend).
pub fn claude_init_system() -> String {
    format!("{CLAUDE_HARNESS}\n{INIT_RULES}")
}

pub fn init_input(goal: &str) -> String {
    format!("The goal of the task:\n\n{goal}")
}

/// Sent to the init session when its feature list was rejected.
pub fn fix_input(problem: &str) -> String {
    format!(
        "The harness rejected .junees/features.json: {problem}\n\
         Fix the file. Every feature must keep \"passes\": false, and its verify \
         command must fail until the feature is built."
    )
}

/// The system prompt of work session `session`: the rules and where the
/// task stands.
pub fn work_system(
    root: &Path,
    session: u64,
    features: &[Feature],
    progress: &str,
    git_log: &str,
) -> String {
    format!(
        "{}\n{}",
        system_prompt(root),
        work_rules(session, features, progress, git_log, MARK_RULE)
    )
}

/// What Junees appends to Claude Code's own system prompt for work
/// session `session` (the `claude-code` backend). Claude Code has no
/// `mark_feature_passing`: the harness runs the verify commands itself
/// after the session.
pub fn claude_work_system(
    session: u64,
    features: &[Feature],
    progress: &str,
    git_log: &str,
) -> String {
    format!(
        "{CLAUDE_HARNESS}\n{}",
        work_rules(session, features, progress, git_log, HARNESS_VERIFY_RULE)
    )
}

/// How a session with `mark_feature_passing` gets a feature passed.
const MARK_RULE: &str = "- When the feature is done, run its verify command yourself, then call \
                         mark_feature_passing with its id. Only the harness marks a feature as \
                         passing, and only when the verify command exits 0. Never edit \
                         .junees/features.json: the harness reverts every edit.\n";

/// How a Claude Code session gets a feature passed.
const HARNESS_VERIFY_RULE: &str = "- When the feature is done, run its verify command yourself \
     and make sure it exits 0. You cannot mark a feature as passing: when this session \
     ends, the harness runs the verify command of every feature that does not pass yet \
     and records the ones that exit 0. Never edit .junees/features.json: the harness \
     reverts every edit.\n";

/// Opens what Junees appends to Claude Code's system prompt.
const CLAUDE_HARNESS: &str = "You are running inside Junees, a harness for long-running \
     tasks. It started this Claude Code session without a person watching: nobody can \
     answer questions or approve anything, so work on your own and stop when you are \
     done. Some shell commands (sudo, force push, recursive removal of / or the home \
     folder, among others) are denied.\n";

/// The rules of a work session and where the task stands; `pass_rule`
/// says how a feature gets marked as passing.
fn work_rules(
    session: u64,
    features: &[Feature],
    progress: &str,
    git_log: &str,
    pass_rule: &str,
) -> String {
    let passing = features.iter().filter(|feature| feature.passes).count();
    let mut list = String::new();
    for feature in features {
        let mark = if feature.passes { 'x' } else { ' ' };
        list.push_str(&format!(
            "- [{mark}] {}: {}\n  verify: {}\n",
            feature.id, feature.description, feature.verify
        ));
    }
    format!(
        "You are working on a long-running task, in work session {session}. Every \
         session starts with a fresh context: what earlier sessions did is in git and \
         in .junees/progress.md.\n\
         \n\
         Rules:\n\
         - Work on exactly one feature that does not pass yet, normally the first one. \
         Leave the others to later sessions.\n\
         - If .junees/init.sh exists, run it first. Check that the features that \
         already pass still work before you change anything.\n\
         {pass_rule}\
         - Commit your work with git, with a message that says what changed. Append to \
         .junees/progress.md what you did, what is left and what the next session \
         should know.\n\
         - Leave the work tree clean and the code working, then stop with a short \
         summary.\n\
         \n\
         Features ({passing}/{} pass):\n{list}\n\
         .junees/progress.md (last part):\n{}\n\n\
         git log --oneline -20:\n{git_log}",
        features.len(),
        tail(progress, PROGRESS_TAIL_CHARS)
    )
}

/// The last `max_chars` characters of `text`, cut on a char boundary.
fn tail(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.trim_end().to_string();
    }
    let start = text
        .char_indices()
        .nth(total - max_chars)
        .map_or(text.len(), |(i, _)| i);
    format!("[...]\n{}", text[start..].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_prompt_shows_the_features_progress_and_log() {
        let features: Vec<Feature> = serde_json::from_value(serde_json::json!([
            {"id": "F1", "description": "parse", "verify": "cargo test parse", "passes": true},
            {"id": "F2", "description": "print", "verify": "cargo test print", "passes": false}
        ]))
        .unwrap();
        let prompt = work_system(
            Path::new("/repo"),
            7,
            &features,
            "notes\n",
            "abc123 first\n",
        );
        assert!(prompt.contains("work session 7"));
        assert!(prompt.contains(
            "Features (1/2 pass):\n- [x] F1: parse\n  verify: cargo test parse\n- [ ] F2: print"
        ));
        assert!(prompt.contains("(last part):\nnotes\n\n"));
        assert!(prompt.ends_with("git log --oneline -20:\nabc123 first\n"));
        assert!(prompt.contains("Working directory: /repo"));
    }

    #[test]
    fn init_prompts_share_the_rules() {
        let api = init_system(Path::new("/repo"));
        assert!(api.starts_with("You are Junees"));
        assert!(api.contains("Working directory: /repo\n"));
        let claude = claude_init_system();
        assert!(claude.starts_with("You are running inside Junees, a harness"));
        assert!(!claude.contains("You are Junees"));
        for prompt in [&api, &claude] {
            assert!(prompt.contains("You are the initializer of a long-running task."));
            assert!(prompt.contains("objects {\"id\": \"F1\", \"description\""));
            assert!(prompt.ends_with("the harness checks the feature list and commits it.\n"));
        }
    }

    #[test]
    fn claude_work_prompt_leaves_the_passes_to_the_harness() {
        let features: Vec<Feature> = serde_json::from_value(serde_json::json!([
            {"id": "F1", "description": "parse", "verify": "cargo test parse", "passes": false}
        ]))
        .unwrap();
        let prompt = claude_work_system(3, &features, "", "abc first\n");
        assert!(prompt.starts_with("You are running inside Junees, a harness"));
        assert!(prompt.contains("in work session 3."));
        assert!(!prompt.contains("mark_feature_passing"));
        assert!(prompt.contains(
            "when this session ends, the harness runs the verify command of every feature \
             that does not pass yet and records the ones that exit 0."
        ));
        assert!(prompt.contains("Features (0/1 pass):\n- [ ] F1: parse\n"));
        assert!(prompt.ends_with("git log --oneline -20:\nabc first\n"));
        let api = work_system(Path::new("/repo"), 3, &features, "", "abc first\n");
        assert!(api.contains("then call mark_feature_passing with its id."));
    }

    #[test]
    fn tail_cuts_on_char_boundaries() {
        assert_eq!(tail("가나다라마", 10), "가나다라마");
        assert_eq!(tail("가나다라마", 2), "[...]\n라마");
    }
}
