//! Junees's own subcommands that change its state (`junees cron add`,
//! `junees plugin enable`, ...), recognized in bash commands.
//!
//! The model must not run them on its own: a job it schedules runs later
//! with no one watching (and can post to any Discord channel the bot sees),
//! and removing or pausing a job breaks the user's. A plugin it installs
//! and enables runs its programs in every later session (its hooks see
//! every tool call and every answer), and disabling one can turn off the
//! user's policy hook. Writing the files under
//! `$JUNEES_HOME` is already refused ([`super::settings_file::DATA_FOLDERS`]);
//! these commands change the same state without naming a path.
//!
//! [`super::CommandPolicy`] refuses them where no one is asked (`-p`, task
//! mode, scheduled runs, `JUNEES_APPROVAL=off`), and leaves them to the
//! approval question in the conversation and the Discord gateway, which
//! asks about them even with `risky` ([`super::CommandPolicy::cautions`]).
//! Read-only subcommands (`junees cron list`) are not listed and run.
//!
//! Like the rest of the policy this is a deny list: a program name built at
//! run time, a copy of the binary under another name or a script file
//! still gets past it.

use std::path::Path;

/// A subcommand of Junees that changes its state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfCommand {
    /// The words after the program name, such as `["cron", "add"]`.
    pub words: &'static [&'static str],
    /// What it does, for messages: "schedules a job".
    pub does: &'static str,
}

/// Every subcommand that changes Junees's state. A new one only needs an
/// entry here. [`find`] compares the words right after the program, so
/// every spelling the CLI accepts is listed (`plugin` and `plugins`);
/// neither `junees cron` nor `junees plugin` takes an option before its
/// subcommand.
pub const STATE_CHANGING: &[SelfCommand] = &[
    SelfCommand {
        words: &["cron", "add"],
        does: "schedules a job",
    },
    SelfCommand {
        words: &["cron", "remove"],
        does: "removes a scheduled job",
    },
    SelfCommand {
        words: &["cron", "rm"],
        does: "removes a scheduled job",
    },
    SelfCommand {
        words: &["cron", "pause"],
        does: "pauses a scheduled job",
    },
    SelfCommand {
        words: &["cron", "resume"],
        does: "resumes a scheduled job",
    },
    SelfCommand {
        words: &["cron", "run"],
        does: "runs a scheduled job now and delivers its answer",
    },
    SelfCommand {
        words: &["cron", "daemon"],
        does: "starts running the scheduled jobs",
    },
    SelfCommand {
        words: &["plugin", "install"],
        does: "installs a plugin (programs Junees runs)",
    },
    SelfCommand {
        words: &["plugin", "enable"],
        does: "enables a plugin (its tools and hooks run in later sessions)",
    },
    SelfCommand {
        words: &["plugin", "disable"],
        does: "disables a plugin",
    },
    SelfCommand {
        words: &["plugin", "remove"],
        does: "removes a plugin",
    },
    SelfCommand {
        words: &["plugins", "install"],
        does: "installs a plugin (programs Junees runs)",
    },
    SelfCommand {
        words: &["plugins", "enable"],
        does: "enables a plugin (its tools and hooks run in later sessions)",
    },
    SelfCommand {
        words: &["plugins", "disable"],
        does: "disables a plugin",
    },
    SelfCommand {
        words: &["plugins", "remove"],
        does: "removes a plugin",
    },
];

/// The approval kind of these commands ([`super::Caution::kind`]).
pub const KIND: &str = "change Junees's own state";

impl SelfCommand {
    /// The command as the user would type it: `junees cron add`.
    pub fn command(&self) -> String {
        format!("junees {}", self.words.join(" "))
    }

    /// Why the command policy refuses it where no one is asked.
    pub fn refusal(&self) -> String {
        format!(
            "`{}` {}, which changes Junees's own state: only the user does that, and no one \
             is here to approve it",
            self.command(),
            self.does
        )
    }

    /// What the approval question says about it.
    pub fn caution(&self) -> String {
        format!("`{}` {} (Junees's own state)", self.command(), self.does)
    }
}

