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
    /// What it does, for messages to the model: "schedules a job".
    pub does: &'static str,
    /// The same for the person's approval question, in Korean (as the
    /// question is): "예약 작업을 추가합니다".
    pub asks: &'static str,
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
        asks: "예약 작업을 추가합니다",
    },
    SelfCommand {
        words: &["cron", "remove"],
        does: "removes a scheduled job",
        asks: "예약 작업을 지웁니다",
    },
    SelfCommand {
        words: &["cron", "rm"],
        does: "removes a scheduled job",
        asks: "예약 작업을 지웁니다",
    },
    SelfCommand {
        words: &["cron", "pause"],
        does: "pauses a scheduled job",
        asks: "예약 작업을 멈춥니다",
    },
    SelfCommand {
        words: &["cron", "resume"],
        does: "resumes a scheduled job",
        asks: "예약 작업을 다시 켭니다",
    },
    SelfCommand {
        words: &["cron", "allow-home"],
        does: "lets a scheduled job work in the home folder and resumes it",
        asks: "예약 작업이 홈 폴더에서 일하게 하고 다시 켭니다",
    },
    SelfCommand {
        words: &["cron", "disallow-home"],
        does: "takes back a scheduled job's leave to work in the home folder",
        asks: "예약 작업의 홈 폴더 허용을 거둡니다",
    },
    SelfCommand {
        words: &["cron", "run"],
        does: "runs a scheduled job now and delivers its answer",
        asks: "예약 작업을 지금 실행하고 답을 보냅니다",
    },
    SelfCommand {
        words: &["cron", "daemon"],
        does: "starts running the scheduled jobs",
        asks: "예약 작업 실행을 시작합니다",
    },
    SelfCommand {
        words: &["webhook", "add"],
        does: "adds a webhook route (requests then start runs)",
        asks: "웹훅 경로를 추가합니다(그 뒤 요청이 실행을 시작합니다)",
    },
    SelfCommand {
        words: &["webhook", "add-github"],
        does: "adds a GitHub flow route (reviews, pushes, merges)",
        asks: "GitHub 흐름 경로를 추가합니다(리뷰, push, merge)",
    },
    SelfCommand {
        words: &["webhook", "remove"],
        does: "removes a webhook route",
        asks: "웹훅 경로를 지웁니다",
    },
    SelfCommand {
        words: &["webhook", "rm"],
        does: "removes a webhook route",
        asks: "웹훅 경로를 지웁니다",
    },
    SelfCommand {
        words: &["webhook", "pause"],
        does: "pauses a webhook route",
        asks: "웹훅 경로를 멈춥니다",
    },
    SelfCommand {
        words: &["webhook", "resume"],
        does: "resumes a webhook route",
        asks: "웹훅 경로를 다시 켭니다",
    },
    SelfCommand {
        words: &["webhook", "allow-home"],
        does: "lets a webhook route work in the home folder and resumes it",
        asks: "웹훅 경로가 홈 폴더에서 일하게 하고 다시 켭니다",
    },
    SelfCommand {
        words: &["webhook", "disallow-home"],
        does: "takes back a webhook route's leave to work in the home folder",
        asks: "웹훅 경로의 홈 폴더 허용을 거둡니다",
    },
    SelfCommand {
        words: &["webhook", "serve"],
        does: "starts receiving webhook requests",
        asks: "웹훅 요청을 받기 시작합니다",
    },
    SelfCommand {
        words: &["webhook", "test"],
        does: "runs a webhook route now and delivers its answer",
        asks: "웹훅 경로를 지금 실행하고 답을 보냅니다",
    },
    SelfCommand {
        words: &["webhook", "run"],
        does: "runs a webhook route now and delivers its answer",
        asks: "웹훅 경로를 지금 실행하고 답을 보냅니다",
    },
    SelfCommand {
        words: &["plugin", "install"],
        does: "installs a plugin (programs Junees runs)",
        asks: "플러그인을 설치합니다(Junees가 실행할 프로그램)",
    },
    SelfCommand {
        words: &["plugin", "enable"],
        does: "enables a plugin (its tools and hooks run in later sessions)",
        asks: "플러그인을 켭니다(다음 세션부터 그 도구와 훅이 실행됩니다)",
    },
    SelfCommand {
        words: &["plugin", "disable"],
        does: "disables a plugin",
        asks: "플러그인을 끕니다",
    },
    SelfCommand {
        words: &["plugin", "remove"],
        does: "removes a plugin",
        asks: "플러그인을 지웁니다",
    },
    SelfCommand {
        words: &["plugins", "install"],
        does: "installs a plugin (programs Junees runs)",
        asks: "플러그인을 설치합니다(Junees가 실행할 프로그램)",
    },
    SelfCommand {
        words: &["plugins", "enable"],
        does: "enables a plugin (its tools and hooks run in later sessions)",
        asks: "플러그인을 켭니다(다음 세션부터 그 도구와 훅이 실행됩니다)",
    },
    SelfCommand {
        words: &["plugins", "disable"],
        does: "disables a plugin",
        asks: "플러그인을 끕니다",
    },
    SelfCommand {
        words: &["plugins", "remove"],
        does: "removes a plugin",
        asks: "플러그인을 지웁니다",
    },
    SelfCommand {
        words: &["mcp", "login"],
        does: "logs in to an MCP server (saves an OAuth token Junees sends to it)",
        asks: "MCP 서버에 로그인합니다(Junees가 그 서버에 보낼 OAuth 토큰을 저장합니다)",
    },
    SelfCommand {
        words: &["mcp", "logout"],
        does: "removes the saved login of an MCP server",
        asks: "저장된 MCP 서버 로그인을 지웁니다",
    },
    // `--check` too: the words after `update` are not looked at.
    SelfCommand {
        words: &["update"],
        does: "replaces the Junees binary with another release",
        asks: "Junees 실행 파일을 다른 릴리스로 바꿉니다",
    },
];

