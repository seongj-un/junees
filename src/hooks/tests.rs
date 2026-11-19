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
    assert!(!trust::is_trusted(&home, &file).unwrap());
    trust::trust(&home, &file).unwrap();
    assert!(trust::is_trusted(&home, &read()).unwrap());
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
    assert!(!trust::is_trusted(&home, &read()).unwrap());
    // Trusting the new content replaces the old entry.
    trust::trust(&home, &read()).unwrap();
    let text = std::fs::read_to_string(trust::path(&home)).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(trust::untrust(&home, &read().path).unwrap());
    assert!(!trust::is_trusted(&home, &read()).unwrap());
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
                pins: Vec::new(),
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
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0\n");
        write(&repo.join(super::super::PROJECT_FILE), PROJECT);
        write(
            &home.join(super::super::FILE_NAME),
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"notify\"]\n",
        );
        let mut found = find(Some(&home), &repo, &repo).unwrap();
        // The user's hook runs; the project's waits for a person.
        assert_eq!(found.hooks.len(), 1);
        assert!(found.untrusted.is_some());
        found.skip_untrusted().unwrap();
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
        write(&repo.join("other.sh"), "#!/bin/sh\nexit 0\n");
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
    async fn listing_reports_a_trust_record_io_error_even_without_project_hooks() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let place = place(&home, &repo, root.path());
        assert!(cli::run(&HooksCommand::List, &place).await.is_ok());

        // A directory in place of the ledger gives a deterministic I/O
        // error even when the tests run as a user that can read any file.
        let ledger = trust::path(&home);
        std::fs::create_dir_all(&ledger).unwrap();
        for command in [
            HooksCommand::List,
            HooksCommand::Trust,
            HooksCommand::Untrust,
        ] {
            let err = format!("{:#}", cli::run(&command, &place).await.unwrap_err());
            assert!(
                err.contains("trusted-hooks") && err.contains("not a regular file"),
                "{err}"
            );
        }
        let err = format!("{:#}", find(Some(&home), &repo, &repo).unwrap_err());
        assert!(
            err.contains("trusted-hooks") && err.contains("not a regular file"),
            "{err}"
        );
        assert!(ledger.is_dir());
    }

    #[tokio::test]
    async fn damaged_trust_records_are_reported_and_not_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let place = place(&home, &repo, root.path());
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0\n");
        write(&repo.join(super::super::PROJECT_FILE), PROJECT);
        cli::run(&HooksCommand::Trust, &place).await.unwrap();
        let ledger = trust::path(&home);
        let original = std::fs::read(&ledger).unwrap();
        for (tail, cause) in [
            (&b"unfinished entry\n"[..], "line 2"),
            (&[0xff], "not UTF-8"),
        ] {
            let damaged = [original.as_slice(), tail].concat();
            std::fs::write(&ledger, &damaged).unwrap();
            for command in [
                HooksCommand::List,
                HooksCommand::Trust,
                HooksCommand::Untrust,
            ] {
                let err = format!("{:#}", cli::run(&command, &place).await.unwrap_err());
                assert!(
                    err.contains("trusted-hooks") && err.contains(cause),
                    "{err}"
                );
                assert_eq!(std::fs::read(&ledger).unwrap(), damaged);
            }
            let err = format!("{:#}", find(Some(&home), &repo, &repo).unwrap_err());
            assert!(
                err.contains("trusted-hooks") && err.contains(cause),
                "{err}"
            );
        }
        std::fs::write(&ledger, &original).unwrap();
        assert!(
            cli::run(&HooksCommand::Untrust, &place)
                .await
                .unwrap()
                .contains("no longer trusted")
        );
        assert!(std::fs::read(&ledger).unwrap().is_empty());
    }

    #[tokio::test]
    async fn trust_record_read_and_write_have_the_same_byte_limit() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let current = place(&home, &repo, root.path());
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0\n");
        write(&repo.join(super::super::PROJECT_FILE), PROJECT);
        cli::run(&HooksCommand::Trust, &current).await.unwrap();

        let ledger = trust::path(&home);
        let mut text = std::fs::read_to_string(&ledger).unwrap();
        let limit = super::super::MAX_FILE_BYTES as usize;
        let digest = "a".repeat(64);
        let remaining = limit - text.len();
        let rows = remaining / 128;
        for index in 0..rows {
            let prefix = format!("{digest} /fixtures/{index:04}/");
            let len = 128
                + if index + 1 == rows {
                    remaining % 128
                } else {
                    0
                };
            text.push_str(&prefix);
            text.push_str(&"a".repeat(len - prefix.len() - 1));
            text.push('\n');
        }
        assert_eq!(text.len(), limit);
        std::fs::write(&ledger, &text).unwrap();

        // An equal-sized replacement is allowed at exactly the read limit.
        cli::run(&HooksCommand::Trust, &current).await.unwrap();
        let bytes = std::fs::read(&ledger).unwrap();
        assert_eq!(bytes.len(), limit);
        assert!(
            cli::run(&HooksCommand::List, &current)
                .await
                .unwrap()
                .contains(": trusted\n")
        );

        let other = repository(&root.path().join("different project"));
        write(&other.join("check.sh"), "#!/bin/sh\nexit 0\n");
        write(&other.join(super::super::PROJECT_FILE), PROJECT);
        let second = place(&home, &other, root.path());
        let err = format!(
            "{:#}",
            cli::run(&HooksCommand::Trust, &second).await.unwrap_err()
        );
        assert!(
            err.contains("trusted-hooks") && err.contains("exceed 256 KiB"),
            "{err}"
        );
        assert_eq!(std::fs::read(&ledger).unwrap(), bytes);

        let mut oversized = bytes;
        oversized.push(b'\n');
        std::fs::write(&ledger, oversized).unwrap();
        let err = format!(
            "{:#}",
            cli::run(&HooksCommand::List, &current).await.unwrap_err()
        );
        assert!(
            err.contains("trusted-hooks") && err.contains("larger than 256 KiB"),
            "{err}"
        );
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
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0\n");
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
        assert!(!trust::is_trusted(&home, &file).unwrap());
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

