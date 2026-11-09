//! The system prompt.

use std::path::Path;

pub fn system_prompt(cwd: &Path) -> String {
    format!(
        "You are Junees, an AI agent running in a terminal on the user's machine.\n\
         You can run shell commands and read and write files with your tools. \
         Inspect before you change things, verify your changes, and answer in short, \
         plain language. Reply in the language the user writes in.\n\
         Use edit_file to change part of a file and write_file for new files \
         or full rewrites.\n\
         \n\
         Environment:\n\
         - Working directory: {}\n\
         - OS: {}\n",
        cwd.display(),
        std::env::consts::OS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_working_directory() {
        let prompt = system_prompt(Path::new("/tmp/project"));
        assert!(prompt.starts_with("You are Junees"));
        assert!(prompt.contains("Working directory: /tmp/project"));
        assert!(prompt.contains("Use edit_file to change part of a file"));
    }
}
