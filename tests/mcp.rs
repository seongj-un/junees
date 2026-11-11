//! Runs the real binary against a fake API server and the fake MCP server
//! (`tests/fixtures/fake_mcp.py`): the model calls MCP tools, the server
//! does not see Junees's secrets, and it is gone when Junees exits.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use junees::approval::{Answer, Ask, Request};
use junees::cancel::Cancel;
use junees::config::Config;
use junees::gateway::engine::{ApiEngines, Engines};
use junees::mcp::Mcp;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{body_of, fake_api, text_reply};

const FAKE_SERVER: &str = include_str!("fixtures/fake_mcp.py");

/// A state folder whose `mcp.json` (with `mode`) runs the fake server as
/// `fake`, and the server's message log.
fn home_with_server(dir: &Path, mode: u32) -> (PathBuf, PathBuf) {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let script = dir.join("fake_mcp.py");
    std::fs::write(&script, FAKE_SERVER).unwrap();
    let log = dir.join("log.jsonl");
    let list = json!({"servers": {"fake": {
        "command": "python3",
        "args": [script],
        "env": {"FAKE_MCP_MODE": "modern", "FAKE_MCP_LOG": log, "FAKE_TOKEN": "from-config"},
    }}});
    let path = home.join("mcp.json");
    std::fs::write(&path, list.to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    (home, log)
}

fn command(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .args(args)
        .env("ANTHROPIC_API_KEY", "sk-fake-mcp-test")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null());
    command
}

