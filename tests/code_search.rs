//! The code search tools in a real run: the model greps, reads what it
//! found and edits it, on a fake API (loopback).

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use common::{body_of, fake_api, text_reply};

const WAIT: Duration = Duration::from_secs(60);

fn tool_call(id: &str, name: &str, input: Value) -> Value {
    json!({
        "id": format!("msg_{id}"), "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

/// The output of the tool result in the last message of `request`.
fn result_of(request: &str) -> String {
    let body = body_of(request);
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    let block = last["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_result")
        .unwrap()
        .clone();
    match &block["content"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[tokio::test]
async fn the_model_greps_reads_and_edits() {
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = dir.path().canonicalize().unwrap();
    let work = root.join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::write(work.join("src/lib.rs"), "fn old_name() {}\n").unwrap();
    std::fs::write(work.join("README.md"), "nothing\n").unwrap();
    std::fs::write(work.join(".gitignore"), "build/\n").unwrap();
    std::fs::create_dir(work.join("build")).unwrap();
    std::fs::write(work.join("build/lib.rs"), "fn old_name() {}\n").unwrap();

    let (url, server) = fake_api(vec![
        (
            200,
            tool_call(
                "t1",
                "grep",
                json!({"pattern": "old_name", "output": "content"}),
            ),
        ),
        (
            200,
            tool_call("t2", "read_file", json!({"path": "src/lib.rs"})),
        ),
        (
            200,
            tool_call(
                "t3",
                "edit_file",
                json!({"path": "src/lib.rs", "old_string": "old_name", "new_string": "new_name"}),
            ),
        ),
        (200, tool_call("t4", "list_dir", json!({}))),
        (200, tool_call("t5", "glob", json!({"pattern": "**/*.rs"}))),
        (200, text_reply("renamed")),
    ])
    .await;
    let mut child = common::junees_binary(&work, &root)
        .args(["-p", "rename old_name"])
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", root.join(".junees"))
        .env("JUNEES_STREAM", "0")
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().shutdown().await.unwrap();
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("junees did not end")
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{said}");
    let requests = tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    assert_eq!(requests.len(), 6, "{said}");

    // The tools are offered, and the system prompt points to them.
    let first = body_of(&requests[0]);
    let offered: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for tool in ["grep", "glob", "list_dir"] {
        assert!(offered.contains(&tool), "{offered:?}");
    }
    assert!(
        first["system"]
            .to_string()
            .contains("use the grep, glob and list_dir tools")
    );
    // grep: relative path, line number, text; the ignored copy left out.
    assert_eq!(result_of(&requests[1]), "src/lib.rs:1:fn old_name() {}\n");
    assert_eq!(result_of(&requests[2]), "     1\tfn old_name() {}\n");
    assert!(result_of(&requests[3]).contains("src/lib.rs"));
    assert_eq!(
        std::fs::read_to_string(work.join("src/lib.rs")).unwrap(),
        "fn new_name() {}\n"
    );
    let listed = result_of(&requests[4]);
    assert!(
        listed.ends_with("/work/\n  README.md (8 B)\n  src/\n"),
        "{listed}"
    );
    assert_eq!(result_of(&requests[5]), "src/lib.rs\n");
    assert_eq!(
        std::fs::read_to_string(work.join("build/lib.rs")).unwrap(),
        "fn old_name() {}\n"
    );
}
