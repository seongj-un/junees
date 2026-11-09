//! System prompts and inputs of the task sessions.

use std::path::Path;

use crate::prompt::system_prompt;

pub fn init_system(root: &Path) -> String {
    format!(
        "{}\n\
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
         {{\"id\": \"F1\", \"description\": \"...\", \"verify\": \"<shell command>\", \
         \"passes\": false}}. Ids are unique. `verify` runs with bash from the \
         repository root and must exit 0 exactly when the feature works (for example \
         a test command). Every feature starts with \"passes\": false.\n\
         4. Write .junees/progress.md: the goal, the plan, and what the first work \
         session needs to know.\n\
         5. If every session needs setup steps (installing dependencies, starting a \
         server), write them to .junees/init.sh.\n\
         \n\
         Do not implement any feature yet and do not commit: the harness checks the \
         feature list and commits it.\n",
        system_prompt(root)
    )
}

pub fn init_input(goal: &str) -> String {
    format!("The goal of the task:\n\n{goal}")
}

/// Sent to the init session when its feature list was rejected.
pub fn fix_input(problem: &str) -> String {
    format!(
        "The harness rejected .junees/features.json: {problem}\n\
         Fix the file. Every feature must keep \"passes\": false."
    )
}
