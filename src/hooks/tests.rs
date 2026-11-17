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
    let text = "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"./check.sh\"]\n";
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
