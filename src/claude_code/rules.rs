//! Junees's command policy and settings file as Claude Code permission
//! rules (`--disallowedTools`).

use std::path::Path;

use crate::tools::self_command::STATE_CHANGING;
use crate::tools::{SettingsFile, ToolSettings};

/// Command rules Claude Code refuses (`--disallowedTools`), in its
/// permission rule syntax: the core of the bash command policy, unless
/// `JUNEES_ALLOW_DANGEROUS` turned it off, and every
/// `JUNEES_DENY_COMMANDS` prefix, and Junees's own state-changing
/// subcommands (`junees cron add`; Claude Code asks no one about them, so
/// they are refused like in `-p`). Claude Code checks each command of a
/// compound line (`a && b`, pipes, `$(...)`) against them, but the rules
/// only match words as written: `/usr/bin/sudo`, `git -C . push --force`
/// or a command a script builds at run time get past them, like they get
/// past parts of the bash tool's own policy.
pub fn deny_rules(settings: &ToolSettings) -> Vec<String> {
    let mut rules = Vec::new();
    if !settings.allow_dangerous {
        for program in [
            "sudo", "su", "doas", "pkexec", "runuser", "shutdown", "reboot", "halt", "poweroff",
            "wipefs",
        ] {
            rules.push(format!("Bash({program} *)"));
        }
        // No space: also mkfs.ext4 and friends.
        rules.push("Bash(mkfs*)".into());
        for action in ["poweroff", "reboot", "halt", "kexec"] {
            rules.push(format!("Bash(systemctl {action} *)"));
        }
        rules.push("Bash(dd * of=/dev/*)".into());
        // A `*` in a rule always matches anything, so `rm -rf /*` cannot
        // be told apart from `rm -rf /tmp/x`; those are left to the rules
        // below and to the user's `JUNEES_DENY_COMMANDS`.
        for flags in ["-rf", "-fr", "-r", "-R", "-Rf", "-fR"] {
            for target in ["/", "~", "~/", "$HOME", "${HOME}", "..", "../"] {
                rules.push(format!("Bash(rm {flags} {target} *)"));
            }
        }
        for push in [
            "git push --force *",
            "git push * --force",
            "git push * --force *",
            "git push -f *",
            "git push * -f",
            "git push * -f *",
            "git push * +*",
        ] {
            rules.push(format!("Bash({push})"));
        }
        // A shell reading its script from a pipe: `curl ... | sh`.
        for shell in ["sh", "bash", "zsh", "dash", "ksh"] {
            rules.push(format!("Bash({shell})"));
            rules.push(format!("Bash({shell} -)"));
            rules.push(format!("Bash({shell} -s *)"));
        }
    }
    for command in STATE_CHANGING {
        let command = command.command();
        rules.push(format!("Bash({command})"));
        rules.push(format!("Bash({command} *)"));
    }
    if settings.webhook {
        // A webhook run (anyone's text): no push, no change of git's
        // configuration, no credentials, no aliases from `-c` (Junees's
        // own bash allows reading the configuration; these rules cannot
        // tell reading from writing).
        for rule in [
            "git push",
            "git push *",
            "git -C * push*",
            "git send-pack *",
            "git subtree push *",
            "git config *",
            "git -C * config *",
            "git credential*",
            "git-credential*",
            "git -c *",
        ] {
            rules.push(format!("Bash({rule})"));
        }
    }
    for prefix in &settings.deny_commands {
        rules.push(format!("Bash({prefix} *)"));
    }
    rules
}

