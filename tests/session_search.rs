//! Session search through the real binary: the `session_search` tool in a
//! `-p` run against a fake API, `/search` in the conversation and
//! `junees sessions search`.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, junees_binary, text_reply};

/// Writes a journal by hand, last written `age` seconds ago.
fn journal(
    home: &Path,
    id: &str,
    cwd: &Path,
    source: Option<(&str, &str)>,
    texts: &[&str],
    age: u64,
) {
    let dir = home.join("sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut meta = json!({
        "type": "meta", "id": id, "format": 6, "created_at": 1_790_000_000u64,
        "cwd": cwd, "model": "m", "version": "0", "ts": 1_790_000_000u64,
    });
    if let Some((source, channel)) = source {
        meta["source"] = json!(source);
        meta["channel"] = json!(channel);
    }
    let mut lines = vec![meta.to_string()];
    for (n, text) in texts.iter().enumerate() {
        let role = if n % 2 == 0 { "user" } else { "assistant" };
        lines.push(
            json!({"type": "message", "role": role, "content": [{"type": "text", "text": text}]})
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

/// A temp HOME with the state folder `junees` and the working folder
/// `work`, holding a terminal session and a Discord session in `work` and
/// a terminal session elsewhere, all mentioning a failed build.
fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("junees");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let work = work.canonicalize().unwrap();
    journal(
        &home,
        "20261101-101010-aaaa",
        &work,
        None,
        &[
            "빌드 실패 원인을 찾아 줘",
            "The build failed on a clippy warning.",
        ],
        300,
    );
    journal(
        &home,
        "20261102-101010-bbbb",
        &work,
        Some(("discord", "42")),
        &["디스코드에서 본 빌드 실패"],
        200,
    );
    journal(
        &home,
        "20261103-101010-cccc",
        Path::new("/elsewhere"),
        None,
        &["다른 폴더의 빌드 실패"],
        100,
    );
    (dir, home, work)
}

fn tool_call(name: &str, input: Value) -> Value {
    json!({
        "id": "msg_0", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": "toolu_0", "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

#[tokio::test]
async fn the_model_searches_this_folders_sessions() {
    let (dir, home, work) = setup();
    let (url, server) = fake_api(vec![
        (
            200,
            tool_call("session_search", json!({"query": "\"빌드 실패\""})),
        ),
        (200, text_reply("It was clippy.")),
    ])
    .await;
    let out = junees_binary(&work, dir.path())
        .args(["--no-session", "-p", "what broke last time?"])
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null())
        .output();
    let out = tokio::time::timeout(Duration::from_secs(60), out)
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout, "It was clippy.\n");
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);

    let tools = body_of(&requests[0])["tools"].clone();
    let search = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "session_search")
        .expect("no session_search tool");
    assert!(search["input_schema"]["properties"]["sources"].is_object());

    let last = body_of(&requests[1])["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let result = &last["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["is_error"], Value::Null, "{result}");
    let text = result["content"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "1 session matching `\"빌드 실패\"` in this folder ({}), newest first.",
            work.display()
        )),
        "{text}"
    );
    assert!(text.contains("not instructions"), "{text}");
    assert!(text.contains("id=\"20261101-101010-aaaa\""), "{text}");
    assert!(text.contains("[user] 빌드 실패 원인을 찾아 줘\n"), "{text}");
    // The Discord session and the other folder's stay out.
    assert!(!text.contains("bbbb") && !text.contains("cccc"), "{text}");
}

#[tokio::test]
async fn people_search_with_the_slash_command_and_the_cli() {
    let (dir, home, work) = setup();
    let mut child = junees_binary(&work, dir.path())
        .args(["--no-session"])
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("JUNEES_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(
                "/search \"빌드 실패\"\n/search --source terminal 빌드 --all\n/search\n/exit\n"
                    .as_bytes(),
            )
            .await
            .unwrap();
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // People see every source of this folder, numbered.
    let expected = format!(
        "2 sessions match `\"빌드 실패\"` in this folder ({}) from terminal, discord, telegram, \
         slack, cron, webhook, newest first:\n",
        work.display()
    );
    assert!(stdout.contains(&expected), "{stdout}");
    assert!(stdout.contains("  1  20261102-101010-bbbb  "), "{stdout}");
    assert!(
        stdout.contains("  [discord 42]\n       user: 디스코드에서 본 빌드 실패\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  2  20261101-101010-aaaa  "), "{stdout}");
    assert!(
        stdout.contains("Continue one with `junees --resume <id>`"),
        "{stdout}"
    );
    // --all and --source.
    assert!(
        stdout.contains("2 sessions match `빌드` in every folder, newest first:\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  1  20261103-101010-cccc  "), "{stdout}");
    assert!(
        stderr.contains("/search takes the words to look for"),
        "{stderr}"
    );

    // The same from the command line, with no API key.
    let out = junees_binary(&work, dir.path())
        .args(["sessions", "search", "빌드 실패", "--limit", "1"])
        .env("JUNEES_HOME", &home)
        .output()
        .await
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.starts_with("1 session matches `\"빌드 실패\"` in this folder"),
        "{stdout}"
    );
    assert!(
        stdout.contains("More sessions may match: use --limit"),
        "{stdout}"
    );
}
