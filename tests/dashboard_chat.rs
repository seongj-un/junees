//! `junees dashboard --chat` as the page sees it: the real binary, a
//! loopback fake of the Anthropic API, the chat API over raw HTTP and its
//! event stream read as it arrives.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Child;

const WAIT: Duration = Duration::from_secs(60);
const PASTED_KEY: &str = "sk-ant-api03-ChatPastedKeyValue0123456789abcdefABCDEF";

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    work: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, work) = (root.join("state"), root.join("work"));
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&work).unwrap();
        Self {
            _dir: dir,
            root,
            state,
            work,
        }
    }

    fn command(&self, api: &str, args: &[&str]) -> tokio::process::Command {
        let mut command = common::junees_binary(&self.work, &self.root);
        command
            .arg("dashboard")
            .args(["--no-open", "--listen", "127.0.0.1:0"])
            .args(args)
            .env("JUNEES_HOME", &self.state)
            .env("ANTHROPIC_API_KEY", "sk-fake")
            .env("ANTHROPIC_BASE_URL", api)
            .env("JUNEES_STREAM", "0")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

struct Dashboard {
    _child: Child,
    port: u16,
    token: String,
}

impl Dashboard {
    async fn start(home: &Home, api: &str, args: &[&str]) -> Self {
        let mut child = home.command(api, args).spawn().unwrap();
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

    /// A request with `headers` (lines without the end) and `body`.
    async fn send(&self, method: &str, path: &str, headers: &str, body: &str) -> (u16, Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n",
            self.host(),
            body.len()
        );
        for line in headers.lines().filter(|line| !line.is_empty()) {
            request.push_str(line);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(WAIT, stream.read_to_end(&mut bytes))
            .await
            .expect("no answer")
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let (head, body) = text.split_once("\r\n\r\n").expect("no head");
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    fn page(&self) -> String {
        format!(
            "Origin: http://{}\nAuthorization: Bearer {}\nContent-Type: application/json",
            self.host(),
            self.token
        )
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send("POST", path, &self.page(), &body.to_string())
            .await
    }

    async fn status(&self) -> Value {
        let auth = format!("Authorization: Bearer {}", self.token);
        self.send("GET", "/api/status", &auth, "").await.1
    }

    /// The event stream of chat `id` after `after`.
    async fn events(&self, id: &str, after: Option<u64>) -> Events {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let query = after.map_or(String::new(), |after| format!("?after={after}"));
        stream
            .write_all(
                format!(
                    "GET /api/chat/{id}/events{query} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\r\n",
                    self.host(),
                    self.token
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        assert!(head.contains("content-type: text/event-stream"), "{head}");
        assert!(head.contains("cache-control: no-store"), "{head}");
        Events {
            reader,
            seen: Vec::new(),
        }
    }
}

/// An event stream as it arrives.
struct Events {
    reader: BufReader<TcpStream>,
    /// `(seq, event)` read so far.
    seen: Vec<(u64, Value)>,
}

impl Events {
    async fn next(&mut self) -> (u64, Value) {
        let mut seq = None;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(WAIT, self.reader.read_line(&mut line))
                .await
                .expect("no event")
                .unwrap();
            assert!(read > 0, "the stream ended");
            let line = line.trim_end_matches('\n');
            if let Some(id) = line.strip_prefix("id: ") {
                seq = Some(id.parse().unwrap());
            } else if let Some(data) = line.strip_prefix("data: ") {
                let event: Value = serde_json::from_str(data).unwrap();
                let seq = seq.expect("data without id");
                self.seen.push((seq, event.clone()));
                return (seq, event);
            }
        }
    }

    /// Events up to and including the first of `kind`.
    async fn until(&mut self, kind: &str) -> Value {
        loop {
            let (_, event) = self.next().await;
            if event["type"] == kind {
                return event;
            }
        }
    }

    fn kinds(&self) -> Vec<String> {
        self.seen
            .iter()
            .map(|(_, event)| event["type"].as_str().unwrap().to_string())
            .collect()
    }

    fn all_text(&self) -> String {
        self.seen
            .iter()
            .map(|(_, event)| event.to_string())
            .collect()
    }
}

fn tool_use(id: &str, command: &str) -> Value {
    json!({
        "id": "msg_t", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": id, "name": "bash", "input": {"command": command}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

/// An API that takes requests and never answers.
async fn silent_api() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    api
}

fn exists(dir: &Path, name: &str) -> bool {
    dir.join(name).exists()
}

#[tokio::test]
async fn a_chat_streams_its_turns_and_waits_for_the_page_on_approvals() {
    let home = Home::new();
    std::fs::write(home.work.join("one.txt"), "x").unwrap();
    std::fs::write(home.work.join("two.txt"), "x").unwrap();
    let (api, served) = common::fake_api(vec![
        (200, tool_use("toolu_1", "rm one.txt")),
        (
            200,
            common::text_reply(&format!("Removed it. The key was {PASTED_KEY} by the way.")),
        ),
        (200, tool_use("toolu_2", "rm two.txt")),
        (200, common::text_reply("Kept it.")),
    ])
    .await;
    let dashboard = Dashboard::start(&home, &api, &["--chat"]).await;
    assert_eq!(dashboard.status().await["chat"], true);

    let (status, opened) = dashboard.post("/api/chat", json!({})).await;
    assert_eq!(status, 200, "{opened}");
    let id = opened["session_id"].as_str().unwrap().to_string();
    let mut events = dashboard.events(&id, None).await;

    // Turn 1: the question waits; another message meanwhile is 409.
    let (status, turn) = dashboard
        .post(
            &format!("/api/chat/{id}/messages"),
            json!({"text": "remove one.txt"}),
        )
        .await;
    assert_eq!((status, turn), (202, json!({"turn": 1})));
    assert_eq!(events.until("turn_start").await["turn"], 1);
    let used = events.until("tool_use").await;
    assert_eq!(used["name"], "bash");
    assert_eq!(used["input_preview"], "rm one.txt");
    let question = events.until("approval").await;
    assert_eq!(question["kind"], "bash");
    assert_eq!(question["options"], json!(["y", "a", "n"]));
    assert!(question["summary"].as_str().unwrap().contains("rm one.txt"));
    let question_id = question["question_id"].as_str().unwrap().to_string();
    let (status, busy) = dashboard
        .post(
            &format!("/api/chat/{id}/messages"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!(
        (status, busy["error"].clone()),
        (409, json!("turn_running"))
    );
    assert!(
        exists(&home.work, "one.txt"),
        "nothing runs before the answer"
    );
    let approvals = format!("/api/chat/{id}/approvals/{question_id}");
    let (status, answered) = dashboard.post(&approvals, json!({"answer": "y"})).await;
    assert_eq!(status, 200, "{answered}");
    let closed = events.until("approval_closed").await;
    assert_eq!(closed["by"], "browser");
    assert_eq!(closed["answer"], "y");
    let result = events.until("tool_result").await;
    assert_eq!(result["is_error"], false, "{result}");
    let usage = events.until("usage").await;
    assert_eq!(
        (
            usage["input_tokens"].clone(),
            usage["output_tokens"].clone()
        ),
        (json!(8), json!(3))
    );
    let end = events.until("turn_end").await;
    assert_eq!(end["stop_reason"], "end_turn");
    assert!(!exists(&home.work, "one.txt"), "the approved command ran");
    // Answered already, and a question that does not exist.
    assert_eq!(
        dashboard.post(&approvals, json!({"answer": "n"})).await.0,
        409
    );
    let missing = format!("/api/chat/{id}/approvals/nope");
    assert_eq!(
        dashboard.post(&missing, json!({"answer": "n"})).await.0,
        404
    );
    let text: String = events
        .seen
        .iter()
        .filter(|(_, event)| event["type"] == "text")
        .map(|(_, event)| event["delta"].as_str().unwrap().to_string())
        .collect();
    assert!(text.starts_with("Removed it."), "{text}");
    let last_of_turn_one = events.seen.last().unwrap().0;

    // Turn 2: declined, the command does not run.
    let (status, _) = dashboard
        .post(
            &format!("/api/chat/{id}/messages"),
            json!({"text": "remove two.txt"}),
        )
        .await;
    assert_eq!(status, 202);
    let question = events.until("approval").await;
    let approvals = format!(
        "/api/chat/{id}/approvals/{}",
        question["question_id"].as_str().unwrap()
    );
    assert_eq!(
        dashboard.post(&approvals, json!({"answer": "n"})).await.0,
        200
    );
    let result = events.until("tool_result").await;
    assert_eq!(result["is_error"], true);
    assert!(
        result["text_preview"]
            .as_str()
            .unwrap()
            .contains("declined")
    );
    assert_eq!(events.until("turn_end").await["stop_reason"], "end_turn");
    assert!(
        exists(&home.work, "two.txt"),
        "the declined command did not run"
    );

    // A new stream after turn one sees turn two only.
    let mut again = dashboard.events(&id, Some(last_of_turn_one)).await;
    let (seq, first) = again.next().await;
    assert_eq!(seq, last_of_turn_one + 1);
    assert_eq!(first, json!({"type": "turn_start", "turn": 2}));
    again.until("turn_end").await;
    assert_eq!(
        again.kinds(),
        events.kinds()[events.kinds().len() - again.kinds().len()..]
    );

    // No secret in any event, nor message text in the log.
    let everything = events.all_text();
    assert!(
        !everything.contains(PASTED_KEY) && !everything.contains(&PASTED_KEY[..24]),
        "{everything}"
    );
    let log = std::fs::read_to_string(home.state.join("dashboard.log")).unwrap();
    assert!(log.contains("chat started"), "{log}");
    assert!(
        log.contains(&format!("chat {id} message 1"))
            && log.contains(&format!("chat {id} message 2")),
        "{log}"
    );
    assert!(log.contains(&format!("approval {question_id} y")), "{log}");
    assert!(
        !log.contains("remove one.txt") && !log.contains(&dashboard.token),
        "{log}"
    );

    // The session is an ordinary one of the folder, and continues.
    let journal = home.state.join("sessions").join(format!("{id}.jsonl"));
    let meta: Value = serde_json::from_str(
        std::fs::read_to_string(&journal)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(meta["cwd"], home.work.to_str().unwrap());
    assert!(meta.get("source").is_none_or(Value::is_null), "{meta}");
    let (status, same) = dashboard.post("/api/chat", json!({"session_id": id})).await;
    assert_eq!((status, same["session_id"].clone()), (200, json!(id)));

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert_eq!(requests.len(), 4);
    let system = common::body_of(&requests[0])["system"].to_string();
    assert!(system.contains("Junees dashboard"), "{system}");
}

#[tokio::test]
async fn a_cancel_stops_the_turn_and_closes_its_question() {
    let home = Home::new();
    std::fs::write(home.work.join("one.txt"), "x").unwrap();
    let (api, _served) = common::fake_api(vec![(200, tool_use("toolu_1", "rm one.txt"))]).await;
    let dashboard = Dashboard::start(&home, &api, &["--chat"]).await;
    let id = dashboard.post("/api/chat", json!({})).await.1["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut events = dashboard.events(&id, None).await;
    // Nothing runs: cancelled is false.
    let cancel = format!("/api/chat/{id}/cancel");
    assert_eq!(
        dashboard.post(&cancel, json!({})).await,
        (200, json!({"cancelled": false}))
    );
    dashboard
        .post(
            &format!("/api/chat/{id}/messages"),
            json!({"text": "remove it"}),
        )
        .await;
    events.until("approval").await;
    assert_eq!(
        dashboard.post(&cancel, json!({})).await,
        (200, json!({"cancelled": true}))
    );
    let closed = events.until("approval_closed").await;
    assert_eq!(
        (closed["answer"].clone(), closed["by"].clone()),
        (json!("n"), json!("cancel"))
    );
    assert_eq!(events.until("turn_end").await["stop_reason"], "cancelled");
    assert!(exists(&home.work, "one.txt"));
}

#[tokio::test]
async fn a_cancel_stops_a_turn_waiting_for_the_model() {
    let home = Home::new();
    let api = silent_api().await;
    let dashboard = Dashboard::start(&home, &api, &["--chat"]).await;
    let id = dashboard.post("/api/chat", json!({})).await.1["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut events = dashboard.events(&id, None).await;
    dashboard
        .post(&format!("/api/chat/{id}/messages"), json!({"text": "slow"}))
        .await;
    events.until("turn_start").await;
    let cancel = format!("/api/chat/{id}/cancel");
    assert_eq!(
        dashboard.post(&cancel, json!({})).await.1,
        json!({"cancelled": true})
    );
    assert_eq!(events.until("turn_end").await["stop_reason"], "cancelled");
    // The chat takes the next message.
    let (status, turn) = dashboard
        .post(
            &format!("/api/chat/{id}/messages"),
            json!({"text": "again"}),
        )
        .await;
    assert_eq!((status, turn), (202, json!({"turn": 2})));
}

#[tokio::test]
async fn the_chat_api_is_guarded() {
    let home = Home::new();
    let api = silent_api().await;
    // Without --chat: 404 everywhere, and the status says so.
    let plain = Dashboard::start(&home, &api, &[]).await;
    assert_eq!(plain.status().await["chat"], false);
    for (method, path) in [
        ("POST", "/api/chat"),
        ("POST", "/api/chat/x/messages"),
        ("GET", "/api/chat/x/events"),
        ("POST", "/api/chat/x/cancel"),
        ("POST", "/api/chat/x/approvals/q"),
    ] {
        let (status, body) = plain.send(method, path, &plain.page(), "{}").await;
        assert_eq!(
            (status, body["error"].clone()),
            (404, json!("not_found")),
            "{method} {path}"
        );
    }

    let dashboard = Dashboard::start(&home, &api, &["--chat"]).await;
    let host = dashboard.host();
    let token = dashboard.token.clone();
    // A write without Origin, from another site, without the token.
    let no_origin = format!("Authorization: Bearer {token}\nContent-Type: application/json");
    assert_eq!(
        dashboard
            .send("POST", "/api/chat", &no_origin, "{}")
            .await
            .0,
        403
    );
    let other = format!("Origin: http://evil.example\nAuthorization: Bearer {token}");
    assert_eq!(
        dashboard.send("POST", "/api/chat", &other, "{}").await.0,
        403
    );
    let no_token = format!("Origin: http://{host}\nContent-Type: application/json");
    assert_eq!(
        dashboard.send("POST", "/api/chat", &no_token, "{}").await.0,
        401
    );
    assert_eq!(
        dashboard
            .send("GET", "/api/chat", &dashboard.page(), "")
            .await
            .0,
        405
    );
    let id = dashboard.post("/api/chat", json!({})).await.1["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let messages = format!("/api/chat/{id}/messages");
    // Bodies: shape, length, size.
    for body in [
        json!({}),
        json!({"text": ""}),
        json!({"text": "  "}),
        json!({"text": 1}),
        json!({"text": "x", "more": 1}),
        json!({"text": "x".repeat(32_001)}),
    ] {
        let (status, error) = dashboard.post(&messages, body.clone()).await;
        assert_eq!(
            (status, error["error"].clone()),
            (400, json!("bad_body")),
            "{body}"
        );
    }
    let huge = format!("{{\"text\": \"{}\"}}", "x".repeat(600 * 1024));
    assert_eq!(
        dashboard
            .send("POST", &messages, &dashboard.page(), &huge)
            .await
            .0,
        413
    );
    assert_eq!(
        dashboard
            .post("/api/chat/nope/messages", json!({"text": "x"}))
            .await
            .0,
        404
    );
    let auth = format!("Authorization: Bearer {token}");
    let (status, error) = dashboard
        .send("GET", &format!("/api/chat/{id}/events?after=-1"), &auth, "")
        .await;
    assert_eq!((status, error["error"].clone()), (400, json!("bad_query")));
    // Sessions of other folders, links and unknown ids are not continued.
    let sessions = home.state.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let meta = json!({"type": "meta", "id": "elsewhere", "format": 8, "created_at": 1,
                      "cwd": "/somewhere/else", "model": "m", "version": "0"});
    std::fs::write(sessions.join("elsewhere.jsonl"), format!("{meta}\n")).unwrap();
    for wanted in ["elsewhere", "missing", "../x"] {
        let (status, _) = dashboard
            .post("/api/chat", json!({"session_id": wanted}))
            .await;
        assert!(status == 404 || status == 400, "{wanted}: {status}");
    }
}

#[tokio::test]
async fn the_claude_code_backend_is_refused() {
    let home = Home::new();
    let output = home
        .command("http://127.0.0.1:9", &["--chat"])
        .env("JUNEES_BACKEND", "claude-code")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("claude-code backend"), "{stderr}");
}

#[tokio::test]
async fn with_chat_no_browser_is_opened_unless_asked() {
    let home = Home::new();
    let bin = home.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let opened = home.root.join("opened");
    for name in ["xdg-open", "open"] {
        let script = bin.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", opened.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let mut command = common::junees_binary(&home.work, &home.root);
    let mut child = command
        .args(["dashboard", "--listen", "127.0.0.1:0", "--chat"])
        .env("JUNEES_HOME", &home.state)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("PATH", &path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        first.starts_with("Junees dashboard: http://127.0.0.1:") && first.contains("/#token="),
        "{first}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!opened.exists(), "no opener ran");
    drop(child);

    // --open opens, with a warning.
    let mut command = common::junees_binary(&home.work, &home.root);
    let mut child = command
        .args(["dashboard", "--listen", "127.0.0.1:0", "--chat", "--open"])
        .env("JUNEES_HOME", &home.state)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("PATH", &path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.contains("opening"), "{first}");
    let mut errors = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut warned = String::new();
    while let Ok(Ok(Some(line))) =
        tokio::time::timeout(Duration::from_secs(5), errors.next_line()).await
    {
        if line.contains("--open with --chat") {
            warned = line;
            break;
        }
    }
    assert!(warned.contains("command line"), "{warned}");
}

#[tokio::test]
async fn closed_streams_free_their_place_at_once() {
    let home = Home::new();
    let api = silent_api().await;
    let dashboard = Dashboard::start(&home, &api, &["--chat"]).await;
    let id = dashboard.post("/api/chat", json!({})).await.1["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    for _ in 0..3 {
        let streams: Vec<Events> = {
            let mut streams = Vec::new();
            for _ in 0..8 {
                streams.push(dashboard.events(&id, None).await);
            }
            streams
        };
        drop(streams);
        // Freed within a moment, not after the 30 s write timeout.
        let mut opened = false;
        for _ in 0..40 {
            let auth = format!("Authorization: Bearer {}", dashboard.token);
            let mut stream = TcpStream::connect(("127.0.0.1", dashboard.port))
                .await
                .unwrap();
            stream
                .write_all(
                    format!(
                        "GET /api/chat/{id}/events HTTP/1.1\r\nHost: {}\r\n{auth}\r\n\r\n",
                        dashboard.host()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut head = [0u8; 12];
            stream.read_exact(&mut head).await.unwrap();
            if &head == b"HTTP/1.1 200" {
                opened = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(opened, "a place was free within 2 s");
    }
}

#[tokio::test]
async fn a_chat_closed_for_room_ends_its_stream_and_numbers_on_when_opened_again() {
    let home = Home::new();
    let (api, _served) = common::fake_api(vec![
        (200, common::text_reply("first")),
        (200, common::text_reply("second")),
    ])
    .await;
    let dashboard = Dashboard::start(&home, &api, &["--chat"]).await;
    let id = dashboard.post("/api/chat", json!({})).await.1["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut events = dashboard.events(&id, None).await;
    dashboard
        .post(&format!("/api/chat/{id}/messages"), json!({"text": "one"}))
        .await;
    events.until("turn_end").await;
    let last = events.seen.last().unwrap().0;
    // 16 more chats close the one unused longest: this one.
    for _ in 0..16 {
        assert_eq!(dashboard.post("/api/chat", json!({})).await.0, 200);
    }
    let mut rest = String::new();
    let ended = tokio::time::timeout(WAIT, events.reader.read_to_string(&mut rest)).await;
    assert!(matches!(ended, Ok(Ok(_))), "the closed chat's stream ended");
    assert_eq!(
        dashboard
            .post(&format!("/api/chat/{id}/messages"), json!({"text": "x"}))
            .await
            .0,
        404
    );
    // Opened again by its session id, it numbers on from the page's `after`.
    let (status, again) = dashboard.post("/api/chat", json!({"session_id": id})).await;
    assert_eq!((status, again["session_id"].clone()), (200, json!(id)));
    let mut events = dashboard.events(&id, Some(last)).await;
    dashboard
        .post(&format!("/api/chat/{id}/messages"), json!({"text": "two"}))
        .await;
    let (seq, first) = events.next().await;
    assert!(seq > last, "{seq} > {last}");
    assert_eq!(first, json!({"type": "turn_start", "turn": 1}));
    events.until("turn_end").await;
    // An `after` from another run of the dashboard starts from the oldest.
    let mut fresh = dashboard.events(&id, Some(u64::MAX / 2)).await;
    assert_eq!(fresh.next().await.1["type"], "turn_start");
}
