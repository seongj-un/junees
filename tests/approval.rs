//! Runs the real binary with tool calls that need approval. Without a
//! terminal the conversation mode declines them and says so; `-p` never
//! asks (#approval).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{body_of, fake_api, text_reply};

/// A reply calling bash once per command, with ids `toolu_0`, `toolu_1`...
fn bash_calls(commands: &[&str]) -> Value {
    let content: Vec<Value> = commands
        .iter()
        .enumerate()
        .map(|(i, command)| {
            json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": "bash",
                   "input": {"command": command}})
        })
        .collect();
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn command(home: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(args)
        .current_dir(home)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home)
        .env("HOME", home)
        .env_remove("JUNEES_APPROVAL")
        .env_remove("JUNEES_ALLOW_DANGEROUS")
        .env_remove("JUNEES_DENY_COMMANDS")
        .env_remove("JUNEES_MAX_TURNS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// The tool results of the second request, as (id, content, is_error).
fn results(request: &str) -> Vec<(String, String, bool)> {
    let messages = body_of(request)["messages"].clone();
    let last = messages.as_array().unwrap().last().unwrap().clone();
    last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            (
                block["tool_use_id"].as_str().unwrap().to_string(),
                block["content"].as_str().unwrap().to_string(),
                block["is_error"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

/// Runs the conversation mode with `input` piped to stdin.
async fn converse(home: &Path, url: &str, approval: Option<&str>, input: &str) -> String {
    let mut command = command(home, url, &[]);
    if let Some(mode) = approval {
        command.env("JUNEES_APPROVAL", mode);
    }
    let mut child = command.stdin(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "{stderr}");
    stderr
}

#[tokio::test]
async fn without_a_terminal_risky_calls_are_declined_and_others_run() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("marker"), "keep me").unwrap();
    let (url, server) = fake_api(vec![
        (200, bash_calls(&["rm marker", "touch made"])),
        (200, text_reply("Done.")),
    ])
    .await;
    let stderr = converse(home.path(), &url, None, "clean up\n").await;

    assert!(
        stderr.contains(
            "stdin is not a terminal: tool calls that need approval (JUNEES_APPROVAL=risky) \
             are declined"
        ),
        "{stderr}"
    );
    assert!(
        home.path().join("marker").exists(),
        "rm ran without approval"
    );
    assert!(
        home.path().join("made").exists(),
        "a safe command did not run"
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let results = results(&requests[1]);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0].0, "toolu_0");
    assert!(results[0].2);
    assert_eq!(
        results[0].1,
        "not run: this call needs the user's approval (JUNEES_APPROVAL=risky), but junees \
         cannot ask because its input is not a terminal"
    );
    assert_eq!(
        results[1],
        ("toolu_1".to_string(), "[exit code 0]".to_string(), false)
    );
}

#[tokio::test]
async fn approval_off_runs_risky_calls_in_the_conversation_mode() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("marker"), "remove me").unwrap();
    let (url, server) = fake_api(vec![
        (200, bash_calls(&["rm marker"])),
        (200, text_reply("Done.")),
    ])
    .await;
    let stderr = converse(home.path(), &url, Some("off"), "clean up\n").await;
    assert!(!stderr.contains("not a terminal"), "{stderr}");
    assert!(!home.path().join("marker").exists());
    let requests = server.await.unwrap();
    assert!(!results(&requests[1])[0].2);
}

#[tokio::test]
async fn a_prompt_run_never_asks() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("marker"), "remove me").unwrap();
    let (url, server) = fake_api(vec![
        (200, bash_calls(&["rm marker"])),
        (200, text_reply("Done.")),
    ])
    .await;
    let output = command(home.path(), &url, &["-p", "clean up"])
        .env("JUNEES_APPROVAL", "all")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("approval"), "{stderr}");
    assert!(
        !home.path().join("marker").exists(),
        "-p did not run the call"
    );
    let requests = server.await.unwrap();
    assert_eq!(
        results(&requests[1]),
        [("toolu_0".to_string(), "[exit code 0]".to_string(), false)]
    );
}
