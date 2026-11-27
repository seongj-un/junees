//! `junees sessions list` through the real binary, with synthetic
//! sessions in a temporary state folder and no API key.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

mod common;
use common::junees_binary;

/// Writes a journal by hand, last written `age` seconds ago.
fn journal(
    home: &Path,
    id: &str,
    cwd: &Path,
    source: Option<(&str, &str)>,
    backend: Option<&str>,
    first: &str,
    age: u64,
) {
    let dir = home.join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut meta = json!({
        "type": "meta", "id": id, "format": 9, "created_at": 1_790_000_000u64,
        "cwd": cwd, "model": "m", "version": "0", "ts": 1_790_000_000u64,
    });
    if let Some((source, channel)) = source {
        meta["source"] = json!(source);
        meta["channel"] = json!(channel);
    }
    if let Some(backend) = backend {
        meta["backend"] = json!(backend);
    }
    let mut lines = vec![meta.to_string()];
    if !first.is_empty() {
        lines.push(
            json!({"type": "message", "role": "user", "content": [{"type": "text", "text": first}]})
                .to_string(),
        );
        lines.push(
            json!({"type": "message", "role": "assistant", "content": [{"type": "text", "text": "답변 본문 SECRET-BODY"}]})
                .to_string(),
        );
    }
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(age))
        .unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    work: PathBuf,
    elsewhere: PathBuf,
}

/// Terminal sessions in `work` (newest `…-new`, a claude-code one, a
/// Korean long one), one in another folder, one empty journal, and a
/// Discord and a cron session in `work`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("junees");
    let work = dir.path().join("work");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let work = work.canonicalize().unwrap();
    let elsewhere = elsewhere.canonicalize().unwrap();
    let long = "가".repeat(100);
    journal(
        &home,
        "20261101-000001-old",
        &work,
        None,
        None,
        "오래된 입력\n둘째 줄",
        500,
    );
    journal(
        &home,
        "20261101-000002-cc",
        &work,
        None,
        Some("claude-code"),
        "claude 세션",
        400,
    );
    journal(&home, "20261101-000003-long", &work, None, None, &long, 300);
    journal(
        &home,
        "20261101-000004-other",
        &elsewhere,
        None,
        None,
        "다른 폴더 입력",
        250,
    );
    journal(
        &home,
        "20261101-000005-discord",
        &work,
        Some(("discord", "4242")),
        None,
        "디스코드 입력",
        200,
    );
    journal(
        &home,
        "20261101-000006-cron",
        &work,
        Some(("cron", "job1")),
        None,
        "예약 입력",
        150,
    );
    journal(&home, "20261101-000007-empty", &work, None, None, "", 100);
    journal(
        &home,
        "20261101-000008-new",
        &work,
        None,
        None,
        "가장 새 입력",
        50,
    );
    Fixture {
        _dir: dir,
        home,
        work,
        elsewhere,
    }
}

struct Ran {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

async fn run(fixture: &Fixture, cwd: &Path, args: &[&str]) -> Ran {
    let out = junees_binary(cwd, fixture._dir.path())
        .args(args)
        .env("JUNEES_HOME", &fixture.home)
        .output()
        .await
        .unwrap();
    Ran {
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        code: out.status.code(),
    }
}

fn ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| line.starts_with("2026"))
        .map(|line| line.split("  ").next().unwrap().to_string())
        .collect()
}

fn json_ids(value: &Value) -> Vec<String> {
    value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_this_folders_terminal_sessions_newest_first() {
    let f = fixture();
    let out = run(&f, &f.work, &["sessions", "list"]).await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    assert_eq!(
        ids(&out.stdout),
        [
            "20261101-000008-new",
            "20261101-000007-empty",
            "20261101-000003-long",
            "20261101-000002-cc",
            "20261101-000001-old",
        ]
    );
    // Channel sessions, other folders and no folder column by default.
    assert!(!out.stdout.contains("discord") && !out.stdout.contains("예약"));
    assert!(!out.stdout.contains("다른 폴더"));
    assert!(!out.stdout.contains(f.work.to_str().unwrap()));
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines[0].contains(" UTC  가장 새 입력"), "{}", lines[0]);
    assert!(lines[1].ends_with("  (empty)"), "{}", lines[1]);
    assert!(
        lines[3].contains("UTC  [claude-code]  claude 세션"),
        "{}",
        lines[3]
    );
    // First line only; text of the reply is never shown.
    assert!(lines[4].ends_with("  오래된 입력"), "{}", lines[4]);
    assert!(!out.stdout.contains("SECRET-BODY"));
    // Korean text is cut on character boundaries at 60 characters.
    assert!(
        lines[2].ends_with(&format!("  {}...", "가".repeat(57))),
        "{}",
        lines[2]
    );
    assert_eq!(
        *lines.last().unwrap(),
        "To continue a terminal session, run `junees --resume <id>`."
    );
}