fn tool_calls(calls: &[(&str, &str, Value)]) -> Value {
    let content: Vec<Value> = calls
        .iter()
        .map(
            |(id, name, input)| json!({"type": "tool_use", "id": id, "name": name, "input": input}),
        )
        .collect();
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn tool_names(request: &str) -> Vec<String> {
    body_of(request)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

fn check(output: &Output) {
    assert!(
        output.status.success(),
        "junees failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

#[tokio::test]
async fn the_model_calls_mcp_tools_whose_server_sees_no_secrets() {
    let names = [
        "ANTHROPIC_API_KEY",
        "JUNEES_DISCORD_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "JUNEES_TEST_HIDDEN",
        "FAKE_TOKEN",
        "JUNEES_TEST_VISIBLE",
    ];
    let (url, api) = fake_api(vec![
        (
            200,
            tool_calls(&[
                ("toolu_1", "mcp__fake__env", json!({"names": names})),
                ("toolu_2", "mcp__fake__spawn", json!({})),
            ]),
        ),
        (200, text_reply("done")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (home, log) = home_with_server(dir.path(), 0o600);
    let output = command(&home, dir.path(), &url, &["-p", "go", "--no-session"])
        .env("JUNEES_HIDDEN_ENV", "JUNEES_TEST_HIDDEN")
        .env("JUNEES_TEST_HIDDEN", "hidden-value")
        .env("JUNEES_TEST_VISIBLE", "shown")
        .env("JUNEES_DISCORD_TOKEN", "discord-secret")
        .env("CLAUDE_CODE_OAUTH_TOKEN", "oauth-secret")
        .output()
        .await
        .unwrap();
    check(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mcp: fake started (12 tools)"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n");
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);

    let tools = tool_names(&requests[0]);
    assert!(tools.contains(&"mcp__fake__echo".to_string()), "{tools:?}");
    assert!(tools.contains(&"bash".to_string()), "{tools:?}");
    let echo = body_of(&requests[0])["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "mcp__fake__echo")
        .unwrap()
        .clone();
    assert!(
        echo["description"]
            .as_str()
            .unwrap()
            .starts_with("Echoes\\u{1b}[2K the text"),
        "{echo}"
    );

    let body = body_of(&requests[1]);
    let results = body["messages"].as_array().unwrap().last().unwrap()["content"].clone();
    assert_eq!(results[0]["tool_use_id"], "toolu_1");
    assert_eq!(
        results[0]["content"],
        "ANTHROPIC_API_KEY=<unset>\nJUNEES_DISCORD_TOKEN=<unset>\nCLAUDE_CODE_OAUTH_TOKEN=<unset>\n\
         JUNEES_TEST_HIDDEN=<unset>\n\
         FAKE_TOKEN=from-config\nJUNEES_TEST_VISIBLE=shown"
    );
    let sleep_pid: u32 = results[1]["content"].as_str().unwrap().parse().unwrap();
    // Junees ended the server, and with it what the server started.
    for _ in 0..50 {
        if !alive(sleep_pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive(sleep_pid), "the server's background sleep survived");
    let log = std::fs::read_to_string(log).unwrap();
    assert!(log.contains("\"tools/call\""), "{log}");
}

#[tokio::test]
async fn a_server_list_others_can_read_is_not_used() {
    let (url, api) = fake_api(vec![(200, text_reply("hi"))]).await;
    let dir = tempfile::tempdir().unwrap();
    let (home, log) = home_with_server(dir.path(), 0o644);
    let output = command(&home, dir.path(), &url, &["-p", "hello", "--no-session"])
        .output()
        .await
        .unwrap();
    check(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mcp: servers not started: "), "{stderr}");
    assert!(stderr.contains("chmod 600"), "{stderr}");
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !tool_names(&requests[0])
            .iter()
            .any(|name| name.starts_with("mcp__"))
    );
    assert!(!log.exists(), "the server ran");
}

#[tokio::test]
async fn slash_mcp_shows_the_servers() {
    let dir = tempfile::tempdir().unwrap();
    let (home, _log) = home_with_server(dir.path(), 0o600);
    let mut child = command(&home, dir.path(), "http://127.0.0.1:9", &["--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"/mcp\n/exit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    check(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "  fake: running, MCP 2026-07-28, 12 tools\n     command `python3`, env \
             FAKE_MCP_LOG, FAKE_MCP_MODE, FAKE_TOKEN; 12 tools offered to the model\n"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("from-config"), "{stdout}");
}

fn last_results(request: &str) -> Value {
    let body = body_of(request);
    body["messages"].as_array().unwrap().last().unwrap()["content"].clone()
}

#[tokio::test]
async fn the_conversation_asks_before_untrusted_mcp_tools() {
    // Without a terminal the question cannot be asked, so the call is
    // declined; a trusted server's tool runs.
    let (url, api) = fake_api(vec![
        (
            200,
            tool_calls(&[("toolu_1", "mcp__fake__echo", json!({"text": "hi"}))]),
        ),
        (200, text_reply("ok")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (home, _log) = home_with_server(dir.path(), 0o600);
    let mut child = command(&home, dir.path(), &url, &["--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    check(&output);
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    let result = last_results(&requests[1])[0]["content"].clone();
    assert!(
        result
            .as_str()
            .unwrap()
            .starts_with("not run: this call needs the user's approval"),
        "{result}"
    );

    // The same with `trust: true` runs.
    let (url, api) = fake_api(vec![
        (
            200,
            tool_calls(&[("toolu_1", "mcp__fake__echo", json!({"text": "hi"}))]),
        ),
        (200, text_reply("ok")),
    ])
    .await;
    let path = home.join("mcp.json");
    let mut list: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    list["servers"]["fake"]["trust"] = json!(true);
    std::fs::write(&path, list.to_string()).unwrap();
    let mut child = command(&home, dir.path(), &url, &["--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    check(&output);
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_results(&requests[1])[0]["content"], "echo: hi");
}

/// Declines every question and keeps it.
#[derive(Clone, Default)]
struct Decline(Arc<Mutex<Vec<Request>>>);

#[async_trait]
impl Ask for Decline {
    async fn ask(&mut self, request: &Request) -> Answer {
        self.0.lock().unwrap().push(request.clone());
        Answer::NoBecause("not in the channel".into())
    }
}

#[tokio::test]
async fn gateway_channels_ask_before_untrusted_mcp_tools() {
    let (url, api) = fake_api(vec![
        (
            200,
            tool_calls(&[("toolu_1", "mcp__fake__echo", json!({"text": "hi"}))]),
        ),
        (200, text_reply("ok")),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (home, _log) = home_with_server(dir.path(), 0o600);
    let settings = [
        ("ANTHROPIC_API_KEY", "sk-fake".to_string()),
        ("ANTHROPIC_BASE_URL", url),
        ("JUNEES_HOME", home.display().to_string()),
        ("JUNEES_STREAM", "0".to_string()),
        ("JUNEES_APPROVAL", "risky".to_string()),
    ];
    let config = Config::from_lookup(|key| {
        settings
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let mcp = Arc::new(Mcp::start(config.home.as_deref(), &[], dir.path()).await);
    let engines = ApiEngines::new(config, dir.path().to_path_buf()).with_mcp(Arc::clone(&mcp));
    let asked = Decline::default();
    let mut conversation = engines
        .open(Box::new(asked.clone()), Cancel::new())
        .unwrap();
    let (progress, _receiver) = tokio::sync::mpsc::unbounded_channel();
    conversation.turn("go", &progress).await.unwrap();
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        last_results(&requests[1])[0]["content"],
        "the user declined: not in the channel"
    );
    let asked = asked.0.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].target, "tool `echo` of the MCP server `fake`");
    mcp.shutdown().await;
}

/// Writes an executable script and runs it once, so a test thread that
/// forked while it was open for writing cannot make exec fail later.
fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..200 {
        match std::process::Command::new(path)
            .env("MARKER_DIR", "/nonexistent")
            .stderr(Stdio::null())
            .status()
        {
            Ok(_) => return,
            Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("cannot run {}: {err}", path.display()),
        }
    }
    panic!("{} stayed busy", path.display());
}

/// A working folder holding a Python module and a script that only leave
/// a marker file in `markers` when they run, and a state folder whose
/// `mcp.json` holds `servers`.
fn repo_with_servers(dir: &Path, servers: Value) -> (PathBuf, PathBuf, PathBuf) {
    let repo = dir.join("repo");
    let markers = dir.join("markers");
    let home = dir.join("home");
    for folder in [&repo, &markers, &home] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(
        repo.join("mcp_server_repo.py"),
        "import os\nopen(os.path.join(os.environ['MARKER_DIR'], 'module'), 'w').close()\n",
    )
    .unwrap();
    write_script(
        &repo.join("server.sh"),
        "#!/bin/sh\ntouch \"$MARKER_DIR/script-$(basename \"$PWD\")\"\n",
    );
    let path = home.join("mcp.json");
    std::fs::write(&path, json!({ "servers": servers }).to_string()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (repo, markers, home)
}

fn markers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn servers_do_not_run_files_of_the_working_folder() {
    let dir = tempfile::tempdir().unwrap();
    let env = |markers: &Path| json!({ "MARKER_DIR": markers });
    let markers_dir = dir.path().join("markers");
    let (repo, markers_dir, home) = repo_with_servers(
        dir.path(),
        json!({
            "module": {"command": "python3", "args": ["-m", "mcp_server_repo"], "env": env(&markers_dir)},
            "script": {"command": "./server.sh", "env": env(&markers_dir)},
        }),
    );
    let (url, api) = fake_api(vec![(200, text_reply("done"))]).await;
    let output = command(&home, &repo, &url, &["-p", "go", "--no-session"])
        .output()
        .await
        .unwrap();
    check(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    // Neither the module nor the script of the repository ran: the servers
    // ran in `/`, where `python3 -m` finds no such module and `./server.sh`
    // is `/server.sh`.
    assert!(markers(&markers_dir).is_empty(), "{stderr}");
    assert!(stderr.contains("mcp: module failed to start"), "{stderr}");
    assert!(
        stderr.contains("mcp: script failed to start: cannot run `./server.sh`"),
        "{stderr}"
    );
}

#[tokio::test]
async fn a_relative_command_into_the_working_folder_needs_trust() {
    let dir = tempfile::tempdir().unwrap();
    let markers_dir = dir.path().join("markers");
    let repo = dir.path().join("repo");
    let (repo, markers_dir, home) = repo_with_servers(
        dir.path(),
        json!({
            "pinned": {"command": "./server.sh", "cwd": repo, "env": {"MARKER_DIR": markers_dir}},
            "trusted": {"command": "./server.sh", "cwd": repo, "trust": true,
                        "env": {"MARKER_DIR": markers_dir}},
        }),
    );
    let (url, api) = fake_api(vec![(200, text_reply("done"))]).await;
    let output = command(&home, &repo, &url, &["-p", "go", "--no-session"])
        .output()
        .await
        .unwrap();
    check(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stderr.contains(
            "mcp: pinned failed to start: `command` `./server.sh` is a relative path into \
             the folder Junees works in"
        ),
        "{stderr}"
    );
    // Only the trusted one ran, in its `cwd`.
    assert_eq!(markers(&markers_dir), ["script-repo"], "{stderr}");
}
