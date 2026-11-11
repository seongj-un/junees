//! Runs the real binary and stops it with signals while a tool runs: the
//! turn is cancelled, the tool's processes are gone, and the history keeps
//! a result for the cancelled call (#2).

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod common;
use common::{body_of, fake_api, text_reply};

/// A reply running `sleep` in the background and waiting for it, after
/// writing the sleep's pid to `pid_file`.
fn slow_bash(pid_file: &Path) -> Value {
    let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "tool_use", "id": "toolu_slow", "name": "bash", "input": {"command": command}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn command(home: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary();
    command
        .args(args)
        .current_dir(home)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home)
        .env("HOME", home)
        .kill_on_drop(true);
    command
}

/// Waits up to ten seconds for `check` to hold.
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn pid_in(pid_file: &Path) -> u32 {
    until("the tool to start", || {
        std::fs::read_to_string(pid_file).is_ok_and(|text| text.trim().parse::<u32>().is_ok())
    })
    .await;
    std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Whether the process runs (a zombie does not).
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

fn send(pid: Option<u32>, signal: i32) {
    let pid = libc::pid_t::try_from(pid.unwrap()).unwrap();
    // SAFETY: kill only sends a signal to the child this test started.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
}

/// The text of every session journal under `home`.
fn journals(home: &Path) -> String {
    std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect()
}

#[tokio::test]
async fn sigterm_during_a_prompt_run_stops_the_tool_and_exits_143() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pid");
    let (url, server) = fake_api(vec![(200, slow_bash(&pid_file))]).await;
    let child = command(home.path(), &url, &["-p", "sleep a while"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let sleeper = pid_in(&pid_file).await;
    send(child.id(), libc::SIGTERM);

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(143), "{stderr}");
    assert!(stderr.contains("interrupted by SIGTERM"), "{stderr}");
    until("the tool's child to be gone", || !alive(sleeper)).await;
    assert_eq!(server.await.unwrap().len(), 1);
    // The journal answers the cancelled call, so the session can go on.
    let journal = journals(home.path());
    assert!(
        journal.contains("\"tool_use_id\":\"toolu_slow\""),
        "{journal}"
    );
    assert!(journal.contains("cancelled by the user"), "{journal}");
}

#[tokio::test]
async fn ctrl_c_in_the_repl_cancels_the_turn_and_the_next_input_works() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pid");
    let (url, server) = fake_api(vec![
        (200, slow_bash(&pid_file)),
        (200, text_reply("Back again.")),
    ])
    .await;
    let mut child = command(home.path(), &url, &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = Arc::new(Mutex::new(String::new()));
    let mut pipe = child.stdout.take().unwrap();
    let collected = Arc::clone(&stdout);
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            collected
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"sleep a while\n").await.unwrap();

    let sleeper = pid_in(&pid_file).await;
    send(child.id(), libc::SIGINT);
    until("the tool to be killed", || !alive(sleeper)).await;
    stdin.write_all(b"are you there?\n").await.unwrap();
    until("the second answer", || {
        stdout.lock().unwrap().contains("Back again.")
    })
    .await;

    // Two Ctrl-C at the prompt quit.
    send(child.id(), libc::SIGINT);
    until("the quit hint", || {
        stdout.lock().unwrap().contains("press Ctrl-C again")
    })
    .await;
    send(child.id(), libc::SIGINT);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("junees did not exit")
        .unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("(turn cancelled)"), "{stderr}");

    // The second request carries the cancelled result and the new input
    // in one user message.
    let requests = server.await.unwrap();
    let messages = body_of(&requests[1])["messages"].clone();
    assert_eq!(messages.as_array().unwrap().len(), 3, "{messages}");
    let last = &messages[2]["content"];
    assert_eq!(last[0]["type"], "tool_result");
    assert_eq!(last[0]["tool_use_id"], "toolu_slow");
    assert_eq!(last[0]["is_error"], true);
    assert_eq!(last[0]["content"], "cancelled by the user");
    assert_eq!(last[1]["text"], "are you there?");
}