/// Deny rules that keep Claude Code off Junees's settings file, in its
/// permission rule syntax (<https://code.claude.com/docs/en/permissions>).
///
/// `Read` and `Edit` rules name the file and, unless the settings folder
/// holds the repository or lies inside it, the whole folder (`/**`), else
/// at least its secrets folder (`secrets/**`, webhook secret files):
/// sessions keep their state in the repository's `.junees/`, so they need
/// nothing from `$JUNEES_HOME`. `//` starts an absolute path, and the
/// pattern is gitignore syntax, so its special characters are escaped. A
/// `Read` deny rule also refuses Read, Grep and Glob there and blocks Edit
/// and Write (Claude Code 2.1.228+); the `Edit` rule covers every tool that
/// edits files, NotebookEdit too. They also apply to the file commands
/// Claude Code recognizes in Bash (`cat`, `sed`, `tee`, redirections), but
/// not to commands that read files without naming them (`grep -r`).
///
/// `Bash` rules refuse commands that name the folder as Junees's own bash
/// tool sees it (its path, `~/...`, `$HOME/...`, `$JUNEES_HOME`), so
/// `grep -r api_key ~/.junees` and `cp -r ~/.junees /tmp` are refused too.
/// Paths built at run time or reached from a folder above still get
/// through. The path is made absolute and resolved (`..` and symbolic
/// links) first, as Claude Code compares resolved paths. Empty for a path
/// that is not UTF-8 (no argument can name it).
pub fn settings_rules(file: &SettingsFile, root: &Path) -> Vec<String> {
    let path = file.resolved_path();
    let Some(text) = path.to_str() else {
        return Vec::new();
    };
    let mut rules = vec![
        format!("Read(/{})", gitignore_escape(text)),
        format!("Edit(/{})", gitignore_escape(text)),
    ];
    let secrets: Vec<String> = file
        .resolved_secrets()
        .as_deref()
        .and_then(Path::to_str)
        .map(|dir| {
            let dir = gitignore_escape(dir);
            vec![format!("Read(/{dir}/**)"), format!("Edit(/{dir}/**)")]
        })
        .unwrap_or_default();
    let Some(folder) = path.parent().filter(|folder| folder.parent().is_some()) else {
        rules.extend(secrets);
        return rules;
    };
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = crate::tools::resolve(&root).unwrap_or(root);
    if root.starts_with(folder) || folder.starts_with(&root) {
        rules.extend(secrets);
        return rules;
    }
    let Some(folder_text) = folder.to_str() else {
        return rules;
    };
    rules.push(format!("Read(/{}/**)", gitignore_escape(folder_text)));
    rules.push(format!("Edit(/{}/**)", gitignore_escape(folder_text)));
    // A Bash rule's `*` matches any text and has no escape: skip names
    // that would change the pattern.
    let mut names = vec![folder_text.to_string()];
    if let Some(rest) = file
        .home()
        .and_then(|home| folder.strip_prefix(home).ok())
        .and_then(Path::to_str)
        .filter(|rest| !rest.is_empty())
    {
        for home in ["~", "$HOME", "${HOME}"] {
            names.push(format!("{home}/{rest}"));
        }
    }
    names.extend(["$JUNEES_HOME".to_string(), "${JUNEES_HOME}".to_string()]);
    for name in names {
        if !name.contains(['*', '(', ')']) {
            rules.push(format!("Bash(*{name}*)"));
        }
    }
    rules
}

/// `text` with gitignore's pattern characters escaped, so they match only
/// themselves.
fn gitignore_escape(text: &str) -> String {
    let mut pattern = String::new();
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_rules_carry_the_command_policy() {
        let rules = deny_rules(&ToolSettings::default());
        for rule in [
            "Bash(sudo *)",
            "Bash(su *)",
            "Bash(shutdown *)",
            "Bash(mkfs*)",
            "Bash(dd * of=/dev/*)",
            "Bash(rm -rf / *)",
            "Bash(rm -fr ~ *)",
            "Bash(rm -r $HOME *)",
            "Bash(rm -rf .. *)",
            "Bash(git push --force *)",
            "Bash(git push * -f)",
            "Bash(git push * +*)",
            "Bash(sh)",
            "Bash(bash -s *)",
            "Bash(junees cron add *)",
            "Bash(junees cron rm *)",
            "Bash(junees cron daemon)",
            "Bash(junees plugin enable *)",
            "Bash(junees plugins install *)",
        ] {
            assert!(rules.iter().any(|r| r == rule), "{rule} missing");
        }
        // No rule may deny a whole tool or `--force-with-lease`.
        assert!(
            rules
                .iter()
                .all(|rule| rule.starts_with("Bash(") && rule.ends_with(')') && rule.len() > 6)
        );
        assert!(!rules.iter().any(|rule| rule.contains("force-with-lease")));

        let settings = ToolSettings {
            deny_commands: vec!["docker rm".into()],
            allow_dangerous: true,
            ..ToolSettings::default()
        };
        let rules = deny_rules(&settings);
        assert_eq!(rules.last().unwrap(), "Bash(docker rm *)");
        // These hold with the built-in rules off.
        assert!(rules.iter().any(|rule| rule == "Bash(junees cron add *)"));
        assert!(!rules.iter().any(|rule| rule.contains("sudo")));
        assert!(!rules.iter().any(|rule| rule.contains("cron list")));
        let mut settings = settings;
        settings.allow_dangerous = false;
        assert_eq!(deny_rules(&settings).last().unwrap(), "Bash(docker rm *)");
        assert!(
            !deny_rules(&settings)
                .iter()
                .any(|rule| rule == "Bash(git push *)")
        );
        // Webhook runs: no push, git configuration or credentials, even
        // with the built-in rules off.
        settings.webhook = true;
        settings.allow_dangerous = true;
        let rules = deny_rules(&settings);
        for rule in [
            "Bash(git push)",
            "Bash(git push *)",
            "Bash(git -C * push*)",
            "Bash(git send-pack *)",
            "Bash(git config *)",
            "Bash(git credential*)",
            "Bash(git -c *)",
        ] {
            assert!(rules.iter().any(|r| r == rule), "{rule} missing");
        }
    }
}
