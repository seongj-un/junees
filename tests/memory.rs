//! Runs the real binary against a fake API server: the memory tool saves an
//! entry that the next session (not the current one) has in its system
//! prompt, and `/memory` shows and clears the stores.

use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{body_of, fake_api, text_reply};

/// The binary in `cwd`, talking to the fake API, with `home` as its state
/// folder.
fn command(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .args(args)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null());
    command
}

fn memory_call(input: Value) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "memory", "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn system_of(request: &str) -> String {
    body_of(request)["system"].to_string()
}

fn tool_result(request: &str) -> String {
    let body = body_of(request);
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    last["content"][0]["content"].as_str().unwrap().to_string()
}

fn check(output: &Output) {
    assert!(
        output.status.success(),
        "junees failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn a_saved_entry_reaches_the_next_session() {
    let (url, server) = fake_api(vec![
        (
            200,
            memory_call(json!({"target": "user", "action": "add", "content": "Prefers tea."})),
        ),
        (200, text_reply("Noted.")),
        (200, text_reply("You like tea.")),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    for prompt in ["I like tea", "what do I like?"] {
        let output = command(home.path(), project.path(), &url, &["-p", prompt])
            .output()
            .await
            .unwrap();
        check(&output);
    }
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);

    let tools: Vec<String> = body_of(&requests[0])["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(tools.contains(&"memory".to_string()), "{tools:?}");
    let result = tool_result(&requests[1]);
    assert!(
        result.starts_with("added (user memory: 1 entries, 12/1500 characters)"),
        "{result}"
    );
    // The session that saved it keeps its prompt (and its cache) as it was.
    assert!(system_of(&requests[0]).contains("MEMORY (persistent notes)"));
    assert!(!system_of(&requests[0]).contains("Prefers tea."));
    assert_eq!(system_of(&requests[0]), system_of(&requests[1]));
    // The next one starts with it.
    assert!(system_of(&requests[2]).contains("\\nPrefers tea.\\n"));
    let saved = home.path().join("memory/user.md");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "Prefers tea.\n");
}

#[tokio::test]
async fn the_prompt_shows_clears_and_rereads_memory() {
    let (url, server) = fake_api(vec![
        (
            200,
            memory_call(json!({"target": "project", "action": "add", "content": "Use make."})),
        ),
        (200, text_reply("Saved.")),
        (200, text_reply("Hello again.")),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("memory")).unwrap();
    std::fs::write(
        home.path().join("memory/user.md"),
        "Name: 성준\n§\nKorean, please.\n",
    )
    .unwrap();

    let mut child = command(home.path(), project.path(), &url, &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            "remember make\n/new\nhello\n/memory\n/memory clear user\nn\n\
             /memory clear user\nyes\n/memory clear user\n/memory clear all\n/exit\n"
                .as_bytes(),
        )
        .await
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    check(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let user = home.path().join("memory/user.md");
    assert!(
        stdout.contains(&format!(
            "user ({}): 2 entries, 26/1500 characters\n  Name: 성준\n  §\n  Korean, please.\n",
            user.display()
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains("): 1 entries, 9/2500 characters\n  Use make.\n"),
        "{stdout}"
    );
    assert!(stdout.contains("clear user memory (2 entries)? [y/N] (user memory kept)"));
    assert!(stdout.contains(
        "clear user memory (2 entries)? [y/N] cleared user memory (2 entries); this session \
         still shows what it started with, /new starts one without it"
    ));
    assert!(
        stdout.contains("(user memory is already empty)"),
        "{stdout}"
    );
    assert!(stderr.contains("unknown memory `all`"), "{stderr}");
    assert!(!user.exists());

    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 3);
    let first = system_of(&requests[0]);
    assert!(
        first.contains("\\nName: 성준\\n§\\nKorean, please.\\n"),
        "{first}"
    );
    assert!(!first.contains("Use make."));
    // /new started a session with the note saved before it.
    assert!(system_of(&requests[2]).contains("\\nUse make.\\n"));
}
