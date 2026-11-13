//! Runs the real binary against a fake API: the file tools' changes are
//! recorded turn by turn, `/checkpoints` and `junees rewind --list` show
//! them, `junees rewind --turn` and `/undo` undo them (an undo too), the
//! next message carries the undo's line, and the dashboard's chat serves
//! the checkpoints and undo API behind its write checks.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Child;

mod common;
use common::{body_of, fake_api, text_reply};

const WAIT: Duration = Duration::from_secs(60);

/// A HOME with the state folder `.junees` and a working folder `work`.
struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        Self {
            _dir: dir,
            root,
            work,
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join(".junees")
    }

    /// The binary in `work`, on the fake API, asking no approvals.
    fn command(&self, url: &str, args: &[&str]) -> tokio::process::Command {
        let mut command = common::junees_binary(&self.work, &self.root);
        command
            .args(args)
            .env("ANTHROPIC_API_KEY", "sk-fake")
            .env("ANTHROPIC_BASE_URL", url)
            .env("JUNEES_HOME", self.state())
            .env("JUNEES_STREAM", "0")
            .env("JUNEES_APPROVAL", "off")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Runs `args` with `input` on stdin to its end.
    async fn run(&self, url: &str, args: &[&str], input: &str) -> Output {
        let mut child = self.command(url, args).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).await.unwrap();
        drop(stdin);
        let output = tokio::time::timeout(WAIT, child.wait_with_output())
            .await
            .expect("junees did not end")
            .unwrap();
        assert!(
            output.status.success(),
            "junees {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// The id of the one saved session.
    fn session_id(&self) -> String {
        let ids: Vec<String> = std::fs::read_dir(self.state().join("sessions"))
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".jsonl").map(str::to_string)
            })
            .collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids[0].clone()
    }
}