mod project_validation {
    use super::super::{MAX_PINNED_BYTES, PROJECT_FILE, find, project_file, project_path, trust};
    use super::{repository, write};

    const VALID_HOOKS: &str = concat!(
        "[[hooks]]\nevent = \"pre_tool\"\n",
        "command = [\"python3\", \"check.py\", \"--group=team/default\", \"--input=./data/sample.txt\"]\n",
        "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"notify\"]\n",
    );

    #[test]
    fn logical_values_do_not_hide_hooks_with_explicit_inputs() {
        use super::super::explicit_path;

        for value in [
            "team/default",
            "plain words",
            "https://example.invalid/info",
        ] {
            assert!(!explicit_path(value), "{value}");
        }
        for value in [
            "./data/sample.txt",
            "../sample.txt",
            "/data/sample with spaces.txt",
        ] {
            assert!(explicit_path(value), "{value}");
        }

        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        write(&repo.join(PROJECT_FILE), VALID_HOOKS);
        write(&repo.join("check.py"), "pass\n");
        write(&repo.join("data/sample.txt"), "sample\n");
        let file = project_file(&repo, None, None).unwrap().unwrap();
        let hooks = file.hooks();
        assert_eq!(hooks.len(), 2, "{:?}", file.hooks);
        assert_eq!(hooks[0].pins.len(), 2);
        assert!(hooks[0].pins[1].path.ends_with("data/sample.txt"));
        assert!(hooks[0].pins[1].digest.is_ok());
    }

    #[test]
    fn an_approval_with_an_unhashable_input_keeps_the_existing_record() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let input = repo.join("data/sample.txt");
        write(
            &repo.join(PROJECT_FILE),
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"python3\", \"check.py\", \"./data/sample.txt\"]\n",
        );
        write(&repo.join("check.py"), "pass\n");
        write(&input, "sample\n");
        let read = || project_file(&repo, None, Some(&home)).unwrap().unwrap();
        let original = read();
        trust::trust(&home, &original).unwrap();
        let before = std::fs::read(trust::path(&home)).unwrap();

