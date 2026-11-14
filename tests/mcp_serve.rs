//! `junees mcp serve` as another agent sees it: the real binary on stdio,
//! JSON-RPC lines in and out, a loopback fake of the Anthropic API.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const WAIT: Duration = Duration::from_secs(60);

struct Served {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    /// Every line read from stdout.
    seen: Vec<Value>,
}

/// A temp HOME with `work/` (the server's folder) and `.junees/`.
fn home() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.path().join(".junees")).unwrap();
    (home, work)
}

fn command(home: &Path, work: &Path, api: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(work, home);
    command
        .arg("mcp")
        .arg("serve")
        .args(args)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", api)
        .env("JUNEES_HOME", home.join(".junees"))
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

impl Served {
    fn start(home: &Path, work: &Path, api: &str, args: &[&str]) -> Self {
        let mut child = command(home, work, api, args).spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
            seen: Vec::new(),
        }
    }

    async fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    /// The next stdout line, which must be a JSON-RPC 2.0 message.
    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(WAIT, self.stdout.next_line())
            .await
            .expect("no message on stdout")
            .unwrap()
            .expect("stdout closed");
        let message: Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("stdout holds a line that is not JSON: {line}"));
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
        self.seen.push(message.clone());
        message
    }

    /// The answer to request `id`; notifications on the way are kept in
    /// `seen`.
    async fn answer(&mut self, id: i64) -> Value {
        loop {
            let message = self.next().await;
            if message["id"] == json!(id) {
                return message;
            }
            assert!(
                message.get("id").is_none(),
                "an answer to another request: {message}"
            );
        }
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.answer(id).await
    }

    async fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )
        .await
    }

    async fn initialize(&mut self, version: &str) -> Value {
        let answer = self
            .request(
                1,
                "initialize",
                json!({"protocolVersion": version, "capabilities": {},
                       "clientInfo": {"name": "test", "version": "0"}}),
            )
            .await;
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        answer
    }

    /// Closes stdin and waits for the exit; stdout must hold nothing but
    /// protocol messages to its end.
    async fn close(mut self) -> (std::process::ExitStatus, String, Vec<Value>) {
        drop(self.stdin.take());
        let mut rest = Vec::new();
        while let Ok(Some(line)) = tokio::time::timeout(WAIT, self.stdout.next_line())
            .await
            .unwrap()
        {
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("stdout holds a line that is not JSON: {line}"));
            assert_eq!(message["jsonrpc"], "2.0");
            rest.push(message);
        }
        let output = tokio::time::timeout(WAIT, self.child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        (
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
            rest,
        )
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({
        "id": "msg_t", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

#[tokio::test]
async fn a_run_round_trip_continues_its_session_and_shows_it() {
    let (home, work) = home();
    let (api, served) = common::fake_api(vec![
        (
            200,
            tool_use("toolu_1", "bash", json!({"command": "echo hi"})),
        ),
        (200, common::text_reply("Done: the answer is 42.")),
        (200, common::text_reply("Still here.")),
    ])
    .await;
    let mut server = Served::start(home.path(), &work, &api, &[]);
    let hello = server.initialize("2025-03-26").await;
    assert_eq!(hello["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(hello["result"]["serverInfo"]["name"], "junees");
    assert!(hello["result"]["capabilities"]["tools"].is_object());

    let list = server.request(2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["junees_run", "junees_sessions", "junees_session"]);
    let about = tools[0]["description"].as_str().unwrap();
    assert!(about.contains("read_file, grep, glob, list_dir"), "{about}");
    assert!(
        about.contains("read_file") && !about.contains("bash") && !about.contains("write_file"),
        "{about}"
    );
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["prompt"]));

    // Bad input is a tool error, an unknown tool a protocol error.
    let bad = server.call(3, "junees_run", json!({})).await;
    assert_eq!(bad["result"]["isError"], true, "{bad}");
    let bad = server
        .call(4, "junees_run", json!({"prompt": "x", "turns": 3}))
        .await;
    assert!(
        bad["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown argument `turns`")
    );
    let unknown = server.call(5, "rm_everything", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    // A run: the model asks for bash, which this server does not give.
    let run = server
        .request(
            6,
            "tools/call",
            json!({"name": "junees_run", "arguments": {"prompt": "what is the answer?"},
                   "_meta": {"progressToken": "p-1"}}),
        )
        .await;
    let result = &run["result"];
    assert_eq!(result["isError"], false, "{run}");
    let content = &result["structuredContent"];
    assert_eq!(content["text"], "Done: the answer is 42.");
    assert_eq!(content["stop_reason"], "end_turn");
    assert_eq!(content["input_tokens"], 8);
    assert_eq!(content["output_tokens"], 3);
    assert_eq!(result["content"][0]["text"], "Done: the answer is 42.");
    let session_id = content["session_id"].as_str().unwrap().to_string();
    // The other fields reach clients that read `content` only.
    let fields: Value =
        serde_json::from_str(result["content"][1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(fields["session_id"], content["session_id"], "{run}");
    assert_eq!(fields["stop_reason"], "end_turn");
    assert!(fields.get("text").is_none(), "{fields}");
    let progress: Vec<&Value> = server
        .seen
        .iter()
        .filter(|message| message["method"] == "notifications/progress")
        .collect();
    assert_eq!(progress.len(), 1, "{:?}", server.seen);
    assert_eq!(progress[0]["params"]["progressToken"], "p-1");
    assert_eq!(progress[0]["params"]["message"], "tool call: bash");

    // Continued: the model gets the earlier turn.
    let again = server
        .call(
            7,
            "junees_run",
            json!({"prompt": "and again?", "session_id": session_id}),
        )
        .await;
    assert_eq!(again["result"]["structuredContent"]["text"], "Still here.");
    assert_eq!(
        again["result"]["structuredContent"]["session_id"],
        json!(session_id)
    );
    // This call's usage, not the session's total.
    assert_eq!(again["result"]["structuredContent"]["input_tokens"], 3);
    assert_eq!(again["result"]["structuredContent"]["output_tokens"], 1);

    let sessions = server.call(8, "junees_sessions", json!({})).await;
    let listed = &sessions["result"]["structuredContent"]["sessions"];
    assert_eq!(listed[0]["id"], json!(session_id), "{sessions}");
    assert_eq!(listed[0]["title"], "what is the answer?");
    assert_eq!(listed[0]["source"], "mcp");
    let one = server
        .call(9, "junees_session", json!({"session_id": session_id}))
        .await;
    let page = &one["result"]["structuredContent"];
    assert!(page["total"].as_u64().unwrap() >= 6, "{one}");
    let missing = server
        .call(10, "junees_session", json!({"session_id": "nope"}))
        .await;
    assert_eq!(missing["result"]["isError"], true);
    let (status, stderr, _) = server.close().await;
    assert!(status.success(), "{stderr}");
    assert!(stderr.contains("stdin closed"), "{stderr}");

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert_eq!(requests.len(), 3);
    let first = common::body_of(&requests[0]);
    let offered: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for tool in ["read_file", "grep", "glob", "list_dir"] {
        assert!(offered.contains(&tool), "{offered:?}");
    }
    for left_out in ["bash", "write_file", "edit_file"] {
        assert!(!offered.contains(&left_out), "{offered:?}");
    }
    assert!(first["system"].to_string().contains("junees mcp serve"));
    // The bash call was answered as a tool error, without running.
    let second = common::body_of(&requests[1]);
    let answered = second["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    assert!(
        answered.contains("toolu_1") && answered.contains("\"is_error\":true"),
        "{answered}"
    );
    let third = common::body_of(&requests[2]).to_string();
    assert!(
        third.contains("what is the answer?") && third.contains("and again?"),
        "{third}"
    );
}

#[tokio::test]
async fn tools_follow_the_flags_and_versions_are_negotiated() {
    let (home, work) = home();
    let mut server = Served::start(
        home.path(),
        &work,
        "http://127.0.0.1:9",
        &["--allow-write", "--allow-bash"],
    );
    let early = server.request(1, "tools/list", json!({})).await;
    assert_eq!(early["error"]["code"], -32002, "{early}");
    let pong = server.request(2, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    let hello = server.initialize("1999-01-01").await;
    assert_eq!(hello["result"]["protocolVersion"], "2025-06-18");
    let list = server.request(3, "tools/list", json!({})).await;
    let about = list["result"]["tools"][0]["description"].as_str().unwrap();
    for tool in ["bash", "write_file", "edit_file"] {
        assert!(about.contains(tool), "{about}");
    }
    server
        .send(json!([{"jsonrpc": "2.0", "id": 4, "method": "ping"}]))
        .await;
    let batch = server.next().await;
    assert_eq!(batch["error"]["code"], -32600);
    server
        .send(json!({"jsonrpc": "2.0", "id": 5, "method": "resources/list"}))
        .await;
    assert_eq!(server.answer(5).await["error"]["code"], -32601);
    let (status, _, _) = server.close().await;
    assert!(status.success());
}

#[tokio::test]
async fn a_cancelled_run_stops_and_gets_no_answer() {
    let (home, work) = home();
    // An API that takes the request and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let (asked, mut asked_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            let _ = asked.send(());
            held.push(stream);
        }
    });
    let mut server = Served::start(home.path(), &work, &api, &[]);
    server.initialize("2025-06-18").await;
    server
        .send(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                     "params": {"name": "junees_run", "arguments": {"prompt": "slow"}}}))
        .await;
    // Waits behind the run.
    server
        .send(json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call",
                     "params": {"name": "junees_sessions", "arguments": {}}}))
        .await;
    tokio::time::timeout(WAIT, asked_rx.recv()).await.unwrap();
    // Answered while the run runs: only tools/call waits.
    let pong = server.request(9, "ping", json!({})).await;
    assert_eq!(pong["result"], json!({}));
    server
        .send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                     "params": {"requestId": 7, "reason": "test"}}),
        )
        .await;
    let listed = server.answer(8).await;
    assert!(
        listed["result"]["structuredContent"]["sessions"].is_array(),
        "{listed}"
    );
    let (status, stderr, _) = server.close().await;
    assert!(status.success(), "{stderr}");
    assert!(stderr.contains("cancelled by the client"), "{stderr}");
}

