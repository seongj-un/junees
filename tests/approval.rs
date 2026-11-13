//! Runs the real binary with tool calls that need approval. Without a
//! terminal the conversation mode declines them and says so; `-p` never
//! asks (#approval).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{Served, body_of, fake_api, fake_stream_api, sse_stream, text_reply};

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

/// The working folder under `home`: a subfolder, so the Landlock sandbox
/// lets commands create files in it (a working folder holding the state
/// folder keeps new entries out of its top level).
fn work(home: &Path) -> std::path::PathBuf {
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    work
}

fn command(home: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(&work(home), home);
    command
        .args(args)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home.join(".junees"))
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
    std::fs::write(work(home.path()).join("marker"), "keep me").unwrap();
    let (url, server) = fake_api(vec![
        (200, bash_calls(&["rm marker", "touch made"])),
        (200, text_reply("Done.")),
    ])
    .await;
    let stderr = converse(home.path(), &url, None, "clean up\n").await;

    assert!(
        stderr.contains(
            "stdin or stderr is not a terminal: tool calls that need approval (JUNEES_APPROVAL=risky) \
             are declined"
        ),
        "{stderr}"
    );
    assert!(
        work(home.path()).join("marker").exists(),
        "rm ran without approval"
    );
    assert!(
        work(home.path()).join("made").exists(),
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
         cannot ask without a terminal on stdin and stderr"
    );
    assert_eq!(
        results[1],
        ("toolu_1".to_string(), "[exit code 0]".to_string(), false)
    );
}

#[tokio::test]
async fn approval_off_runs_risky_calls_in_the_conversation_mode() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work(home.path()).join("marker"), "remove me").unwrap();
    let (url, server) = fake_api(vec![
        (200, bash_calls(&["rm marker"])),
        (200, text_reply("Done.")),
    ])
    .await;
    let stderr = converse(home.path(), &url, Some("off"), "clean up\n").await;
    assert!(!stderr.contains("not a terminal"), "{stderr}");
    assert!(!work(home.path()).join("marker").exists());
    let requests = server.await.unwrap();
    assert!(!results(&requests[1])[0].2);
}

#[tokio::test]
async fn a_prompt_run_never_asks() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work(home.path()).join("marker"), "remove me").unwrap();
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
        !work(home.path()).join("marker").exists(),
        "-p did not run the call"
    );
    let requests = server.await.unwrap();
    assert_eq!(
        results(&requests[1]),
        [("toolu_0".to_string(), "[exit code 0]".to_string(), false)]
    );
}

/// A pseudo-terminal without echo: (the side the test reads and writes,
/// the side junees gets as stdin, stdout and stderr).
fn terminal() -> (std::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let (mut leader, mut follower) = (0, 0);
    // SAFETY: openpty writes the two descriptors it opens; the termios
    // calls get a valid descriptor and a zeroed struct they fill in.
    unsafe {
        assert_eq!(
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            0
        );
        let mut mode: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(follower, &mut mode), 0);
        mode.c_lflag &= !libc::ECHO;
        assert_eq!(libc::tcsetattr(follower, libc::TCSANOW, &mode), 0);
        (
            std::fs::File::from_raw_fd(leader),
            std::os::fd::OwnedFd::from_raw_fd(follower),
        )
    }
}

#[tokio::test]
async fn streamed_text_and_the_question_share_the_terminal() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(work(home.path()).join("marker"), "remove me").unwrap();
    let ask = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [
            {"type": "text", "text": "Let me remove the marker."},
            {"type": "tool_use", "id": "toolu_0", "name": "bash", "input": {"command": "rm marker"}}
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let (url, server) = fake_stream_api(vec![
        Served::Stream {
            body: sse_stream(&ask, 3),
            chunk: 7,
        },
        Served::Stream {
            body: sse_stream(&text_reply("Removed it."), 3),
            chunk: 7,
        },
    ])
    .await;
    let (mut screen, follower) = terminal();
    let mut child = command(home.path(), &url, &["--no-session"])
        .env_remove("JUNEES_STREAM")
        .stdin(Stdio::from(follower.try_clone().unwrap()))
        .stdout(Stdio::from(follower.try_clone().unwrap()))
        .stderr(Stdio::from(follower))
        .spawn()
        .unwrap();
    // The prompt and the question read the same lines.
    use std::io::{Read, Write};
    screen.write_all(b"clean up\ny\n/exit\n").unwrap();
    let reader = std::thread::spawn(move || {
        let mut shown = Vec::new();
        // Reading fails (EIO) once junees and its children closed the
        // terminal.
        let _ = screen.read_to_end(&mut shown);
        String::from_utf8_lossy(&shown).replace('\r', "")
    });
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("junees did not exit")
        .unwrap();
    let shown = tokio::task::spawn_blocking(move || reader.join().unwrap())
        .await
        .unwrap();
    assert!(status.success(), "{shown}");

    // The streamed line is ended before the call and its question show.
    let text = shown.find("Let me remove the marker.\n").expect(&shown);
    let call = shown.find("\n  -> bash ").expect(&shown);
    let question = shown
        .find("\n  ?? 승인 요청 bash: rm marker\n     * `rm`: 파일을 지웁니다\n")
        .expect(&shown);
    let answer = shown.find("Removed it.\n").expect(&shown);
    assert!(
        text < call && call < question && question < answer,
        "{shown}"
    );
    assert!(!work(home.path()).join("marker").exists(), "{shown}");
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body_of(&requests[0])["stream"], true);
    assert_eq!(
        results(&requests[1]),
        [("toolu_0".to_string(), "[exit code 0]".to_string(), false)]
    );
}
