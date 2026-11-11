//! Runs the real binary twice against a fake API server and checks that the
//! second run continues the conversation saved by the first.

use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, text_reply};

/// The binary in `cwd`, talking to the fake API, with no state folder set.
fn command(cwd: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary();
    command
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env_remove("HOME")
        .stdin(Stdio::null());
    command
}

/// Runs the binary with `home` as its state folder and expects success.
async fn junees(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> Output {
    let output = command(cwd, url, args)
        .env("JUNEES_HOME", home)
        .env("HOME", home)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "junees {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn bash_call(command: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": "Running it."},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": command}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

#[tokio::test]
async fn continue_sends_the_earlier_conversation() {
    let (url, server) = fake_api(vec![
        (200, bash_call("echo junees-session")),
        (200, text_reply("It printed junees-session.")),
        (200, text_reply("Yes, same folder.")),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let first = junees(home.path(), project.path(), &url, &["-p", "run echo"]).await;
    assert!(stderr(&first).contains("session 20"), "{}", stderr(&first));
    assert!(String::from_utf8_lossy(&first.stdout).contains("It printed junees-session."));

    let second = junees(
        home.path(),
        project.path(),
        &url,
        &["-c", "-p", "계속: same folder?"],
    )
    .await;
    assert!(
        stderr(&second).contains("(4 messages)"),
        "{}",
        stderr(&second)
    );
    assert_eq!(
        String::from_utf8_lossy(&second.stdout),
        "Yes, same folder.\n"
    );

    let requests = server.await.unwrap();
    let messages = body_of(&requests[2])["messages"].clone();
    let roles: Vec<&str> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user"]);
    assert_eq!(messages[0]["content"][0]["text"], "run echo");
    assert_eq!(messages[1]["content"][1]["id"], "toolu_1");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(
        messages[2]["content"][0]["content"],
        "junees-session\n[exit code 0]"
    );
    assert_eq!(
        messages[3]["content"][0]["text"],
        "It printed junees-session."
    );
    assert_eq!(messages[4]["content"][0]["text"], "계속: same folder?");

    // Both runs wrote to the same single session file.
    let files: Vec<_> = std::fs::read_dir(home.path().join("sessions"))
        .unwrap()
        .collect();
    assert_eq!(files.len(), 1);
}

/// The id of the only session saved under `home`.
fn only_session_id(home: &Path) -> String {
    let files: Vec<_> = std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files[0].file_stem().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn resume_takes_an_id_prefix() {
    let (url, server) = fake_api(vec![(200, text_reply("Noted.")), (200, text_reply("42."))]).await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    junees(home.path(), project.path(), &url, &["-p", "remember 42"]).await;
    let id = only_session_id(home.path());
    let prefix = &id[..id.len() - 2];
    let second = junees(
        home.path(),
        project.path(),
        &url,
        &["--resume", prefix, "-p", "which number?"],
    )
    .await;
    assert!(
        stderr(&second).contains(&format!("resumed {id} (2 messages)\n")),
        "{}",
        stderr(&second)
    );

    let requests = server.await.unwrap();
    let messages = body_of(&requests[1])["messages"].clone();
    assert_eq!(messages.as_array().unwrap().len(), 3);
    assert_eq!(messages[0]["content"][0]["text"], "remember 42");
    assert_eq!(messages[2]["content"][0]["text"], "which number?");
    assert_eq!(only_session_id(home.path()), id);
}

#[tokio::test]
async fn resuming_from_another_folder_says_where_the_session_started() {
    let (url, server) = fake_api(vec![(200, text_reply("a")), (200, text_reply("b"))]).await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();

    junees(home.path(), project.path(), &url, &["-p", "hi"]).await;
    let id = only_session_id(home.path());
    let second = junees(
        home.path(),
        other.path(),
        &url,
        &["--resume", &id, "-p", "hi"],
    )
    .await;
    let started_in = std::fs::canonicalize(project.path()).unwrap();
    assert!(
        stderr(&second).contains(&format!(
            "resumed {id} (2 messages, started in {})\n",
            started_in.display()
        )),
        "{}",
        stderr(&second)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn no_session_saves_nothing_and_continue_starts_fresh_elsewhere() {
    let (url, server) = fake_api(vec![(200, text_reply("a")), (200, text_reply("b"))]).await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    junees(
        home.path(),
        project.path(),
        &url,
        &["--no-session", "-p", "hi"],
    )
    .await;
    assert!(!home.path().join("sessions").exists());

    let fresh = junees(home.path(), project.path(), &url, &["-c", "-p", "hello"]).await;
    assert!(
        stderr(&fresh).contains("no earlier session"),
        "{}",
        stderr(&fresh)
    );

    let requests = server.await.unwrap();
    assert_eq!(
        body_of(&requests[1])["messages"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        std::fs::read_dir(home.path().join("sessions"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn without_home_it_runs_unsaved_and_only_continuing_fails() {
    let (url, server) = fake_api(vec![(200, text_reply("a")), (200, text_reply("b"))]).await;
    let project = tempfile::tempdir().unwrap();
    let run = |args: &'static [&'static str]| command(project.path(), &url, args).output();

    let off = run(&["--no-session", "-p", "hi"]).await.unwrap();
    assert!(off.status.success(), "{}", stderr(&off));

    let unsaved = run(&["-p", "hi"]).await.unwrap();
    assert!(unsaved.status.success(), "{}", stderr(&unsaved));
    assert!(
        stderr(&unsaved).contains("session not saved: neither JUNEES_HOME nor HOME is set"),
        "{}",
        stderr(&unsaved)
    );

    let resumed = run(&["-c", "-p", "hi"]).await.unwrap();
    assert!(!resumed.status.success());
    assert!(
        stderr(&resumed).contains(
            "error: cannot continue a saved session: neither JUNEES_HOME nor HOME is set"
        ),
        "{}",
        stderr(&resumed)
    );

    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn thinking_blocks_survive_a_resumed_session() {
    let thinking = json!({"type": "thinking", "thinking": "Check the folder first.",
                          "signature": "WaUjzkypQ2mUEVM36O2Txu+/=="});
    let mut call = bash_call("echo junees-thinking");
    call["content"]
        .as_array_mut()
        .unwrap()
        .insert(0, thinking.clone());
    let (url, server) = fake_api(vec![
        (200, call.clone()),
        (200, text_reply("It printed junees-thinking.")),
        (200, text_reply("Still the same.")),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let thinking_on = |args: &'static [&'static str]| {
        let mut command = command(project.path(), &url, args);
        command
            .env("JUNEES_HOME", home.path())
            .env("HOME", home.path())
            .env("JUNEES_THINKING_BUDGET", "1024")
            .env("JUNEES_MAX_TOKENS", "2048");
        command
    };

    let first = thinking_on(&["-p", "run echo"]).output().await.unwrap();
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stderr(&first).contains("(thinking...)"),
        "{}",
        stderr(&first)
    );
    assert!(
        !stderr(&first).contains("Check the folder"),
        "{}",
        stderr(&first)
    );
    let second = thinking_on(&["-c", "-p", "again?"]).output().await.unwrap();
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(String::from_utf8_lossy(&second.stdout), "Still the same.\n");

    let requests = server.await.unwrap();
    let assistant_turn = json!({"role": "assistant", "content": call["content"]});
    for (i, request) in requests.iter().enumerate() {
        let body = body_of(request);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024}),
            "request {i}"
        );
        assert_eq!(body["max_tokens"], 2048);
        if i > 0 {
            assert_eq!(body["messages"][1], assistant_turn, "request {i}");
        }
    }
    // The resumed run read it back from the journal, which keeps it as is.
    let sessions = home.path().join("sessions");
    let file = std::fs::read_dir(sessions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let journal = std::fs::read_to_string(file.path()).unwrap();
    let lines: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["format"], junees::session::FORMAT);
    assert!(lines.iter().any(|line| line["content"][0] == thinking));
}