/// The state-changing subcommand that `words` runs, if any. `words` is one
/// simple command with the wrappers (`env`, `nohup`, `timeout`, ...) and
/// assignments already taken off, as the command policy sees it: the
/// program, then its arguments.
pub fn find(words: &[String]) -> Option<&'static SelfCommand> {
    let (program, args) = words.split_first()?;
    if !is_junees(program) {
        return None;
    }
    STATE_CHANGING.iter().find(|command| {
        args.len() >= command.words.len()
            && args
                .iter()
                .zip(command.words)
                .all(|(arg, word)| arg == word)
    })
}

/// Whether `program`, as written in a command, runs Junees: `junees`
/// under any folder, the file name of the running binary (an installed
/// copy may have another name), or a path to that binary (relative paths
/// from the current folder, links followed).
pub fn is_junees(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program);
    if name == "junees" {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if !name.is_empty() && exe.file_name().is_some_and(|exe| exe == name) {
        return true;
    }
    program.contains('/')
        && match (Path::new(program).canonicalize(), exe.canonicalize()) {
            (Ok(program), Ok(exe)) => program == exe,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<String> {
        command.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn finds_the_state_changing_subcommands_of_any_junees() {
        let exe = std::env::current_exe().unwrap();
        let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
        for (command, expected) in [
            ("junees cron add every_1m post", "junees cron add"),
            ("junees cron remove a1", "junees cron remove"),
            ("junees cron rm a1", "junees cron rm"),
            ("junees cron pause a1", "junees cron pause"),
            ("junees cron resume a1", "junees cron resume"),
            ("junees cron run a1", "junees cron run"),
            ("junees cron daemon", "junees cron daemon"),
            ("/usr/local/bin/junees cron add x y", "junees cron add"),
            ("./target/debug/junees cron rm a1", "junees cron rm"),
            (
                &format!("{} cron pause a1", exe.display()),
                "junees cron pause",
            ),
            (&format!("{exe_name} cron add x y"), "junees cron add"),
            ("junees plugin install /tmp/p", "junees plugin install"),
            ("junees plugin enable p --trust", "junees plugin enable"),
            (
                "junees plugin disable policy-guard",
                "junees plugin disable",
            ),
            ("junees plugin remove p", "junees plugin remove"),
            ("junees plugins install /tmp/p", "junees plugins install"),
            ("junees plugins enable p", "junees plugins enable"),
            ("junees plugins disable p", "junees plugins disable"),
            ("junees plugins remove p", "junees plugins remove"),
        ] {
            assert_eq!(
                find(&words(command)).map(SelfCommand::command).as_deref(),
                Some(expected),
                "{command}"
            );
        }
        for command in [
            "junees cron list",
            "junees cron ls",
            "junees cron",
            "junees cron --help",
            "junees plugin",
            "junees plugin list",
            "junees plugins",
            "junees plugin --help",
            "junees status",
            "junees -p cron add",
            "junees add cron",
            "echo junees cron add x",
            "juneesx cron add x",
            "cron add x",
            "",
        ] {
            assert_eq!(find(&words(command)), None, "{command}");
        }
    }

    #[test]
    fn a_path_counts_by_where_it_leads() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("j");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &link).unwrap();
        assert!(is_junees(&link.display().to_string()));
        let other = dir.path().join("other");
        std::fs::write(&other, "").unwrap();
        assert!(!is_junees(&other.display().to_string()));
        assert!(!is_junees("/nonexistent/j"));
    }

    #[tokio::test]
    async fn bash_refuses_them_without_a_gate_and_runs_nothing() {
        use crate::tools::{Bash, Tool, ToolSettings};
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let command = format!("touch {} && junees cron add 'every 1m' x", marker.display());
        let output = Bash::with_settings(&ToolSettings::default())
            .run(serde_json::json!({ "command": command }))
            .await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("`junees cron add` schedules a job"),
            "{}",
            output.content
        );
        assert!(!marker.exists());
    }

    #[test]
    fn messages_name_the_command_and_what_it_does() {
        let add = find(&words("junees cron add x y")).unwrap();
        assert_eq!(
            add.refusal(),
            "`junees cron add` schedules a job, which changes Junees's own state: only the user \
             does that, and no one is here to approve it"
        );
        assert_eq!(
            add.caution(),
            "`junees cron add` schedules a job (Junees's own state)"
        );
    }
}
