//! Webhook runs may not write the project instruction files
//! ([`crate::context_files`]): anyone's text drives such a run, and what it
//! wrote to `AGENTS.md` would be the project's instructions for every
//! later session in that folder. Webhook runs read the files from the
//! last commit anyway; this keeps the working tree's copy clean too.
//!
//! The rule goes by file name ([`ToolSettings::instruction_files`]), in
//! any folder: write_file and edit_file refuse a path with such a name
//! (or a link to one), and the command policy refuses a redirect that
//! writes to one and any program other than the readers that names one,
//! like the rule for Junees's data folders. A deny list: a name built at
//! run time, or a command that writes a whole folder (`cp -r`, `git
//! checkout -- .`), still gets past it.
//!
//! [`ToolSettings::instruction_files`]: super::ToolSettings::instruction_files

use std::path::Path;

use super::settings_file::{expand_braces, glob_match, is_glob, lands};

/// The instruction file name the shell word `word` may write to: its last
/// path component, with braces expanded and the part after the first `=`
/// counted too (`--output=AGENTS.md`); a glob counts when it can match.
pub fn named<'a>(word: &str, names: &'a [String]) -> Option<&'a str> {
    let mut words = Vec::new();
    for word in std::iter::once(word).chain(word.split_once('=').map(|(_, value)| value)) {
        expand_braces(word, &mut words);
    }
    words.iter().find_map(|word| {
        let last = word.rsplit('/').next().unwrap_or(word);
        names
            .iter()
            .find(|name| {
                if is_glob(last) {
                    glob_match(last, name)
                } else {
                    last == name.as_str()
                }
            })
            .map(String::as_str)
    })
}

/// The instruction file name of `path` for write_file and edit_file: its
/// own name, or the name of the file it leads to once links (dangling
/// ones too) are followed.
pub fn path_named<'a>(path: &Path, names: &'a [String]) -> Option<&'a str> {
    let absolute = std::path::absolute(path).ok();
    let landed = absolute.as_deref().and_then(lands);
    [Some(path), landed.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|path| path.file_name()?.to_str())
        .find_map(|file| names.iter().find(|name| name.as_str() == file))
        .map(String::as_str)
}

/// The error write_file and edit_file return for `path`.
pub fn refusal(path: &str, name: &str) -> String {
    format!(
        "{path} is a project instruction file ({name}). This run was started by a webhook \
         (anyone's text), so it may read instruction files but not change them: later \
         sessions in this folder would take what it wrote as the project's instructions."
    )
}

/// Why the command policy refuses a command that may write to `word`.
pub fn reason(word: &str, name: &str) -> String {
    format!(
        "`{word}` is a project instruction file ({name}). This run was started by a webhook \
         (anyone's text): commands may read instruction files (cat, grep, ls and the like) \
         but not change, move or copy them, since later sessions in this folder would take \
         what it wrote as the project's instructions."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        crate::context_files::DEFAULT_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect()
    }

    #[test]
    fn knows_the_names_in_any_spelling_of_a_word() {
        let names = names();
        for word in [
            "AGENTS.md",
            "./AGENTS.md",
            "sub/dir/CLAUDE.md",
            "/abs/JUNEES.md",
            "--output=AGENTS.md",
            "{notes,AGENTS}.md",
            "*.md",
            "AGENTS.*",
            "[A]GENTS.md",
            "?LAUDE.md",
        ] {
            assert!(named(word, &names).is_some(), "{word}");
        }
        for word in ["notes.md", "AGENTS.md.bak", "agents.md", "AGENTS.md/", "-i"] {
            assert_eq!(named(word, &names), None, "{word}");
        }
        assert_eq!(named("AGENTS.md", &[]), None);
    }

    #[test]
    fn follows_links_to_a_name() {
        let names = names();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("AGENTS.md"), "rules\n").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.join("link.md")).unwrap();
        // A dangling link: writing it creates the file it names.
        std::os::unix::fs::symlink("sub/CLAUDE.md", root.join("dangling.md")).unwrap();
        assert_eq!(path_named(&root.join("link.md"), &names), Some("AGENTS.md"));
        assert_eq!(
            path_named(&root.join("dangling.md"), &names),
            Some("CLAUDE.md")
        );
        assert_eq!(
            path_named(&root.join("x/../JUNEES.md"), &names),
            Some("JUNEES.md")
        );
        assert_eq!(path_named(&root.join("notes.md"), &names), None);
    }
}