        std::fs::OpenOptions::new()
            .write(true)
            .open(&input)
            .unwrap()
            .set_len(MAX_PINNED_BYTES + 1)
            .unwrap();
        let err = trust::trust(&home, &read()).unwrap_err().to_string();
        assert!(
            err.contains("data/sample.txt") && err.contains("larger than"),
            "{err}"
        );
        assert_eq!(std::fs::read(trust::path(&home)).unwrap(), before);
        assert!(trust::is_trusted(&home, &original).unwrap());
        let err = find(Some(&home), &repo, &repo).unwrap_err().to_string();
        assert!(
            err.contains("data/sample.txt") && err.contains("larger than"),
            "{err}"
        );
    }

    #[test]
    fn a_project_config_edit_requires_repair_or_explicit_untrust() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let path = repo.join(PROJECT_FILE);
        let load = || find(Some(&home), &repo, &repo);
        let good = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"check\"]\n";

        write(&path, "[[hooks]\n");
        let fresh = load().unwrap();
        assert!(fresh.hooks.is_empty() && fresh.untrusted.is_none());
        assert_eq!(fresh.warnings.len(), 1);

        write(&path, good);
        let mut pending = load().unwrap();
        pending.trust_untrusted(&home).unwrap();
        assert_eq!(load().unwrap().hooks.len(), 1);

        write(&path, "[[hooks]\n");
        let err = load().unwrap_err().to_string();
        assert!(
            err.contains("previously trusted") && err.contains("hooks.toml"),
            "{err}"
        );
        assert!(err.contains("line 1"), "{err}");

        write(&path, good);
        assert_eq!(load().unwrap().hooks.len(), 1);
        std::fs::remove_file(&path).unwrap();
        let err = load().unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        assert!(trust::untrust(&home, &project_path(&repo, None)).unwrap());
        let absent = load().unwrap();
        assert!(absent.hooks.is_empty() && absent.warnings.is_empty());
    }

    #[test]
    fn failed_reapproval_keeps_the_changed_hooks_pending() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let path = repo.join(PROJECT_FILE);
        let good = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"check\"]\n";
        write(&path, good);
        find(Some(&home), &repo, &repo)
            .unwrap()
            .trust_untrusted(&home)
            .unwrap();
        let before = std::fs::read(trust::path(&home)).unwrap();

        write(&path, &format!("# revised policy\n{good}"));
        let mut pending = find(Some(&home), &repo, &repo).unwrap();
        let invalid_state = root.path().join("not-a-folder");
        write(&invalid_state, "ordinary file\n");
        assert!(pending.trust_untrusted(&invalid_state).is_err());
        assert!(pending.untrusted().is_some());
        assert_eq!(std::fs::read(trust::path(&home)).unwrap(), before);
        let tools = crate::tools::ToolSettings::default();
        let hide = || std::sync::Arc::new(|_: &mut String| {});
        let err = pending
            .build(&repo, &tools, hide())
            .err()
            .expect("reapproval was not saved");
        assert!(err.to_string().contains("no longer matches its approval"));

        // The same valid edit is held until the person decides.
        let mut pending = find(Some(&home), &repo, &repo).unwrap();
        assert!(pending.requires_reapproval());
        pending.trust_untrusted(&home).unwrap();
        assert!(
            pending
                .build(&repo, &tools, hide())
                .unwrap()
                .hooks
                .is_some()
        );
        assert_ne!(std::fs::read(trust::path(&home)).unwrap(), before);

        // Explicitly revoking approval lets a later run omit the hooks.
        write(&path, &format!("# another valid edit\n{good}"));
        let err = find(Some(&home), &repo, &repo)
            .unwrap()
            .build(&repo, &tools, hide())
            .err()
            .expect("an unattended run must stop");
        assert!(err.to_string().contains("junees hooks untrust"));
        assert!(trust::untrust(&home, &project_path(&repo, None)).unwrap());
        let loaded = find(Some(&home), &repo, &repo)
            .unwrap()
            .build(&repo, &tools, hide())
            .unwrap();
        assert!(loaded.hooks.is_none());
        assert_eq!(loaded.warnings.len(), 1);
    }
}

mod pinning {
    use std::sync::Arc;

    use serde_json::json;

