//! The system prompt, and the sections Junees adds to it from its data
//! folder (memory, skills).

use std::path::Path;

use crate::memory::{self, Memory};
use crate::skills;

/// The longest single argument Linux passes to a program
/// (`MAX_ARG_STRLEN`, 32 pages): Claude Code gets its added system prompt
/// as one argument (`--append-system-prompt`), and a longer one fails the
/// spawn with E2BIG.
pub const MAX_ARG_BYTES: usize = 128 * 1024;

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

/// The sections a session adds to its system prompt, read now: the
/// memory section for `memory_mode` (when there is a memory), then the
/// skills section for `skills_mode` (when there is a state folder `home`).
/// Every session builds them here, so all of them see the same order.
pub fn data_sections(
    memory: Option<&Memory>,
    memory_mode: memory::Mode,
    home: Option<&Path>,
    skills_mode: skills::Mode,
) -> String {
    let mut out = memory.map_or_else(String::new, |memory| memory.snapshot().section(memory_mode));
    out.push_str(&skills::system_section(home, skills_mode));
    out
}

/// What a Claude Code session (claude-code backend) gets on top of `base`
/// in its one `--append-system-prompt` argument: the memory to read only
/// (the memory tool is Junees's own) and the skill list (Claude Code
/// cannot read the skill files). Both are empty while there is nothing to
/// show; at their limits they stay well below [`MAX_ARG_BYTES`].
pub fn claude_append(base: String, memory: Option<&Memory>, home: Option<&Path>) -> String {
    base + &data_sections(
        memory,
        memory::Mode::ReadOnly,
        home,
        skills::Mode::ClaudeCode,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Limits, MAX_CHARS, Target};

    #[test]
    fn mentions_working_directory() {
        let prompt = system_prompt(Path::new("/tmp/project"));
        assert!(prompt.starts_with("You are Junees"));
        assert!(prompt.contains("Working directory: /tmp/project"));
        assert!(prompt.contains("Use edit_file to change part of a file"));
    }

    #[test]
    fn data_sections_put_memory_before_skills() {
        let home = tempfile::tempdir().unwrap();
        let memory = Memory::new(home.path(), Path::new("/p"), Limits::default());
        assert_eq!(
            claude_append("base".into(), Some(&memory), Some(home.path())),
            "base"
        );
        assert_eq!(claude_append(String::new(), None, None), "");
        memory
            .change(Target::User, memory::Change::Add("Name: 성준".into()))
            .unwrap();
        skills::Store::new(skills::dir(home.path()))
            .create(
                "deploy",
                "---\nname: deploy\ndescription: Use when deploying\n---\n1. ship\n",
            )
            .unwrap();
        let claude = claude_append("base".into(), Some(&memory), Some(home.path()));
        let memory_at = claude.find("\nMEMORY (persistent notes)\n").unwrap();
        let skills_at = claude.find("\n# Skills\n").unwrap();
        assert!(
            claude.starts_with("base") && memory_at < skills_at,
            "{claude}"
        );
        assert!(claude.contains("They cannot be changed in this session"));
        assert!(claude.contains("their files cannot be read in this session"));
        assert!(
            claude.ends_with("- deploy: Use when deploying\n"),
            "{claude}"
        );
        let api = data_sections(
            Some(&memory),
            memory::Mode::Conversation,
            Some(home.path()),
            skills::Mode::Manage,
        );
        assert!(api.find("\nMEMORY").unwrap() < api.find("\n# Skills\n").unwrap());
        assert!(api.contains("save the procedure with skill_manage"));
    }

    #[test]
    fn the_claude_code_argument_fits_at_the_limits() {
        // Every memory store full at the highest limit and the skill list
        // full, all in 4-byte characters, under a long project path.
        let home = tempfile::tempdir().unwrap();
        let big = Limits {
            user: MAX_CHARS,
            notes: MAX_CHARS,
            project: MAX_CHARS,
        };
        let root = format!("/{}", "😀".repeat(1000));
        let memory = Memory::new(home.path(), Path::new(&root), big);
        for target in Target::ALL {
            let path = memory.path(target);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "😀".repeat(MAX_CHARS * 2)).unwrap();
        }
        let skills = skills::dir(home.path());
        let description = "😀".repeat(skills::MAX_DESCRIPTION_CHARS);
        for n in 0..100 {
            let name = format!("skill-{n:03}-{}", "x".repeat(50));
            std::fs::create_dir_all(skills.join(&name)).unwrap();
            std::fs::write(
                skills.join(&name).join(skills::SKILL_FILE),
                format!("---\nname: {name}\ndescription: {description}\n---\nsteps\n"),
            )
            .unwrap();
        }
        let base = crate::task::prompt::claude_init_system();
        let argument = claude_append(base, Some(&memory), Some(home.path()));
        assert!(argument.contains("more skills not listed"));
        assert!(argument.contains("cut at the 8000-character limit"));
        assert!(argument.len() > 100 * 1024, "{}", argument.len());
        assert!(argument.len() < MAX_ARG_BYTES, "{}", argument.len());
    }
}