#[tokio::test]
async fn all_limit_and_source_choose_what_is_listed() {
    let f = fixture();
    let out = run(&f, &f.work, &["sessions", "list", "--all", "--limit", "3"]).await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(
        ids(&out.stdout),
        [
            "20261101-000008-new",
            "20261101-000007-empty",
            "20261101-000004-other"
        ]
    );
    assert!(out.stdout.contains(f.work.to_str().unwrap()));
    assert!(out.stdout.contains(f.elsewhere.to_str().unwrap()));

    let out = run(&f, &f.work, &["sessions", "list", "--all"]).await;
    assert!(ids(&out.stdout).contains(&"20261101-000004-other".to_string()));
    assert!(out.stdout.contains(f.elsewhere.to_str().unwrap()));

    let out = run(
        &f,
        &f.work,
        &["sessions", "list", "--source", "discord,cron"],
    )
    .await;
    assert_eq!(
        ids(&out.stdout),
        ["20261101-000006-cron", "20261101-000005-discord"]
    );
    assert!(out.stdout.contains("[cron]") && out.stdout.contains("[discord]"));
    assert!(!out.stdout.contains("4242") && !out.stdout.contains("job1"));

    let out = run(
        &f,
        &f.work,
        &["sessions", "list", "--source", "all", "--all"],
    )
    .await;
    assert_eq!(ids(&out.stdout).len(), 8);

    // From another folder the work sessions are not listed.
    let out = run(&f, &f.elsewhere, &["sessions", "list"]).await;
    assert_eq!(ids(&out.stdout), ["20261101-000004-other"]);
}

#[tokio::test]
async fn limit_is_one_to_fifty() {
    let f = fixture();
    for (value, ok) in [
        ("1", true),
        ("50", true),
        ("0", false),
        ("51", false),
        ("x", false),
        ("-1", false),
    ] {
        let out = run(&f, &f.work, &["sessions", "list", "--limit", value]).await;
        if ok {
            assert_eq!(out.code, Some(0), "{value}: {}", out.stderr);
        } else {
            assert_eq!(out.code, Some(1), "{value}");
            assert_eq!(out.stdout, "", "{value}");
            assert!(
                out.stderr
                    .contains("--limit must be a whole number from 1 to 50"),
                "{value}: {}",
                out.stderr
            );
        }
    }
    let out = run(&f, &f.work, &["sessions", "list", "--limit", "1"]).await;
    assert_eq!(ids(&out.stdout), ["20261101-000008-new"]);
}

#[tokio::test]
async fn bad_arguments_are_refused() {
    let f = fixture();
    for (args, expected) in [
        (
            &["sessions", "list", "--source", "irc"][..],
            "unknown session source `irc`",
        ),
        (&["sessions", "list", "words"], "takes no words"),
        (&["sessions", "list", "--bogus"], "unknown option `--bogus`"),
        (
            &["sessions", "show"],
            "unknown sessions command `show`; use `list` or `search`",
        ),
    ] {
        let out = run(&f, &f.work, args).await;
        assert_eq!(out.code, Some(1), "{args:?}");
        assert_eq!(out.stdout, "", "{args:?}");
        assert!(out.stderr.contains(expected), "{args:?}: {}", out.stderr);
    }
}

#[tokio::test]
async fn an_empty_list_is_one_line_and_success() {
    let f = fixture();
    let empty = tempfile::tempdir().unwrap();
    let work = empty.path().canonicalize().unwrap();
    let out = run(&f, &work, &["sessions", "list"]).await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(
        out.stdout,
        format!(
            "No saved sessions in this folder ({}) from terminal. --all lists every folder.\n",
            work.display()
        )
    );
    let out = run(&f, &work, &["sessions", "list", "--json"]).await;
    assert_eq!(out.code, Some(0));
    let value: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(value["sessions"], json!([]));
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["scope"], json!({"all": false, "folder": work}));

    // No sessions folder at all.
    let home = tempfile::tempdir().unwrap();
    let out = junees_binary(&work, home.path())
        .args(["sessions", "list", "--all"])
        .env("JUNEES_HOME", home.path().join("none"))
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "No saved sessions in every folder from terminal.\n"
    );
}

