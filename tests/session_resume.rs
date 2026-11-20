//! Exercises the real CLI's saved-session paths against a fake API,
//! including resuming completed and interrupted conversations.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use junees::message::{ContentBlock, Message, Role};
use junees::session::{Entry, Journal, Session, SessionWriter};
use serde_json::{Value, json};
use tokio::time::timeout;

mod common;
use common::{body_of, fake_api, text_reply};

/// The binary in `cwd`, talking to the fake API, with no state folder set.
fn command(cwd: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, cwd);
    command
        .args(args)
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
async fn resume_discards_only_the_torn_journal_tail() {
    use std::io::Write;

    const WAIT: Duration = Duration::from_secs(30);
    const FIRST_INPUT: &str = "The notebook is blue.";
    const FIRST_REPLY: &str = "I noted the blue notebook.";
    const NEXT_INPUT: &str = "The pencil is green.";
    const NEXT_REPLY: &str = "I noted the green pencil.";

    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let (url, server) = fake_api(vec![
        (200, text_reply(FIRST_REPLY)),
        (200, text_reply(NEXT_REPLY)),
    ])
    .await;
    let run = |args: &[&str]| {
        let mut cli = common::junees_binary(&project, home.path());
        cli.env_clear()
            .env("HOME", home.path())
            .env("JUNEES_HOME", home.path())
            .env("ANTHROPIC_API_KEY", "sk-fake")
            .env("ANTHROPIC_BASE_URL", &url)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        async move {
            let output = timeout(WAIT, cli.output())
                .await
                .expect("junees did not exit")
                .unwrap();
            assert!(output.status.success(), "{}", stderr(&output));
            output
        }
    };

    let first = run(&["-p", FIRST_INPUT]).await;
    assert_eq!(
        String::from_utf8_lossy(&first.stdout),
        format!("{FIRST_REPLY}\n")
    );
    let id = only_session_id(home.path());
    let journal = home.path().join("sessions").join(format!("{id}.jsonl"));
    let before = std::fs::read(&journal).unwrap();
    assert!(before.ends_with(b"\n"));

    // Simulate a plain-text message write cut short, with no final newline.
    let tail = br#"{"type":"message","role":"user","content":[{"type":"text","text":"The spare notebook is"#;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(tail)
        .unwrap();

    let resumed = run(&["--resume", &id, "-p", NEXT_INPUT]).await;
    assert_eq!(
        String::from_utf8_lossy(&resumed.stdout),
        format!("{NEXT_REPLY}\n")
    );
    assert_eq!(only_session_id(home.path()), id);
    let after = std::fs::read(&journal).unwrap();
    let appended = after
        .strip_prefix(before.as_slice())
        .expect("resume changed bytes in the original journal");
    assert!(!appended.windows(tail.len()).any(|bytes| bytes == tail));

    let assistant = |text: &str| Message {
        role: Role::Assistant,
        content: vec![ContentBlock::text(text)],
    };
    // Parse each new line directly so the assertion cannot repair the journal.
    let saved: Vec<_> = appended
        .strip_suffix(b"\n")
        .expect("the resumed journal must end with a complete line")
        .split(|&byte| byte == b'\n')
        .filter_map(
            |line| match serde_json::from_slice::<Entry>(line).unwrap() {
                Entry::Message(message) => Some(message),
                _ => None,
            },
        )
        .collect();
    assert_eq!(
        saved,
        [Message::user_text(NEXT_INPUT), assistant(NEXT_REPLY)]
    );

    let requests = timeout(WAIT, server)
        .await
        .expect("fake API did not receive both requests")
        .unwrap();
    let [first, resumed] = requests.as_slice() else {
        panic!("expected two requests, got {}", requests.len());
    };
    let messages = |request: &str| {
        serde_json::from_value::<Vec<Message>>(body_of(request)["messages"].take()).unwrap()
    };
    assert_eq!(messages(first), [Message::user_text(FIRST_INPUT)]);
    assert_eq!(
        messages(resumed),
        [
            Message::user_text(FIRST_INPUT),
            assistant(FIRST_REPLY),
            Message::user_text(NEXT_INPUT),
        ]
    );
}

#[tokio::test]
async fn resume_recovers_an_interrupted_tool_call_once() {
    const TOOL_ID: &str = "toolu_interrupted_read";
    const REASON: &str = "interrupted: Junees stopped before this tool finished";
    const WAIT: Duration = Duration::from_secs(30);
    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("notes.txt"), "Local fixture notes.\n").unwrap();

    // Leave a normal journal ending at the request, without running the tool.
    let (meta, mut writer) = SessionWriter::create(home.path(), &project, "test-model").unwrap();
    let journal = writer.path().to_path_buf();
    writer
        .record(&Entry::Message(Message::user_text("Read the notes.")))
        .unwrap();
    writer
        .record(&Entry::Message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: TOOL_ID.into(),
                name: "read_file".into(),
                input: json!({"path": "notes.txt"}),
            }],
        }))
        .unwrap();
    drop(writer);

    let turns = [
        ("What happened to the read?", "The read was interrupted."),
        ("Can we continue?", "Yes, we can continue."),
    ];
    let (url, server) = fake_api(
        turns
            .iter()
            .map(|(_, reply)| (200, text_reply(reply)))
            .collect(),
    )
    .await;
    for (prompt, reply) in turns {
        let mut cli = common::junees_binary(&project, home.path());
        cli.env_clear()
            .env("HOME", home.path())
            .env("JUNEES_HOME", home.path())
            .env("ANTHROPIC_API_KEY", "sk-fake")
            .env("ANTHROPIC_BASE_URL", &url)
            .args(["--resume", &meta.id, "-p", prompt])
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = timeout(WAIT, cli.output())
            .await
            .expect("junees --resume did not exit")
            .unwrap();
        assert!(output.status.success(), "{prompt}: {}", stderr(&output));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{reply}\n")
        );

        // Read without repairing: the first run must save the result and
        // the second must leave exactly that one result in the same journal.
        let saved = Session::load(&journal).unwrap();
        let results: Vec<_> = saved
            .transcript
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } if tool_use_id == TOOL_ID => Some((content.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(results, [(REASON, true)], "{prompt}: saved recovery");
    }
    assert_eq!(only_session_id(home.path()), meta.id);

    let requests = timeout(WAIT, server)
        .await
        .expect("fake API did not receive both resume requests")
        .unwrap();
    assert_eq!(requests.len(), turns.len());
    for (request, (prompt, _)) in requests.iter().zip(turns) {
        let body = body_of(request);
        let messages = body["messages"].as_array().unwrap();
        let call = &messages[1]["content"][0];
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(call["type"], "tool_use");
        assert_eq!(call["id"], TOOL_ID);

        let count = messages
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter(|block| block["type"] == "tool_result" && block["tool_use_id"] == TOOL_ID)
            .count();
        assert_eq!(count, 1, "{prompt}: expected one recovery in {messages:?}");
        assert_eq!(messages[2]["role"], "user");
        let result = &messages[2]["content"][0];
        assert_eq!(
            result["type"], "tool_result",
            "{prompt}: result must follow the call"
        );
        assert_eq!(result["tool_use_id"], TOOL_ID);
        assert_eq!(result["is_error"], true);
        assert_eq!(result["content"], REASON);

        // The prompt may share the recovery's user message, but must come after it.
        let latest = messages.last().unwrap();
        assert_eq!(latest["role"], "user");
        let last_block = latest["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last_block["type"], "text");
        assert_eq!(last_block["text"], prompt);
    }
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