#[tokio::test]
async fn the_home_folder_and_claude_code_are_refused() {
    let (home, work) = home();
    let output = command(
        home.path(),
        &work,
        "http://127.0.0.1:9",
        &["--dir", home.path().to_str().unwrap()],
    )
    .output()
    .await
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("home folder"), "{stderr}");
    assert!(stderr.contains("--allow-home"), "{stderr}");

    let output = command(home.path(), &work, "http://127.0.0.1:9", &[])
        .env("JUNEES_BACKEND", "claude-code")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("claude-code backend"));
}

/// An API that takes requests and never answers; the receiver hears of
/// each connection.
async fn silent_api() -> (String, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let (asked, asked_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            let _ = asked.send(());
            held.push(stream);
        }
    });
    (api, asked_rx)
}

#[tokio::test]
async fn closing_stdin_stops_the_running_call_without_an_answer() {
    let (home, work) = home();
    let (api, mut asked) = silent_api().await;
    let mut server = Served::start(home.path(), &work, &api, &[]);
    server.initialize("2025-06-18").await;
    server
        .send(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                     "params": {"name": "junees_run", "arguments": {"prompt": "slow"}}}))
        .await;
    server
        .send(json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call",
                     "params": {"name": "junees_sessions", "arguments": {}}}))
        .await;
    tokio::time::timeout(WAIT, asked.recv()).await.unwrap();
    let (status, stderr, rest) = server.close().await;
    assert!(status.success(), "{stderr}");
    assert!(
        rest.iter().all(|message| message.get("id").is_none()),
        "{rest:?}"
    );
    assert!(stderr.contains("stopping the running call 7"), "{stderr}");
    assert!(stderr.contains("1 waiting call(s) dropped"), "{stderr}");
    assert!(stderr.contains("stdin closed"), "{stderr}");
}

#[tokio::test]
async fn an_unreadable_stdin_stops_the_server() {
    let (home, work) = home();
    // Reading a folder fails every time (EISDIR).
    let folder = std::fs::File::open(&work).unwrap();
    let output = tokio::time::timeout(
        WAIT,
        command(home.path(), &work, "http://127.0.0.1:9", &[])
            .stdin(Stdio::from(folder))
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("cannot read stdin").count(), 3, "{stderr}");
    assert!(stderr.contains("stdin cannot be read"), "{stderr}");
}