#[tokio::test]
async fn json_is_one_object_and_nothing_else() {
    let f = fixture();
    let out = run(&f, &f.work, &["sessions", "list", "--json", "--limit", "4"]).await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert_eq!(out.stderr, "");
    assert!(out.stdout.ends_with("}\n") && out.stdout.matches('\n').count() == 1);
    let value: Value = serde_json::from_str(&out.stdout).unwrap();
    let mut keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["schema_version", "scope", "sessions"]);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["scope"], json!({"all": false, "folder": f.work}));
    assert_eq!(
        json_ids(&value),
        [
            "20261101-000008-new",
            "20261101-000007-empty",
            "20261101-000003-long",
            "20261101-000002-cc"
        ]
    );
    let first = &value["sessions"][0];
    let mut fields: Vec<&String> = first.as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(
        fields,
        ["backend", "cwd", "id", "source", "title", "updated_at"]
    );
    assert_eq!(first["cwd"], json!(f.work));
    assert_eq!(first["backend"], "api");
    assert_eq!(first["source"], "terminal");
    assert_eq!(first["title"], "가장 새 입력");
    let stamp = first["updated_at"].as_str().unwrap();
    assert!(
        stamp.len() == 20 && stamp.ends_with('Z') && &stamp[10..11] == "T",
        "{stamp}"
    );
    assert_eq!(value["sessions"][1]["title"], Value::Null);
    assert_eq!(value["sessions"][3]["backend"], "claude-code");
    // The title is the dashboard's: 80 characters, not 60.
    assert_eq!(
        value["sessions"][2]["title"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        80
    );
    // No journal body, tool output or channel id.
    assert!(!out.stdout.contains("SECRET-BODY"));

    let all = run(
        &f,
        &f.work,
        &["sessions", "list", "--json", "--all", "--source", "all"],
    )
    .await;
    let value: Value = serde_json::from_str(&all.stdout).unwrap();
    assert_eq!(value["scope"], json!({"all": true, "folder": null}));
    assert_eq!(json_ids(&value).len(), 8);
    let discord = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "20261101-000005-discord")
        .unwrap();
    assert_eq!(discord["source"], "discord");
    assert!(!all.stdout.contains("4242"));
}

#[tokio::test]
async fn a_secret_in_the_first_input_is_not_shown() {
    let f = fixture();
    let key = format!("sk-ant-api03-{}", "aB3dE6gH9".repeat(6));
    journal(
        &f.home,
        "20261101-000009-key",
        &f.work,
        None,
        None,
        &format!("키 {key} 로 해 줘"),
        10,
    );
    for args in [&["sessions", "list"][..], &["sessions", "list", "--json"]] {
        let out = run(&f, &f.work, args).await;
        assert_eq!(out.code, Some(0));
        assert!(!out.stdout.contains(&key[..16]), "{}", out.stdout);
        assert!(out.stdout.contains("20261101-000009-key"));
    }
}

#[tokio::test]
async fn journals_are_left_unchanged_and_links_are_not_followed() {
    let f = fixture();
    let dir = f.home.join("sessions");
    // A link to a journal outside the sessions folder is not a session.
    let outside = f._dir.path().join("outside.jsonl");
    std::fs::write(
        &outside,
        json!({
            "type": "meta", "id": "20261101-000099-link", "format": 9, "created_at": 1u64,
            "cwd": f.work, "model": "m", "version": "0", "ts": 1u64,
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("20261101-000099-link.jsonl")).unwrap();
    let snapshot = |dir: &Path| {
        let mut files: Vec<(String, Vec<u8>, SystemTime)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let path = e.path();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read(&path).unwrap(),
                    std::fs::metadata(&path).unwrap().modified().unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    };
    let before = snapshot(&dir);
    let out = run(
        &f,
        &f.work,
        &["sessions", "list", "--source", "all", "--limit", "50"],
    )
    .await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(!out.stdout.contains("000099-link"), "{}", out.stdout);
    assert_eq!(snapshot(&dir), before);
    assert_eq!(
        std::fs::read_dir(f.home.join("sessions")).unwrap().count(),
        before.len()
    );
    assert!(!f.home.join("index").exists());
}

#[tokio::test]
async fn search_and_help_are_as_before() {
    let f = fixture();
    let out = run(&f, &f.work, &["sessions", "search", "입력", "--limit", "1"]).await;
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    assert!(
        out.stdout
            .starts_with("1 session matches `입력` in this folder"),
        "{}",
        out.stdout
    );

    for args in [
        &["sessions", "--help"][..],
        &["sessions", "list", "--help"],
        &["sessions"],
    ] {
        let out = run(&f, &f.work, args).await;
        assert_eq!(out.code, Some(0), "{args:?}");
        assert!(
            out.stdout.contains("junees sessions search [OPTIONS]"),
            "{args:?}"
        );
        assert!(
            out.stdout.contains("junees sessions list [OPTIONS]"),
            "{args:?}"
        );
        assert!(
            out.stdout
                .contains("--limit <N>      sessions to show, 1 to 50 (default 10)")
        );
    }
    let out = run(&f, &f.work, &["--help"]).await;
    assert!(
        out.stdout.contains("junees sessions list"),
        "{}",
        out.stdout
    );
}
