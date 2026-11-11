//! Task sessions on the `claude-code` backend (see [`crate::claude_code`]):
//! one `claude -p` run per session, its events shown like an agent's,
//! saved raw to a file, and counted against the budget. Claude Code has no
//! `mark_feature_passing`: after the session the harness runs the verify
//! commands itself (see the runner).

use std::path::Path;

#[cfg(test)]
pub(crate) use crate::claude_code::testing;
pub use crate::claude_code::*;

use super::runner::{Report, Reporter};
use crate::agent::Limit;
use crate::cancel::Cancel;
use crate::tools::ToolSettings;

/// One session for [`ClaudeCode::run`].
pub struct Request<'a> {
    /// The repository root, where `claude` runs.
    pub root: &'a Path,
    /// Appended to Claude Code's own system prompt.
    pub system: &'a str,
    /// The prompt of the session.
    pub input: &'a str,
    /// `--max-turns`.
    pub max_turns: usize,
    /// The user's command rules and hidden variables.
    pub settings: &'a ToolSettings,
    /// Where the raw stream-json output is saved, if anywhere.
    pub log: Option<&'a Path>,
}

impl ClaudeCode {
    /// The arguments of a session. The session runs unattended, so nothing
    /// may wait for a person: edits and the [`TOOLS`] are allowed without
    /// asking, anything else that would ask is refused
    /// (`--permission-prompts none`), and [`deny_rules`] refuse the
    /// dangerous commands. `--dangerously-skip-permissions` is never used.
    /// [`settings_rules`] keep Claude Code off Junees's settings file
    /// (`request.settings.settings_file`) and its folder.
    /// Settings files of the repository are not loaded
    /// (`--setting-sources user`), nor are its MCP servers
    /// (`--strict-mcp-config`): a session can write them, and the next
    /// session would run their hooks and servers outside the rules.
    pub fn arguments(&self, request: &Request<'_>) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            request.input,
            "--output-format",
            "stream-json",
            "--verbose",
            "--append-system-prompt",
            request.system,
            "--permission-mode",
            "acceptEdits",
            "--permission-prompts",
            "none",
            "--tools",
            TOOLS,
            "--allowedTools",
            TOOLS,
            "--setting-sources",
            "user",
            "--settings",
            crate::context_files::CLAUDE_SETTINGS,
            "--strict-mcp-config",
            "--max-turns",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        args.push(request.max_turns.to_string());
        if let Some(model) = &self.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        // Last: the option takes every argument up to the next option.
        let mut deny = deny_rules(request.settings);
        if let Some(file) = &request.settings.settings_file {
            deny.extend(settings_rules(file, request.root));
        }
        if !deny.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(deny);
        }
        args
    }

    /// Runs one session to its end. `cancel` and `limit` stop it (SIGINT to
    /// Claude Code's process group, which ends its turn, and after
    /// [`ClaudeCode::grace`] SIGKILL); `limit` is asked after each model
    /// response and every second, with the tokens the session used so far.
    pub async fn run(
        &self,
        request: &Request<'_>,
        cancel: &Cancel,
        limit: Option<&mut Limit>,
        report: &mut Reporter<'_>,
    ) -> Outcome {
        let spawn = Spawn {
            root: request.root,
            args: self.arguments(request),
            settings: request.settings,
            log: request.log,
            partial: false,
            resumed: false,
            stdin: None,
        };
        self.session(&spawn, cancel, limit, &mut |event| {
            report(Report::Event(event))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::testing::{SESSION, claude, fake_claude, fake_claude_auth, print};
    use super::*;
    use crate::agent::Event;
    use crate::message::Usage;
    use crate::task::budget::tokens;
    use crate::tools::SettingsFile;

    /// Collects what a session reports, one string per event.
    fn collector(seen: &Arc<Mutex<Vec<String>>>) -> impl FnMut(Report<'_>) + use<> {
        let seen = Arc::clone(seen);
        move |report| {
            let text = match report {
                Report::Line(text) => format!("line {text}"),
                Report::Event(Event::TextDelta(text)) => format!("text {text}"),
                Report::Event(Event::Warning(text)) => format!("warn {text}"),
                Report::Event(Event::ToolCall { name, input }) => format!("-> {name} {input}"),
                Report::Event(Event::ToolResult { name, output }) => {
                    format!("<- {name} {} {}", output.is_error, output.content)
                }
                Report::Event(_) => return,
            };
            seen.lock().unwrap().push(text);
        }
    }

    fn request<'a>(
        root: &'a Path,
        settings: &'a ToolSettings,
        log: Option<&'a Path>,
    ) -> Request<'a> {
        Request {
            root,
            system: "rules",
            input: "do it",
            max_turns: 7,
            settings,
            log,
        }
    }

    #[test]
    fn arguments_allow_the_tools_without_skipping_permissions() {
        let settings = ToolSettings::default();
        let root = Path::new("/repo");
        let mut claude = claude(PathBuf::from("claude"));
        let args = claude.arguments(&request(root, &settings, None));
        assert_eq!(
            args[..23],
            [
                "-p",
                "do it",
                "--output-format",
                "stream-json",
                "--verbose",
                "--append-system-prompt",
                "rules",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none",
                "--tools",
                "Read,Edit,Write,Glob,Grep,Bash",
                "--allowedTools",
                "Read,Edit,Write,Glob,Grep,Bash",
                "--setting-sources",
                "user",
                "--settings",
                crate::context_files::CLAUDE_SETTINGS,
                "--strict-mcp-config",
                "--max-turns",
                "7",
                "--disallowedTools",
            ]
        );
        assert_eq!(args[23..], deny_rules(&settings)[..]);
        for skip in [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "bypassPermissions",
            "--bare",
        ] {
            assert!(!args.iter().any(|arg| arg == skip), "{skip}");
        }

        claude.model = Some("opus".into());
        let args = claude.arguments(&request(root, &settings, None));
        assert_eq!(args[22..24], ["--model", "opus"]);
        assert_eq!(args[24], "--disallowedTools");
    }

    #[test]
    fn arguments_keep_claude_code_off_the_settings_folder() {
        let file = SettingsFile::new(
            "/home/tester/.junees/config.toml",
            Some(PathBuf::from("/home/tester")),
        );
        let settings = ToolSettings {
            settings_file: Some(file.clone()),
            ..ToolSettings::default()
        };
        let claude = claude(PathBuf::from("claude"));
        let expected = [
            "Read(//home/tester/.junees/config.toml)",
            "Edit(//home/tester/.junees/config.toml)",
            "Read(//home/tester/.git-credentials)",
            "Read(//home/tester/.git-credentials/**)",
            "Edit(//home/tester/.git-credentials)",
            "Edit(//home/tester/.git-credentials/**)",
            "Read(//home/tester/.config/git/credentials)",
            "Read(//home/tester/.config/git/credentials/**)",
            "Edit(//home/tester/.config/git/credentials)",
            "Edit(//home/tester/.config/git/credentials/**)",
            "Read(//home/tester/.config/gh)",
            "Read(//home/tester/.config/gh/**)",
            "Edit(//home/tester/.config/gh)",
            "Edit(//home/tester/.config/gh/**)",
            "Read(//home/tester/.junees/**)",
            "Edit(//home/tester/.junees/**)",
            "Bash(*/home/tester/.junees*)",
            "Bash(*~/.junees*)",
            "Bash(*$HOME/.junees*)",
            "Bash(*${HOME}/.junees*)",
            "Bash(*$JUNEES_HOME*)",
            "Bash(*${JUNEES_HOME}*)",
        ];
        let args = claude.arguments(&request(Path::new("/repo"), &settings, None));
        let deny = &args[args
            .iter()
            .position(|arg| arg == "--disallowedTools")
            .unwrap()
            + 1..];
        assert_eq!(deny[deny.len() - expected.len()..], expected);
        assert_eq!(
            deny[..deny.len() - expected.len()],
            deny_rules(&ToolSettings::default())[..]
        );

        // Even with the command policy off.
        let open = ToolSettings {
            allow_dangerous: true,
            ..settings.clone()
        };
        let args = claude.arguments(&request(Path::new("/repo"), &open, None));
        // Only the rules that hold with the policy off come before them.
        let kept = deny_rules(&open);
        assert!(kept.iter().all(|rule| rule.starts_with("Bash(junees ")));
        let start = args.len() - expected.len() - kept.len();
        assert_eq!(args[start - 1], "--disallowedTools");
        assert_eq!(args[start..args.len() - expected.len()], kept[..]);
        assert_eq!(args[args.len() - expected.len()..], expected);

        // `..` is resolved, as Claude Code compares resolved paths.
        let dotted = SettingsFile::new("/nonexistent-junees/a/../j/config.toml", None);
        let rules = settings_rules(&dotted, Path::new("/repo"));
        assert_eq!(
            rules[..4],
            [
                "Read(//nonexistent-junees/j/config.toml)",
                "Edit(//nonexistent-junees/j/config.toml)",
                "Read(//nonexistent-junees/j/**)",
                "Edit(//nonexistent-junees/j/**)",
            ]
        );
        // A relative path starts from the current folder.
        let relative = SettingsFile::new("state/config.toml", None);
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(
            settings_rules(&relative, Path::new("/repo"))[0],
            format!("Read(/{}/state/config.toml)", cwd.display())
        );

        // A folder holding the repository, or inside it: only the file and
        // the secrets folder.
        let file = SettingsFile::new("/nonexistent-junees/j/config.toml", None);
        for root in ["/nonexistent-junees/j/repo", "/nonexistent-junees", "/"] {
            assert_eq!(
                settings_rules(&file, Path::new(root)),
                [
                    "Read(//nonexistent-junees/j/config.toml)",
                    "Edit(//nonexistent-junees/j/config.toml)",
                    "Read(//nonexistent-junees/j/secrets/**)",
                    "Edit(//nonexistent-junees/j/secrets/**)",
                ],
                "{root}"
            );
        }

        // Gitignore pattern characters match only themselves; a Bash rule
        // cannot escape `*`, so that name is left out.
        let odd = SettingsFile::new("/srv/a [x]/*?\\/config.toml", None);
        assert_eq!(
            settings_rules(&odd, Path::new("/repo")),
            [
                "Read(//srv/a \\[x\\]/\\*\\?\\\\/config.toml)",
                "Edit(//srv/a \\[x\\]/\\*\\?\\\\/config.toml)",
                "Read(//srv/a \\[x\\]/\\*\\?\\\\/**)",
                "Edit(//srv/a \\[x\\]/\\*\\?\\\\/**)",
                "Bash(*$JUNEES_HOME*)",
                "Bash(*${JUNEES_HOME}*)",
            ]
        );
    }

    #[tokio::test]
    async fn runs_a_session_and_saves_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(
            dir.path(),
            &format!(
                "[ \"$1\" = --version ] && exit 3\n\
                 printf '%s\\0' \"$@\" > args\n\
                 echo \"${{CLAUDE_CODE_SUBPROCESS_ENV_SCRUB-unset}}\" > scrub\n\
                 echo made > made.txt\n\
                 echo 'not json'\n\
                 {}",
                print(SESSION)
            ),
        );
        let settings = ToolSettings::default();
        let log = dir.path().join("out/session-1.jsonl");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = claude(bin)
            .run(
                &request(dir.path(), &settings, Some(&log)),
                &Cancel::new(),
                None,
                &mut collector(&seen),
            )
            .await;
        assert_eq!(outcome.end.unwrap(), End::Answered);
        assert_eq!(tokens(outcome.usage), 187);
        assert!(dir.path().join("made.txt").exists());
        // Claude Code keeps credentials from the commands it runs.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("scrub")).unwrap(),
            "1\n"
        );
        let args = std::fs::read(dir.path().join("args")).unwrap();
        let args: Vec<String> = args
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        assert_eq!(
            args,
            claude(PathBuf::new()).arguments(&request(dir.path(), &settings, None))
        );
        let saved = std::fs::read_to_string(&log).unwrap();
        assert_eq!(saved, format!("not json\n{SESSION}\n"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], "warn claude: not json");
        assert_eq!(seen[1], "text Looking.");
    }

    #[tokio::test]
    async fn every_session_checks_the_sign_in_first() {
        let dir = tempfile::tempdir().unwrap();
        // Records where and how it was asked; the session records that it ran.
        let calls = dir.path().join("auth-calls");
        let ran = dir.path().join("ran");
        let fake = |method: &str| {
            claude(fake_claude_auth(
                dir.path(),
                &format!(
                    "printf '%s\\n' \"$PWD\" \"$*\" \"${{{SCRUB_ENV}-unset}}\" >> '{}'\n\
                     echo '{{\"loggedIn\":true,\"authMethod\":\"{method}\"}}'; exit 0",
                    calls.display()
                ),
                &format!("touch '{}'\n{}", ran.display(), print(SESSION)),
            ))
        };
        let settings = ToolSettings::default();
        let request = request(dir.path(), &settings, None);
        let run = async |claude: ClaudeCode| {
            claude
                .run(&request, &Cancel::new(), None, &mut |_| {})
                .await
        };
        let outcome = run(fake("api_key_helper")).await;
        let err = outcome.end.unwrap_err();
        assert!(
            format!("{err:#}").contains("signs in with `api_key_helper`"),
            "{err:#}"
        );
        assert!(!ran.exists(), "the session ran");
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            "/\nauth status --json\nunset\n"
        );

        let outcome = run(fake("claude.ai")).await;
        assert_eq!(outcome.end.unwrap(), End::Answered);
        assert!(ran.exists());
    }

    /// Whether a process is gone (a zombie counts as gone).
    fn gone(pid: &str) -> bool {
        match std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    async fn until_gone(pid: &str) {
        for _ in 0..100 {
            if gone(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("process {pid} is still there");
    }

    /// A session that starts long commands, prints its first response (then
    /// creates `printed`) and waits; `trap` runs first. Like Claude Code's
    /// Bash tool, it starts one command detached, in a session of its own
    /// (`setsid`), and that command starts another. Their pids are in the
    /// files [`COMMANDS`]: in Claude Code's group, detached, its child.
    fn hanging(trap: &str) -> String {
        format!(
            "{trap}\nsleep 30 &\necho $! > pid\n\
             setsid bash -c 'sleep 30 & echo $! > gpid; wait' &\necho $! > dpid\n\
             until [ -s gpid ]; do sleep 0.01; done\n\
             {}\ntouch printed\nwait\nsleep 30\n",
            print(&SESSION.lines().take(2).collect::<Vec<_>>().join("\n"))
        )
    }

    /// The pid files of [`hanging`].
    const COMMANDS: [&str; 3] = ["pid", "dpid", "gpid"];

    async fn commands_gone(dir: &Path) {
        for name in COMMANDS {
            until_gone(&wait_for(&dir.join(name)).await).await;
        }
    }

    async fn wait_for(path: &Path) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && (text.ends_with('\n') || path.ends_with("printed"))
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{} never appeared", path.display());
    }

    #[tokio::test]
    async fn cancel_stops_the_session_and_its_commands() {
        for trap in ["", "trap '' INT"] {
            let dir = tempfile::tempdir().unwrap();
            let bin = fake_claude(dir.path(), &hanging(trap));
            let settings = ToolSettings::default();
            let cancel = Cancel::new();
            let claude = claude(bin);
            let request = request(dir.path(), &settings, None);
            let stop = async {
                wait_for(&dir.path().join("printed")).await;
                cancel.cancel();
            };
            let mut report = |_: Report<'_>| {};
            let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(claude.run(&request, &cancel, None, &mut report), stop)
            })
            .await
            .expect("the session did not stop");
            assert_eq!(outcome.end.unwrap(), End::Interrupted, "trap `{trap}`");
            assert_eq!(tokens(outcome.usage), 115);
            commands_gone(dir.path()).await;
        }
    }

    #[tokio::test]
    async fn the_budget_stops_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_claude(dir.path(), &hanging(""));
        let settings = ToolSettings::default();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let mut limit: Limit = Box::new({
            let asked = Arc::clone(&asked);
            move |usage| {
                asked.lock().unwrap().push(tokens(usage));
                (tokens(usage) > 100).then(|| "token budget used up".to_string())
            }
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            claude(bin).run(
                &request(dir.path(), &settings, None),
                &Cancel::new(),
                Some(&mut limit),
                &mut collector(&seen),
            ),
        )
        .await
        .expect("the budget did not stop the session");
        assert_eq!(
            outcome.end.unwrap(),
            End::Budget("token budget used up".into())
        );
        assert_eq!(asked.lock().unwrap().last(), Some(&115));
        assert!(
            seen.lock()
                .unwrap()
                .contains(&"warn token budget used up; stopping Claude Code".to_string())
        );
        commands_gone(dir.path()).await;
    }

    #[tokio::test]
    async fn a_missing_program_fails_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let settings = ToolSettings::default();
        let outcome = claude(dir.path().join("nothing"))
            .run(
                &request(dir.path(), &settings, None),
                &Cancel::new(),
                None,
                &mut |_| {},
            )
            .await;
        let err = outcome.end.unwrap_err();
        assert!(format!("{err:#}").starts_with("cannot run `"), "{err:#}");
        assert_eq!(outcome.usage, Usage::default());
    }
}