fn tool_call(calls: Vec<Value>) -> Value {
    json!({
        "id": "msg_t", "type": "message", "role": "assistant", "model": "m",
        "content": calls,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

fn write_file(id: &str, path: &Path, content: &str) -> Value {
    json!({"type": "tool_use", "id": id, "name": "write_file",
           "input": {"path": path.display().to_string(), "content": content}})
}

fn edit_file(id: &str, path: &Path, old: &str, new: &str) -> Value {
    json!({"type": "tool_use", "id": id, "name": "edit_file",
           "input": {"path": path.display().to_string(), "old_string": old, "new_string": new}})
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The text of the last user message of an API request.
fn last_user_text(request: &str) -> String {
    let body = body_of(request);
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
    match last["content"].as_str() {
        Some(text) => text.to_string(),
        None => last["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[tokio::test]
async fn turns_are_listed_rewound_and_told_to_the_conversation() {
    let home = Home::new();
    let a = home.work.join("a.txt");
    let b = home.work.join("b.txt");
    std::fs::write(&a, "zero\n").unwrap();

    // Turn 1 writes a.txt; turn 2 edits it and creates b.txt.
    let (url, server) = fake_api(vec![
        (200, tool_call(vec![write_file("t1", &a, "one\n")])),
        (200, text_reply("wrote")),
        (
            200,
            tool_call(vec![
                edit_file("t2", &a, "one", "two"),
                write_file("t3", &b, "made\n"),
            ]),
        ),
        (200, text_reply("edited")),
    ])
    .await;
    let output = home
        .run(&url, &[], "first\nsecond\n/checkpoints\n/exit\n")
        .await;
    let shown = text(&output);
    server.await.unwrap();
    assert_eq!(read(&a).as_deref(), Some("two\n"));
    assert_eq!(read(&b).as_deref(), Some("made\n"));
    for part in ["turn 1 (", "turn 2 (", "modified", "created"] {
        assert!(shown.contains(part), "/checkpoints: {part} in {shown}");
    }

    let id = home.session_id();
    let dir = home.state().join("checkpoints").join(&id);
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("2/manifest.json")), 0o600);

    // --list changes nothing.
    let (url, _) = fake_api(Vec::new()).await;
    let list = home
        .run(&url, &["rewind", "--session", &id, "--list"], "")
        .await;
    let listed = String::from_utf8_lossy(&list.stdout).to_string();
    assert!(
        listed.contains("turn 1") && listed.contains("turn 2"),
        "{listed}"
    );
    assert!(
        listed.contains(&format!("created  {}", b.display())),
        "{listed}"
    );

    // Without --yes it asks; "n" keeps everything.
    let kept = home
        .run(&url, &["rewind", "--session", &id, "--turn", "2"], "n\n")
        .await;
    assert!(text(&kept).contains("(nothing undone)"), "{}", text(&kept));
    assert_eq!(read(&a).as_deref(), Some("two\n"));

    // Turn 2 undone: a.txt back to what turn 1 left, b.txt gone.
    let undone = home
        .run(
            &url,
            &["rewind", "--session", &id, "--turn", "2", "--yes"],
            "",
        )
        .await;
    let said = text(&undone);
    assert_eq!(read(&a).as_deref(), Some("one\n"), "{said}");
    assert!(!b.exists(), "{said}");
    assert!(said.contains("recorded as turn 3"), "{said}");
    assert!(said.contains("bash changed are not recorded"), "{said}");

    // The next message of the session starts with the undo's line, once.
    let (url, server) =
        fake_api(vec![(200, text_reply("noted")), (200, text_reply("again"))]).await;
    home.run(&url, &["--resume", &id], "next\nmore\n/exit\n")
        .await;
    let requests = server.await.unwrap();
    let first = last_user_text(&requests[0]);
    assert!(
        first.starts_with("[되돌림] 턴 2의 파일 변경을 되돌렸습니다: "),
        "{first}"
    );
    assert!(
        first.contains("a.txt") && first.contains("b.txt"),
        "{first}"
    );
    assert!(first.ends_with("next"), "{first}");
    assert!(!last_user_text(&requests[1]).contains("[되돌림]"));

    // /undo undoes the last turn that changed files: the undo itself.
    let (url, server) = fake_api(vec![]).await;
    let output = home
        .run(&url, &["--resume", &id], "/undo\ny\n/exit\n")
        .await;
    server.await.unwrap();
    let said = text(&output);
    assert!(said.contains("undo these changes"), "{said}");
    assert_eq!(read(&a).as_deref(), Some("two\n"), "{said}");
    assert_eq!(read(&b).as_deref(), Some("made\n"), "{said}");

    // A file changed since is skipped without --force.
    std::fs::write(&a, "by hand\n").unwrap();
    let skipped = home
        .run(
            &url,
            &["rewind", "--session", &id, "--turn", "1", "--yes"],
            "",
        )
        .await;
    assert!(text(&skipped).contains("skipped"), "{}", text(&skipped));
    assert_eq!(read(&a).as_deref(), Some("by hand\n"));
}

#[tokio::test]
async fn a_file_undone_part_of_the_way_is_reported_as_changed() {
    let home = Home::new();
    let c = home.work.join("c.txt");
    // Turn 1 makes c.txt; something outside changes it; turn 2 changes it.
    let (url, server) = fake_api(vec![
        (200, tool_call(vec![write_file("t1", &c, "C1\n")])),
        (200, text_reply("made")),
        (200, tool_call(vec![write_file("t2", &c, "C2\n")])),
        (200, text_reply("changed")),
    ])
    .await;
    let mut child = home.command(&url, &[]).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"make it\n").await.unwrap();
    // Wait for turn 1's file before changing it by hand.
    let deadline = tokio::time::Instant::now() + WAIT;
    while read(&c).as_deref() != Some("C1\n") {
        assert!(tokio::time::Instant::now() < deadline, "turn 1 never wrote");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::write(&c, "C1x\n").unwrap();
    stdin.write_all(b"change it\n/exit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    server.await.unwrap();
    let id = home.session_id();

    let (url, _) = fake_api(Vec::new()).await;
    let undone = home
        .run(
            &url,
            &["rewind", "--session", &id, "--turn", "1", "--yes"],
            "",
        )
        .await;
    let said = text(&undone);
    assert_eq!(read(&c).as_deref(), Some("C1x\n"), "{said}");
    assert!(
        said.contains(&format!(
            "partly   {} (턴 2까지 되돌림, 턴 1은 건너뜀: ",
            c.display()
        )),
        "{said}"
    );
    assert!(!said.contains("skipped  "), "{said}");

    let (url, server) = fake_api(vec![(200, text_reply("noted"))]).await;
    home.run(&url, &["--resume", &id, "-p", "next"], "").await;
    let first = last_user_text(&server.await.unwrap()[0]);
    assert!(
        first.starts_with(&format!(
            "[되돌림] 턴 1, 2의 파일 변경을 되돌렸습니다: {} (턴 2까지 되돌림, 턴 1은 건너뜀: ",
            c.display()
        )),
        "{first}"
    );
}

#[tokio::test]
async fn the_setting_turns_recording_off() {
    let home = Home::new();
    let a = home.work.join("a.txt");
    let (url, server) = fake_api(vec![
        (200, tool_call(vec![write_file("t1", &a, "one\n")])),
        (200, text_reply("wrote")),
    ])
    .await;
    let mut command = home.command(&url, &["-p", "write it"]);
    command.env("JUNEES_CHECKPOINTS", "0").stdin(Stdio::null());
    let output = tokio::time::timeout(WAIT, command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    server.await.unwrap();
    assert_eq!(read(&a).as_deref(), Some("one\n"));
    assert!(!home.state().join("checkpoints").exists());

    let id = home.session_id();
    let (url, _) = fake_api(Vec::new()).await;
    let mut rewind = home.command(&url, &["rewind", "--session", &id, "--list"]);
    rewind.stdin(Stdio::null());
    let output = rewind.output().await.unwrap();
    assert!(!output.status.success());
    assert!(
        text(&output).contains("has no file checkpoints"),
        "{}",
        text(&output)
    );
}

/// `junees dashboard --chat` on the fake API.
struct Dashboard {
    _child: Child,
    port: u16,
    token: String,
}

impl Dashboard {
    async fn start(home: &Home, url: &str, chat: bool) -> Self {
        let mut args = vec!["dashboard", "--no-open", "--listen", "127.0.0.1:0"];
        if chat {
            args.push("--chat");
        }
        let mut command = home.command(url, &args);
        command.stdin(Stdio::null());
        let mut child = command.spawn().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .expect("no address printed")
            .unwrap()
            .expect("the dashboard exited");
        let url = line
            .strip_prefix("Junees dashboard: http://127.0.0.1:")
            .unwrap_or_else(|| panic!("not an address: {line}"));
        let (port, token) = url.split_once("/#token=").unwrap();
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Self {
            _child: child,
            port: port.parse().unwrap(),
            token: token.to_string(),
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    async fn send(&self, method: &str, path: &str, headers: &str, body: &str) -> (u16, Value) {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            self.host(),
            body.len()
        );
        let bytes = common::exchange(("127.0.0.1", self.port), request.as_bytes(), WAIT).await;
        let text = String::from_utf8_lossy(&bytes).to_string();
        let (head, body) = text.split_once("\r\n\r\n").expect("no head");
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    fn bearer(&self) -> String {
        format!("Authorization: Bearer {}\r\n", self.token)
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let headers = format!(
            "Origin: http://{}\r\n{}Content-Type: application/json\r\n",
            self.host(),
            self.bearer()
        );
        self.send("POST", path, &headers, &body.to_string()).await
    }

    /// The first `turn_end` event of chat `id`, read from its stream.
    async fn turn_end(&self, id: &str) -> Value {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let request = format!(
            "GET /api/chat/{id}/events HTTP/1.1\r\nHost: {}\r\n{}\r\n",
            self.host(),
            self.bearer()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut lines = BufReader::new(stream).lines();
        loop {
            let line = tokio::time::timeout(WAIT, lines.next_line())
                .await
                .expect("no turn_end")
                .unwrap()
                .expect("the stream ended");
            if let Some(data) = line.strip_prefix("data: ") {
                let event: Value = serde_json::from_str(data).unwrap();
                if event["type"] == "turn_end" {
                    return event;
                }
            }
        }
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send("GET", path, &self.bearer(), "").await
    }

    /// Waits until chat `id` has no turn running: an undo of a turn that
    /// is not there answers 409 until then, and 404 after.
    async fn idle(&self, id: &str) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let (status, answer) = self
                .post(&format!("/api/chat/{id}/undo"), json!({"turn": 99}))
                .await;
            match status {
                404 => return,
                409 => assert_eq!(answer["error"], "turn_running"),
                _ => panic!("{status} {answer}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn never ended"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test]
async fn the_chat_serves_checkpoints_and_undo() {
    let home = Home::new();
    let made = home.work.join("made.txt");
    let (url, server) = fake_api(vec![
        (200, tool_call(vec![write_file("t1", &made, "made\n")])),
        (200, text_reply("wrote")),
    ])
    .await;
    let dashboard = Dashboard::start(&home, &url, true).await;
    let (status, opened) = dashboard.post("/api/chat", json!({})).await;
    assert_eq!(status, 200, "{opened}");
    let id = opened["session_id"].as_str().unwrap().to_string();
    let (status, sent) = dashboard
        .post(
            &format!("/api/chat/{id}/messages"),
            json!({"text": "make it"}),
        )
        .await;
    assert_eq!(status, 202, "{sent}");
    tokio::time::timeout(WAIT, server).await.unwrap().unwrap();
    dashboard.idle(&id).await;
    assert_eq!(read(&made).as_deref(), Some("made\n"));

    // The chat's turn says which checkpoint it recorded.
    let end = dashboard.turn_end(&id).await;
    assert_eq!(
        (&end["turn"], &end["checkpoint"]),
        (&json!(1), &json!(1)),
        "{end}"
    );

    let (status, mut list) = dashboard.get(&format!("/api/chat/{id}/checkpoints")).await;
    assert_eq!(status, 200);
    let created_at = list[0]["created_at"].take();
    assert!(
        created_at.as_str().is_some_and(|at| at.ends_with('Z')),
        "{created_at}"
    );
    assert_eq!(
        list,
        json!([{"turn": 1, "created_at": null, "files": [
            {"path": made.display().to_string(), "change": "created", "restorable": true}
        ]}])
    );

    // The write checks: no Origin is 403, a bad body 400, an unknown turn 404.
    let no_origin = format!("{}Content-Type: application/json\r\n", dashboard.bearer());
    let (status, _) = dashboard
        .send(
            "POST",
            &format!("/api/chat/{id}/undo"),
            &no_origin,
            "{\"turn\":1}",
        )
        .await;
    assert_eq!(status, 403);
    let (status, answer) = dashboard
        .post(&format!("/api/chat/{id}/undo"), json!({"turn": "1"}))
        .await;
    assert_eq!((status, &answer["error"]), (400, &json!("bad_body")));
    let (status, answer) = dashboard
        .post(&format!("/api/chat/{id}/undo"), json!({"turn": 9}))
        .await;
    assert_eq!((status, &answer["error"]), (404, &json!("not_found")));
    assert!(made.exists());

    let (status, answer) = dashboard
        .post(&format!("/api/chat/{id}/undo"), json!({"turn": 1}))
        .await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer,
        json!({"restored": [made.display().to_string()], "skipped": [], "partial": []})
    );
    assert!(!made.exists());
    let log = std::fs::read_to_string(home.state().join("dashboard.log")).unwrap();
    assert!(
        log.contains(&format!(
            "chat {id} undo from turn 1: 1 restored, 0 skipped"
        )),
        "{log}"
    );
}

#[tokio::test]
async fn an_undo_waits_for_no_turn_and_needs_the_chat() {
    let home = Home::new();
    // An API that never answers: the turn keeps running.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let dashboard = Dashboard::start(&home, &url, true).await;
    let (_, opened) = dashboard.post("/api/chat", json!({})).await;
    let id = opened["session_id"].as_str().unwrap().to_string();
    let (status, _) = dashboard
        .post(&format!("/api/chat/{id}/messages"), json!({"text": "hi"}))
        .await;
    assert_eq!(status, 202);
    let (status, answer) = dashboard
        .post(&format!("/api/chat/{id}/undo"), json!({"turn": 1}))
        .await;
    assert_eq!(
        (status, &answer["error"]),
        (409, &json!("turn_running")),
        "{answer}"
    );

    let plain = Dashboard::start(&home, &url, false).await;
    let (status, _) = plain
        .post(&format!("/api/chat/{id}/undo"), json!({"turn": 1}))
        .await;
    assert_eq!(status, 404);
    let (status, _) = plain.get(&format!("/api/chat/{id}/checkpoints")).await;
    assert_eq!(status, 404);
}