    use super::super::run::{Hooks, Input, Outcome};
    use super::super::{
        CHANGED, Hook, NOT_PINNED, Origin, Script, config, find, indirect, script_arg, trust,
        user_hooks, user_pins,
    };
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
        let pin = file.pins[0][0].clone();
        assert_eq!(pin.path, repo.canonicalize().unwrap().join("check.sh"));
        assert!(pin.digest.is_ok());
        // The question lists what runs, with the pinned file.
        let question = super::super::trust_question(&file);
        assert!(question.contains("훅 1개:"), "{question}");
        assert!(
            question.contains("- pre_tool -> ./check.sh (pinned "),
            "{question}"
        );
        found.trust_untrusted(&home).unwrap();
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_none() && found.hooks.len() == 1);
        assert_eq!(found.hooks[0].pins, vec![pin]);
        // The script changed: the same hooks file is no longer trusted.
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0 # revised\n");
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_some() && found.hooks.is_empty());
        assert!(!trust::is_trusted(&home, found.untrusted.as_ref().unwrap()).unwrap());
        // A missing script cannot be pinned; an approved policy cannot be loaded.
        std::fs::remove_file(repo.join("check.sh")).unwrap();
        let err = find(Some(&home), &repo, &repo).unwrap_err().to_string();
        assert!(
            err.contains("check.sh") && err.contains("does not exist"),
            "{err}"
        );
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
        assert!(!user_pins(&spec("./x.sh"), &work).is_empty());
        let inside = work.join("x.sh");
        assert!(!user_pins(&spec(inside.to_str().unwrap()), &work).is_empty());
        assert!(user_pins(&spec("/bin/sh"), &work).is_empty());
        assert!(user_pins(&spec("sh"), &work).is_empty());
        assert!(user_pins(&spec("../x.sh"), &work).is_empty());
        // A link in the working folder is not followed: it cannot be pinned.
        std::os::unix::fs::symlink("/bin/sh", work.join("link")).unwrap();
        let pin = user_pins(&spec("./link"), &work).remove(0);
        assert!(pin.digest.unwrap_err().contains("link"));
    }

    #[test]
    fn a_project_hook_may_not_run_a_file_outside_the_project() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        let outside = root.path().join("outside");
        write(&outside.join("check.sh"), "#!/bin/sh\nexit 0\n");
        std::os::unix::fs::symlink(&outside, repo.join("tools")).unwrap();
        let file = |program: &str| {
            write(
                &repo.join(super::super::PROJECT_FILE),
                &format!(
                    "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"notify\"]\n\
                     [[hooks]]\nevent = \"pre_tool\"\ncommand = [\"{program}\"]\n"
                ),
            );
        };
        let outside_path = outside.canonicalize().unwrap().join("check.sh");
        for program in [
            "./tools/check.sh".to_string(),
            "tools/check.sh".to_string(),
            "../outside/check.sh".to_string(),
            "./missing/../../outside/check.sh".to_string(),
            outside_path.display().to_string(),
        ] {
            file(&program);
            let found = find(Some(&home), &repo, &repo).unwrap();
            // Not one of its hooks runs, and it cannot be trusted.
            assert!(
                found.hooks.is_empty() && found.untrusted.is_none(),
                "{program}"
            );
            let warning = found.warnings.join("\n");
            assert!(
                warning.contains("hook at line 4")
                    && warning.contains("outside the project folder")
                    && warning.contains(&program),
                "{program}: {warning}"
            );
            let read = super::super::project_file(&repo, Some(root.path()), Some(&home))
                .unwrap()
                .unwrap();
            let err = format!("{:#}", trust::trust(&home, &read).unwrap_err());
            assert!(
                err.contains("outside the project folder"),
                "{program}: {err}"
            );
        }
        file("./missing/check.sh");
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.hooks.is_empty() && found.untrusted.is_none());
        assert!(found.warnings[0].contains("does not exist"));
        // From a session in a subfolder, a file higher up in the project is
        // the project's, and pinned.
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        write(&repo.join("check.sh"), "#!/bin/sh\nexit 0\n");
        file("../check.sh");
        let found = find(Some(&home), &sub, &sub).unwrap();
        let pin = found.untrusted.unwrap().pins[1][0].clone();
        assert_eq!(pin.path, repo.canonicalize().unwrap().join("check.sh"));
        assert!(pin.digest.is_ok());
    }

    #[test]
    fn finds_the_script_a_known_interpreter_runs() {
        let script = |words: &[&str]| {
            script_arg(
                &words
                    .iter()
                    .map(|word| word.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        let code = |option: &str| Script::Code(option.to_string());
        for (words, expected) in [
            (&["sh", "x.sh"][..], Script::File(1)),
            (&["/usr/bin/bash", "-e", "x.sh"], Script::File(2)),
            (&["bash", "-o", "pipefail", "x.sh"], Script::File(3)),
            (&["bash", "-eo", "pipefail", "x.sh"], Script::File(3)),
            (&["zsh", "--rcfile", "rc", "x.sh"], Script::File(3)),
            (&["sh", "-c", "./x.sh"], code("-c")),
            (&["sh", "-ec", "./x.sh"], code("-ec")),
            (&["python3", "-u", "x.py"], Script::File(2)),
            (&["python3", "-W", "ignore", "x.py"], Script::File(3)),
            (&["python3", "-Wignore", "x.py"], Script::File(2)),
            (&["python", "-m", "pkg"], code("-m")),
            (&["python3", "-c", "import x"], code("-c")),
            (&["node", "-e", "x"], code("-e")),
            (
                &["node", "--require=./x.js", "y.js"],
                code("--require=./x.js"),
            ),
            (&["node", "--", "x.js"], Script::File(2)),
            (&["ruby", "-I", "lib", "x.rb"], Script::File(3)),
            (&["ruby", "-e", "x"], code("-e")),
            (&["perl", "-w", "x.pl"], Script::File(2)),
            (&["perl", "-e", "x"], code("-e")),
            (&["sh"], Script::None),
            (&["python3", "-"], Script::None),
            (&["python3.12", "x.py"], Script::File(1)),
            (&["/usr/local/bin/node22", "-e", "x"], code("-e")),
            (&["deno", "run", "x.ts"], Script::NotInterpreter),
            (&["make", "lint"], Script::NotInterpreter),
        ] {
            assert_eq!(script(words), expected, "{words:?}");
        }
    }

    #[test]
    fn an_interpreters_script_in_the_working_folder_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().canonicalize().unwrap().join("work");
        write(&work.join("x.sh"), "exit 0\n");
        write(&work.join("lib/x.py"), "pass\n");
        let pins = |words: &[&str]| {
            let quoted: Vec<String> = words.iter().map(|word| format!("\"{word}\"")).collect();
            let spec = config::parse(
                &format!(
                    "[[hooks]]\nevent = \"pre_tool\"\ncommand = [{}]\n",
                    quoted.join(", ")
                ),
                1,
            )
            .unwrap()
            .remove(0);
            user_pins(&spec, &work)
        };
        for (words, arg, file) in [
            (&["sh", "x.sh"][..], 1, "x.sh"),
            (&["bash", "-e", "./x.sh"], 2, "x.sh"),
            (&["/usr/bin/python3", "-u", "lib/x.py"], 2, "lib/x.py"),
        ] {
            let found = pins(words);
            assert_eq!(found.len(), 1, "{words:?}");
            assert_eq!(found[0].arg, arg, "{words:?}");
            assert_eq!(found[0].path, work.join(file), "{words:?}");
            assert!(found[0].digest.is_ok(), "{words:?}");
        }
        // A file the model could add later is pinned as missing (the hook
        // does not run).
        let missing = pins(&["python3", "later.py"]);
        assert!(
            missing[0]
                .digest
                .as_ref()
                .unwrap_err()
                .contains("not exist")
        );
        // Code given inline, files outside the folder, other programs.
        for words in [
            &["sh", "-c", "sh x.sh"][..],
            &["python3", "-m", "x"],
            &["sh", "/etc/hostname"],
            &["sh", "../x.sh"],
            &["make", "x.sh"],
        ] {
            assert!(pins(words).is_empty(), "{words:?}");
        }
        // A program in the folder running a script in the folder: both.
        write(&work.join("bin/python3"), "#!/bin/sh\n");
        let both = pins(&["./bin/python3", "lib/x.py"]);
        assert_eq!(both.iter().map(|pin| pin.arg).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn hooks_that_run_files_indirectly_are_named() {
        let hook = |words: &str, project: bool| {
            let spec = config::parse(
                &format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = {words}\n"),
                1,
            )
            .unwrap()
            .remove(0);
            let file = std::path::PathBuf::from("/x/hooks.toml");
            Hook {
                spec,
                origin: if project {
                    Origin::Project(file)
                } else {
                    Origin::User(file)
                },
                pins: Vec::new(),
            }
        };
        for (words, project, part) in [
            (r#"["make", "lint"]"#, false, "the Makefile"),
            (r#"["/usr/bin/npm", "run", "fmt"]"#, true, "package.json"),
            (
                r#"["cargo", "+nightly", "clippy"]"#,
                true,
                "`cargo clippy` may build",
            ),
            (
                r#"["sh", "-c", "./x.sh"]"#,
                true,
                "if that calls or loads files",
            ),
            (r#"["node", "--require=./a.js", "b.js"]"#, true, "--require"),
            (
                r#"["python3", "x.py"]"#,
                false,
                "relative to the session's folder",
            ),
            (r#"["python3.12", "-m", "x"]"#, true, "`python3.12 -m`"),
            (r#"["node", "-r", "./x.js", "y.js"]"#, true, "`node -r`"),
            (
                r#"["deno", "run", "x.ts"]"#,
                true,
                "`deno` is an interpreter Junees does not know",
            ),
            (
                r#"["/usr/bin/php8.2", "x.php"]"#,
                true,
                "`php8.2` is an interpreter",
            ),
            (r#"["dash", "x.sh"]"#, false, "`dash` is an interpreter"),
            (
                r#"["env", "A=1", "./x.sh"]"#,
                false,
                "`env` runs another command",
            ),
            (
                r#"["timeout", "5", "sh", "x.sh"]"#,
                true,
                "`timeout` runs another command",
            ),
            (
                r#"["/usr/bin/nice", "./x.sh"]"#,
                true,
                "`nice` runs another command",
            ),
        ] {
            let why = indirect(&hook(words, project)).unwrap_or_default();
            assert!(why.contains(part), "{words}: {why}");
        }
        for (words, project) in [
            (r#"["python3", "x.py"]"#, true),
            (r#"["python3", "/opt/hooks/x.py"]"#, false),
            (r#"["./check.sh"]"#, true),
            (r#"["notify-send", "x"]"#, false),
            (r#"["cargo", "fmt", "--all"]"#, false),
        ] {
            assert_eq!(indirect(&hook(words, project)), None, "{words}");
        }
    }

    #[tokio::test]
    async fn every_file_an_interpreter_is_given_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        for name in ["a.pl", "b.pl", "a.js", "b.js"] {
            write(&dir.join(name), "1;\n");
        }
        let input = json!({});
        // An option Junees does not know takes `a.*` as its value; the
        // interpreter runs `b.*`.
        for (command, runs) in [
            (r#"["perl", "-I", "./a.pl", "./b.pl"]"#, "b.pl"),
            (r#"["node", "--title", "./a.js", "./b.js"]"#, "b.js"),
            (r#"["node", "--env-file", "./a.js", "./b.js"]"#, "b.js"),
            (r#"["node", "-C", "./a.js", "./b.js"]"#, "b.js"),
            (r#"["node", "--loader=./a.js", "b.js"]"#, "a.js"),
            (r#"["perl", "-I./a.pl", "b.pl"]"#, "a.pl"),
        ] {
            for name in ["a.pl", "b.pl", "a.js", "b.js"] {
                write(&dir.join(name), "1;\n");
            }
            let hooks = pinned_hooks(
                &dir,
                &format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = {command}\n"),
            );
            let pins = &hooks.hooks()[0].pins;
            assert!(
                pins.iter().any(|pin| pin.path == dir.join(runs)),
                "{command}: {pins:?}"
            );
            // At most one is run from a copy: the word read as the script.
            assert!(
                pins.iter().filter(|pin| pin.copied).count() <= 1,
                "{command}"
            );
            write(&dir.join(runs), "system('touch ran');\n");
            let fired = hooks
                .fire(
                    Event::PreTool,
                    Input {
                        tool: Some(("bash", &input)),
                        ..Input::default()
                    },
                )
                .await;
            assert!(!dir.join("ran").exists(), "{command}");
            let Outcome::Failed(what) = &fired.records[0].outcome else {
                panic!("{command}: {fired:?}");
            };
            assert!(what.starts_with(CHANGED) && what.contains(runs), "{what}");
            assert!(fired.blocked.is_some(), "{command}: {fired:?}");
        }
    }

    #[test]
    fn a_project_interpreter_may_not_be_given_a_path_outside_the_project() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        write(&root.path().join("outside/y.py"), "pass\n");
        write(&repo.join("x.py"), "pass\n");
        for (command, refused) in [
            (
                r#"["python3", "x.py", "../outside/y.py"]"#,
                Some("../outside/y.py"),
            ),
            (
                r#"["node", "--require=/opt/x.js", "a.js"]"#,
                Some("/opt/x.js"),
            ),
            (r#"["python3.12", "/opt/x.py"]"#, Some("/opt/x.py")),
            (r#"["sh", "-c", "echo /opt/x"]"#, None),
            (
                r#"["python3", "./x.py", "--format=text", "plain words"]"#,
                None,
            ),
            // Not an interpreter: only `command[0]` is looked at.
            (r#"["notify-send", "/opt/x"]"#, None),
        ] {
            write(
                &repo.join(super::super::PROJECT_FILE),
                &format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = {command}\n"),
            );
            let found = find(Some(&home), &repo, &repo).unwrap();
            match refused {
                Some(path) => {
                    let warning = found.warnings.join("\n");
                    assert!(
                        found.untrusted.is_none()
                            && warning.contains("gives the interpreter")
                            && warning.contains(path),
                        "{command}: {warning}"
                    );
                }
                None => assert!(found.untrusted.is_some(), "{command}: {:?}", found.warnings),
            }
        }
    }

    #[test]
    fn project_file_arguments_use_the_same_paths_in_each_option_form() {
        use super::super::{file_words, project_file};
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let outside = root.path().join("data/settings.txt");
        write(&outside, "sample settings\n");
        write(&repo.join("data/settings.txt"), "sample settings\n");
        write(&repo.join("check.py"), "pass\n");
        let local = "./data/settings.txt";
        for (value, inside) in [(local, true), (outside.to_str().unwrap(), false)] {
            for word in [
                value.to_string(),
                format!("--file={value}"),
                format!("-F{value}"),
            ] {
                let command = ["python3".to_string(), "check.py".to_string(), word];
                assert_eq!(
                    file_words(&command).collect::<Vec<_>>(),
                    [(1, "check.py", true), (2, value, false)]
                );
                write(
                    &repo.join(super::super::PROJECT_FILE),
                    &format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = {command:?}\n"),
                );
                let file = project_file(&repo, None, None).unwrap().unwrap();
                if inside {
                    assert!(file.hooks.is_ok());
                    assert_eq!(file.pins[0].len(), 2);
                    assert_eq!(
                        file.pins[0][1].path,
                        repo.canonicalize().unwrap().join("data/settings.txt")
                    );
                } else {
                    assert!(
                        file.hooks
                            .unwrap_err()
                            .contains("outside the project folder")
                    );
                    assert!(file.pins.is_empty());
                }
            }
        }
    }

    #[test]
    fn a_missing_project_input_requires_a_new_approval_after_it_is_created() {
        use super::super::{check_project_pins, project_file};
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        write(&repo.join("check.py"), "pass\n");
        write(
            &repo.join(super::super::PROJECT_FILE),
            concat!(
                "[[hooks]]\nevent = \"pre_tool\"\n",
                "command = [\"python3\", \"check.py\", \"--file=./data/settings.txt\", \"plain words\"]\n"
            ),
        );
        let read = || project_file(&repo, None, Some(&home)).unwrap().unwrap();
        let missing = read();
        let error = missing.hooks.as_ref().unwrap_err();
        assert!(
            error.contains("data/settings.txt") && error.contains("does not exist"),
            "{error}"
        );
        assert!(trust::trust(&home, &missing).is_err());
        let data = repo.join("data/settings.txt");
        write(&data, "sample settings\n");
        let available = read();
        assert!(!trust::is_trusted(&home, &available).unwrap());
        assert_ne!(missing.digest, available.digest);
        trust::trust(&home, &available).unwrap();
        let hook = available.hooks().remove(0);
        assert!(check_project_pins(&hook, &repo).is_ok());
        std::fs::remove_file(&data).unwrap();
        assert!(
            check_project_pins(&hook, &repo)
                .unwrap_err()
                .contains("does not exist")
        );
        // A parent that is a file is an inspection error, not absence.
        std::fs::remove_dir(repo.join("data")).unwrap();
        write(&repo.join("data"), "not a directory\n");
        let error = read().hooks.unwrap_err();
        assert!(error.contains("cannot be inspected"), "{error}");
    }

    #[tokio::test]
    async fn an_interpreters_changed_script_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        write(&dir.join("guard.sh"), "exit 0\n");
        let hooks = pinned_hooks(
            &dir,
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"sh\", \"guard.sh\"]\n",
        );
        assert_eq!(hooks.hooks()[0].pins[0].arg, 1);
        let input = json!({});
        let call = || Input {
            tool: Some(("bash", &input)),
            ..Input::default()
        };
        let fired = hooks.fire(Event::PreTool, call()).await;
        assert_eq!(fired.records[0].outcome, Outcome::Passed, "{fired:?}");
        write(&dir.join("guard.sh"), "touch ran\n");
        let fired = hooks.fire(Event::PreTool, call()).await;
        assert!(!dir.join("ran").exists(), "the changed script ran");
        assert!(fired.blocked.is_some(), "{fired:?}");
    }

    #[test]
    fn a_changed_project_script_run_by_an_interpreter_asks_again() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let home = root.path().join("state");
        write(
            &repo.join(super::super::PROJECT_FILE),
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"python3\", \"scripts/check.py\"]\n",
        );
        write(&repo.join("scripts/check.py"), "pass\n");
        let mut found = find(Some(&home), &repo, &repo).unwrap();
        let question = super::super::trust_question(found.untrusted.as_ref().unwrap());
        assert!(
            question.contains("python3 scripts/check.py (pinned ")
                && question.contains("check.py sha256"),
            "{question}"
        );
        found.trust_untrusted(&home).unwrap();
        assert!(find(Some(&home), &repo, &repo).unwrap().untrusted.is_none());
        write(&repo.join("scripts/check.py"), "import os\n");
        let found = find(Some(&home), &repo, &repo).unwrap();
        assert!(found.untrusted.is_some() && found.hooks.is_empty());
    }

    /// What a hook script reports about itself: its `$0`, the
    /// `JUNEES_HOOK_FILE` it got, its folder and that folder's mode, and a
    /// file it found next to its original.
    const SHOW: &str = "#!/bin/sh\n\
        printf '%s\\n' \"$0\" \"${JUNEES_HOOK_FILE:-none}\" \"$(pwd)\" \\\n\
          \"$(stat -c %a \"$(dirname \"$0\")\")\" \\\n\
          \"$(cat \"$(dirname \"${JUNEES_HOOK_FILE:-$0}\")/sibling.txt\")\" > shown.txt\n";

    #[tokio::test]
    async fn a_pinned_file_runs_from_a_private_copy_removed_after_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let work = dir.join("work");
        let copies = dir.join("state/hook-copies");
        write(&work.join("bin/show.sh"), SHOW);
        write(&work.join("bin/sibling.txt"), "found it");
        let input = json!({});
        for (command, original) in [
            (r#"["./bin/show.sh"]"#, "bin/show.sh"),
            (r#"["sh", "bin/show.sh"]"#, "bin/show.sh"),
        ] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                work.join("bin/show.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            let hooks = pinned_hooks(
                &work,
                &format!("[[hooks]]\nevent = \"pre_tool\"\ncommand = {command}\n"),
            )
            .with_copies(copies.clone());
            let fired = hooks
                .fire(
                    Event::PreTool,
                    Input {
                        tool: Some(("bash", &input)),
                        ..Input::default()
                    },
                )
                .await;
            assert_eq!(fired.records[0].outcome, Outcome::Passed, "{fired:?}");
            let shown = std::fs::read_to_string(work.join("shown.txt")).unwrap();
            let lines: Vec<&str> = shown.lines().collect();
            // It ran from a copy in a 0700 folder under the copies folder,
            // in the session's folder, told where its original is.
            assert!(
                lines[0].starts_with(copies.to_str().unwrap()),
                "{command}: {shown}"
            );
            assert!(lines[0].ends_with("/show.sh"), "{command}: {shown}");
            assert_eq!(lines[1], work.join(original).to_str().unwrap(), "{command}");
            assert_eq!(lines[2], work.to_str().unwrap(), "{command}");
            assert_eq!(lines[3], "700", "{command}");
            assert_eq!(lines[4], "found it", "{command}");
            // Nothing is left behind.
            assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 0, "{command}");
        }
        // A hook that runs no pinned file gets no JUNEES_HOOK_FILE.
        let hooks = pinned_hooks(
            &work,
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"/bin/sh\", \"-c\", \"echo ${JUNEES_HOOK_FILE:-none} > shown.txt\"]\n",
        );
        let fired = hooks.fire(Event::TurnEnd, Input::default()).await;
        assert_eq!(fired.records[0].outcome, Outcome::Passed, "{fired:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("shown.txt")).unwrap(),
            "none\n"
        );
    }

    #[tokio::test]
    async fn the_copy_holds_the_checked_bytes_and_keeps_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        write(&dir.join("x.sh"), "checked\n");
        let hooks = pinned_hooks(
            &dir,
            "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"./x.sh\"]\n",
        );
        let hook = &hooks.hooks()[0];
        // The file changes after the check: the copy is what was checked.
        write(&dir.join("x.sh"), "swapped\n");
        let copies =
            super::super::run::Copies::make(&dir.join("copies"), hook, vec![b"checked\n".to_vec()])
                .unwrap();
        let argv = copies.argv(&hook.spec.command);
        assert_eq!(std::fs::read(&argv[0]).unwrap(), b"checked\n");
        // Not runnable: the original had no `x` bit.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&argv[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let run_dir = copies.dir.clone().unwrap();
        drop(copies);
        assert!(!run_dir.exists());
    }

    #[tokio::test]
    async fn a_copies_folder_that_is_a_link_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        write(&dir.join("x.sh"), "touch ran\n");
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("copies")).unwrap();
        let hooks = pinned_hooks(
            &dir,
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"sh\", \"x.sh\"]\n",
        )
        .with_copies(dir.join("copies"));
        let fired = hooks.fire(Event::TurnEnd, Input::default()).await;
        assert!(!dir.join("ran").exists());
        let Outcome::Failed(what) = &fired.records[0].outcome else {
            panic!("{fired:?}");
        };
        assert!(
            what.contains("private copy") && what.contains("link"),
            "{what}"
        );
        assert_eq!(std::fs::read_dir(dir.join("elsewhere")).unwrap().count(), 0);
    }

    /// Hooks of `toml` in `dir` with their files pinned as a run pins them.
    fn pinned_hooks(dir: &std::path::Path, toml: &str) -> Hooks {
        let hooks = config::parse(toml, 1)
            .unwrap()
            .into_iter()
            .map(|spec| {
                let pins = user_pins(&spec, dir);
                Hook {
                    spec,
                    origin: Origin::User(dir.join("hooks.toml")),
                    pins,
                }
            })
            .collect();
        let mut hooks = Hooks::new(hooks, dir, &[], None, Arc::new(|_: &mut String| {}));
        hooks.test_dir = Some(dir.to_path_buf());
        hooks
    }

    #[tokio::test]
    async fn a_script_changed_during_the_session_does_not_run_and_blocks_whatever_on_error_says() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let input = json!({});
        let call = || Input {
            tool: Some(("bash", &input)),
            ..Input::default()
        };
        for on_error in ["allow", "block"] {
            write(&dir.join("guard.sh"), "#!/bin/sh\nexit 0\n");
            let hooks = pinned_hooks(
                &dir,
                &format!(
                    "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"./guard.sh\"]\n\
                     on_error = \"{on_error}\"\n"
                ),
            );
            assert!(!hooks.hooks()[0].pins.is_empty());
            // The model rewrites the script after the hooks were read.
            write(&dir.join("guard.sh"), "#!/bin/sh\ntouch ran\nexit 0\n");
            let fired = hooks.fire(Event::PreTool, call()).await;
            assert!(!dir.join("ran").exists(), "the changed script ran");
            let Outcome::Failed(what) = &fired.records[0].outcome else {
                panic!("{fired:?}");
            };
            assert!(what.starts_with(CHANGED), "{what}");
            assert_eq!(
                fired.notes[0]
                    .split(": ")
                    .nth(1)
                    .unwrap()
                    .split(" (")
                    .next(),
                Some(format!("실패, {CHANGED}, 확인하지 못한 파일이라 막음").as_str()),
                "{:?}",
                fired.notes
            );
            // A changed policy script is a sign of tampering: it blocks
            // even with the default on_error.
            let blocked = fired.blocked.as_deref().unwrap_or_default();
            assert!(
                blocked.contains("whatever its on_error says"),
                "{on_error}: {fired:?}"
            );
            assert!(fired.warnings.is_empty(), "{on_error}: {fired:?}");
            assert_eq!(fired.records[0].json()["blocked"], true);
        }
        // A point that cannot block only warns.
        write(&dir.join("after.sh"), "#!/bin/sh\nexit 0\n");
        let hooks = pinned_hooks(
            &dir,
            "[[hooks]]\nevent = \"post_tool\"\ncommand = [\"./after.sh\"]\n",
        );
        write(&dir.join("after.sh"), "#!/bin/sh\ntouch ran\n");
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
        assert!(!dir.join("ran").exists());
        assert!(fired.blocked.is_none(), "{fired:?}");
        assert!(fired.warnings[0].contains(CHANGED), "{:?}", fired.warnings);
    }

    #[tokio::test]
    async fn a_file_that_could_not_be_pinned_blocks_a_prompt_hook() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        // Missing when the hooks were read; the model writes it later.
        let hooks = pinned_hooks(
            &dir,
            "[[hooks]]\nevent = \"user_prompt\"\ncommand = [\"./late.sh\"]\n",
        );
        assert!(hooks.hooks()[0].pins[0].digest.is_err());
        write(&dir.join("late.sh"), "#!/bin/sh\ntouch ran\n");
        let fired = hooks
            .fire(
                Event::UserPrompt,
                Input {
                    prompt: Some("hi"),
                    ..Input::default()
                },
            )
            .await;
        assert!(!dir.join("ran").exists());
        assert!(fired.blocked.is_some(), "{fired:?}");
        assert!(
            fired.notes[0].contains(&format!("실패, {NOT_PINNED}, 확인하지 못한 파일이라 막음")),
            "{:?}",
            fired.notes
        );
    }

    #[tokio::test]
    async fn an_absolute_path_outside_the_working_folder_runs_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = pinned_hooks(
            dir.path(),
            "[[hooks]]\nevent = \"turn_end\"\ncommand = [\"/bin/sh\", \"-c\", \"touch ran\"]\n",
        );
        assert!(hooks.hooks()[0].pins.is_empty());
        let fired = hooks.fire(Event::TurnEnd, Input::default()).await;
        assert_eq!(fired.records[0].outcome, Outcome::Passed, "{fired:?}");
        assert!(dir.path().join("ran").exists());
    }
}