/// The approval kind of these commands ([`super::Caution::kind`]).
pub const KIND: &str = "change Junees's own state";

/// The approval kind of `--allow-home` on a subcommand that takes it,
/// and of `allow-home` ([`allows_home`]): asked about every time, so an
/// "always" for [`KIND`] or an earlier one does not cover it
/// ([`crate::approval::ASKED_EVERY_TIME`]).
pub const HOME_KIND: &str = "let unattended runs work in the home folder (asked every time)";

/// The subcommands that take `--allow-home` (a job or route whose
/// unattended runs may work in the home folder).
const TAKES_ALLOW_HOME: &[&[&str]] = &[
    &["cron", "add"],
    &["webhook", "add"],
    &["webhook", "add-github"],
];

/// The subcommands that mark an existing job or route the way
/// `--allow-home` does.
const MARKS_ALLOW_HOME: &[&[&str]] = &[&["cron", "allow-home"], &["webhook", "allow-home"]];

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
        format!("`{}`: {} (Junees 자체 상태)", self.command(), self.asks)
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

/// What the approval question says when `words` (as for [`find`]) adds a
/// job or route with `--allow-home`, or marks an existing one with
/// `allow-home`: the model may run either after a refusal names it, so it
/// gets a question of its own ([`HOME_KIND`]). Any `--allow-home` word
/// counts, even one after `--` (asking more is harmless).
pub fn allows_home(words: &[String]) -> Option<String> {
    let command = find(words)?;
    if MARKS_ALLOW_HOME.contains(&command.words) {
        return Some(format!(
            "`{}`: 기존 예약 작업이나 웹훅 경로의 무인 실행이 홈 폴더에서 일하게 하고 다시 \
             켭니다. 그 실행의 도구는 보호된 경로를 뺀 홈의 모든 것을 바꿀 수 있습니다",
            command.command()
        ));
    }
    (TAKES_ALLOW_HOME.contains(&command.words) && words.iter().any(|word| word == "--allow-home"))
        .then(|| {
            format!(
                "`{} --allow-home`: 추가하는 작업의 무인 실행이 홈 폴더에서 일하게 합니다. 그 \
                 실행의 도구는 보호된 경로를 뺀 홈의 모든 것을 바꿀 수 있습니다",
                command.command()
            )
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
            ("junees webhook add pr --prompt x", "junees webhook add"),
            ("junees webhook rm pr", "junees webhook rm"),
            ("junees webhook resume pr", "junees webhook resume"),
            ("junees cron allow-home a1", "junees cron allow-home"),
            ("junees cron disallow-home a1", "junees cron disallow-home"),
            ("junees webhook allow-home pr", "junees webhook allow-home"),
            (
                "junees webhook disallow-home pr",
                "junees webhook disallow-home",
            ),
            (
                "junees webhook serve --listen 0.0.0.0:80",
                "junees webhook serve",
            ),
            ("junees webhook test pr p.json", "junees webhook test"),
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
            ("junees mcp login docs", "junees mcp login"),
            ("junees mcp logout docs", "junees mcp logout"),
            ("junees update", "junees update"),
            ("junees update --version v0.1.0", "junees update"),
            ("/home/u/.local/bin/junees update", "junees update"),
        ] {
            assert_eq!(
                find(&words(command)).map(SelfCommand::command).as_deref(),
                Some(expected),
                "{command}"
            );
        }
        for command in [
            "junees cron list",
            "junees webhook list",
            "junees webhook",
            "junees cron ls",
            "junees cron",
            "junees cron --help",
            "junees plugin",
            "junees plugin list",
            "junees plugins",
            "junees plugin --help",
            "junees mcp",
            "junees mcp --help",
            "junees updates",
            "junees -p update",
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
    fn allow_home_is_found_on_the_subcommands_that_take_it() {
        for (command, expected) in [
            (
                "junees cron add 'every 1h' x --allow-home",
                "junees cron add",
            ),
            (
                "junees webhook add pr --prompt x --allow-home --no-secret",
                "junees webhook add",
            ),
            (
                "junees webhook add-github f --repo o/n --review --allow-home",
                "junees webhook add-github",
            ),
        ] {
            let caution = allows_home(&words(command)).unwrap_or_default();
            assert!(
                caution.starts_with(&format!("`{expected} --allow-home`: 추가하는 작업의")),
                "{command}: {caution}"
            );
        }
        // Marking an existing one needs no option.
        for (command, expected) in [
            ("junees cron allow-home a1", "junees cron allow-home"),
            ("junees webhook allow-home pr", "junees webhook allow-home"),
        ] {
            let caution = allows_home(&words(command)).unwrap_or_default();
            assert!(
                caution.starts_with(&format!(
                    "`{expected}`: 기존 예약 작업이나 웹훅 경로의 무인 실행이"
                )),
                "{command}: {caution}"
            );
        }
        for command in [
            "junees cron disallow-home a1",
            "junees webhook disallow-home pr",
            "echo junees cron allow-home a1",
            "junees cron add 'every 1h' x",
            "junees cron resume a1 --allow-home",
            "junees cron list --allow-home",
            "echo junees cron add x --allow-home",
        ] {
            assert_eq!(allows_home(&words(command)), None, "{command}");
        }
        assert!(HOME_KIND.ends_with(crate::approval::ASKED_EVERY_TIME));
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
        let ran = dir.path().join("junees-ran");
        // A shell function named like the program, in a sandbox: should
        // the rule fail, no real `junees` schedules anything.
        let command = format!(
            "touch {} && junees() {{ touch {}; }}; junees cron add 'every 1m' x",
            marker.display(),
            ran.display()
        );
        let output = Bash::with_settings(&ToolSettings::default())
            .sandboxed(dir.path())
            .run(serde_json::json!({ "command": command }))
            .await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("`junees cron add` schedules a job"),
            "{}",
            output.content
        );
        assert!(!marker.exists());
        assert!(!ran.exists());
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
            "`junees cron add`: 예약 작업을 추가합니다 (Junees 자체 상태)"
        );
    }
}
