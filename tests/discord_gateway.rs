//! The Discord gateway against a fake Gateway (a local WebSocket server)
//! and a fake REST API, both started inside the tests. Nothing connects
//! to Discord.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use junees::agent::Cancelled;
use junees::approval::Ask;
use junees::cancel::Cancel;
use junees::config::Config;
use junees::gateway::config::DiscordConfig;
use junees::gateway::engine::{ApiEngines, Conversation, Engines, Progress};
use junees::gateway::shard::Timing;
use junees::gateway::{Gateway, Log};
use junees::session::{Journal, Session, Transcript};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const WAIT: Duration = Duration::from_secs(15);
/// A token that is easy to look for in logs and errors.
const TOKEN: &str = "fake-bot-token-must-not-leak";
const ME: &str = "900";
const USER: &str = "111";
const STRANGER: &str = "222";
const GUILD: &str = "700";
/// A heartbeat interval the tests never reach, unless they test heartbeats.
const SLOW_BEAT: u64 = 600_000;

// ---------------------------------------------------------------- REST

/// A request the fake REST API received.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

#[derive(Default)]
struct RestState {
    requests: Mutex<Vec<Received>>,
    notify: Notify,
    /// Answer this many message posts with 429 first.
    limit_next: Mutex<usize>,
    /// Answer every request with 401.
    unauthorized: AtomicBool,
    gateway_url: Mutex<String>,
}

struct FakeRest {
    url: String,
    state: Arc<RestState>,
}

impl FakeRest {
    async fn start(gateway_url: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/v10", listener.local_addr().unwrap());
        let state = Arc::new(RestState::default());
        *state.gateway_url.lock().unwrap() = gateway_url.to_string();
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_rest(socket, Arc::clone(&shared)));
            }
        });
        Self { url, state }
    }

    fn requests(&self) -> Vec<Received> {
        self.state.requests.lock().unwrap().clone()
    }

    /// The contents posted to `channel` so far.
    fn posts(&self, channel: &str) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|r| {
                r.method == "POST" && r.path.ends_with(&format!("/channels/{channel}/messages"))
            })
            .filter(|r| !r.headers.contains_key("x-fake-status"))
            .map(|r| r.body["content"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Waits until a post to `channel` satisfies `want`; returns every post
    /// to it so far.
    async fn wait_post(&self, channel: &str, want: impl Fn(&str) -> bool) -> Vec<String> {
        self.wait_posts(channel, 1, want).await
    }

    /// Waits until `count` posts to `channel` satisfy `want`.
    async fn wait_posts(
        &self,
        channel: &str,
        count: usize,
        want: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let notified = self.state.notify.notified();
            let posts = self.posts(channel);
            if posts.iter().filter(|post| want(post)).count() >= count {
                return posts;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!("no such post to channel {channel}; posts: {posts:#?}");
            }
        }
    }

    fn typing(&self, channel: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.path.ends_with(&format!("/channels/{channel}/typing")))
            .count()
    }

    /// Waits until `channel` showed typing more than `before` times: a
    /// turn started there.
    async fn wait_typing(&self, channel: &str, before: usize) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let notified = self.state.notify.notified();
            if self.typing(channel) > before {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!("no turn started in channel {channel}");
            }
        }
    }
}

async fn serve_rest(mut socket: TcpStream, state: Arc<RestState>) {
    let Some(mut request) = read_http(&mut socket).await else {
        return;
    };
    let (status, body) = if state.unauthorized.load(Ordering::SeqCst) {
        (401, json!({"message": "401: Unauthorized", "code": 0}))
    } else if request.path.ends_with("/gateway/bot") {
        let url = state.gateway_url.lock().unwrap().clone();
        (200, json!({"url": url, "shards": 1}))
    } else if request.path.ends_with("/typing") {
        (204, Value::Null)
    } else if request.path.ends_with("/messages") {
        let limited = {
            let mut left = state.limit_next.lock().unwrap();
            let limited = *left > 0;
            *left = left.saturating_sub(1);
            limited
        };
        if limited {
            request.headers.insert("x-fake-status".into(), "429".into());
            (
                429,
                json!({"message": "You are being rate limited.", "retry_after": 0.2, "global": false}),
            )
        } else {
            (200, json!({"id": "1"}))
        }
    } else {
        (404, json!({"message": "404: Not Found", "code": 0}))
    };
    let extra = if status == 429 {
        "retry-after: 1\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset-after: 0.2\r\nx-ratelimit-scope: user\r\n"
    } else {
        "x-ratelimit-remaining: 4\r\nx-ratelimit-reset-after: 1\r\n"
    };
    let text = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let response = format!(
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\n\
         connection: close\r\n\r\n{text}",
        text.len()
    );
    state.requests.lock().unwrap().push(request);
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
    state.notify.notify_waiters();
}

async fn read_http(socket: &mut TcpStream) -> Option<Received> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head_end, length) = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            if buf.len() >= end + 4 + length {
                break (end, length);
            }
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_string()))
        .collect();
    let body = &buf[head_end + 4..head_end + 4 + length];
    Some(Received {
        method,
        path,
        headers,
        body: serde_json::from_slice(body).unwrap_or(Value::Null),
    })
}

// ------------------------------------------------------------- Gateway

struct FakeGateway {
    listener: TcpListener,
    url: String,
}

impl FakeGateway {
    async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        Self { listener, url }
    }

    // The handshake callback's error type is the library's.
    #[allow(clippy::result_large_err)]
    async fn accept(&self) -> Conn {
        let (socket, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("the gateway client did not connect")
            .unwrap();
        let mut query = String::new();
        let ws = tokio_tungstenite::accept_hdr_async(
            socket,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                query = request.uri().query().unwrap_or_default().to_string();
                Ok(response)
            },
        )
        .await
        .unwrap();
        assert_eq!(query, "v=10&encoding=json");
        Conn { ws, seq: 0 }
    }
}

struct Conn {
    ws: WebSocketStream<TcpStream>,
    seq: u64,
}

impl Conn {
    async fn send(&mut self, payload: Value) {
        self.ws
            .send(Message::Text(payload.to_string().into()))
            .await
            .unwrap();
    }

    /// The next payload of the client; `None` once it closed.
    async fn next(&mut self) -> Option<Value> {
        loop {
            let message = tokio::time::timeout(WAIT, self.ws.next())
                .await
                .expect("the client sent nothing")?;
            match message.ok()? {
                Message::Text(text) => return Some(serde_json::from_str(text.as_str()).unwrap()),
                Message::Close(_) => return None,
                _ => {}
            }
        }
    }

    /// The next heartbeat carrying `seq`; earlier ones are acknowledged.
    async fn beat(&mut self, seq: u64) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let beat = self.expect(1).await;
            if beat["d"] == seq {
                return beat;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no heartbeat with {seq}, last {beat}"
            );
            self.send(json!({"op": 11})).await;
        }
    }

    /// The next payload with `op`, answering heartbeats on the way.
    async fn expect(&mut self, op: u64) -> Value {
        loop {
            let payload = self.next().await.expect("the client closed");
            if payload["op"] == op {
                return payload;
            }
            if payload["op"] == 1 {
                self.send(json!({"op": 11})).await;
            }
        }
    }

    async fn hello(&mut self, interval: u64) {
        self.send(json!({"op": 10, "d": {"heartbeat_interval": interval}}))
            .await;
    }

    async fn dispatch(&mut self, name: &str, data: Value) {
        self.seq += 1;
        let seq = self.seq;
        self.send(json!({"op": 0, "t": name, "s": seq, "d": data}))
            .await;
    }

    /// Hello, the client's Identify, and Ready.
    async fn start(&mut self, resume_url: &str) -> Value {
        self.hello(SLOW_BEAT).await;
        let identify = self.expect(2).await;
        self.dispatch(
            "READY",
            json!({
                "v": 10,
                "user": {"id": ME, "username": "junees", "bot": true},
                "session_id": "session-1",
                "resume_gateway_url": resume_url,
                "guilds": [],
            }),
        )
        .await;
        identify
    }

    async fn close(&mut self, code: u16) {
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        let _ = self.ws.send(Message::Close(Some(frame))).await;
    }
}

/// A user message: in a DM without `guild`, else in that server.
fn message(channel: &str, author: &str, content: &str, guild: bool, mentions: &[&str]) -> Value {
    let mut data = json!({
        "id": "1",
        "type": 0,
        "channel_id": channel,
        "author": {"id": author, "username": "someone"},
        "content": content,
        "mentions": mentions.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
    });
    if guild {
        data["guild_id"] = json!(GUILD);
    }
    data
}

// -------------------------------------------------------------- Engines

/// Conversations that answer at once, with what they were asked.
struct FakeEngines {
    /// How long `slow` inputs take.
    slow: Duration,
}

impl Engines for FakeEngines {
    fn describe(&self) -> String {
        "fake backend".into()
    }
    fn session_backend(&self) -> Option<&'static str> {
        None
    }
    fn model(&self) -> String {
        "fake".into()
    }
    fn asks(&self) -> bool {
        false
    }
    fn open(&self, _ask: Box<dyn Ask>, cancel: Cancel) -> Result<Box<dyn Conversation>> {
        Ok(Box::new(FakeConversation {
            cancel,
            turns: 0,
            slow: self.slow,
        }))
    }
}

struct FakeConversation {
    cancel: Cancel,
    turns: usize,
    slow: Duration,
}

#[async_trait(?Send)]
impl Conversation for FakeConversation {
    async fn turn(&mut self, input: &str, out: &UnboundedSender<Progress>) -> Result<()> {
        if input == "wait" {
            self.cancel.cancelled().await;
            return Err(Cancelled.into());
        }
        if input.starts_with("slow") {
            tokio::select! {
                _ = tokio::time::sleep(self.slow) => {}
                _ = self.cancel.cancelled() => return Err(Cancelled.into()),
            }
        }
        if input == "long" {
            let code: Vec<String> = (0..300).map(|n| format!("let value_{n} = {n};")).collect();
            let text = format!(
                "Here it is:\n```rust\n{}\n```\nThat was long.",
                code.join("\n")
            );
            out.send(Progress::Text(text)).unwrap();
            return Ok(());
        }
        self.turns += 1;
        out.send(Progress::Tool("-> bash echo hi".into())).unwrap();
        out.send(Progress::Text(format!("reply {}: {input}", self.turns)))
            .unwrap();
        Ok(())
    }
    fn resume(&mut self, _: Transcript, _: Option<Box<dyn Journal>>) {}
    fn reset(&mut self, _: Option<Box<dyn Journal>>) {
        self.turns = 0;
    }
    fn usage(&self) -> String {
        format!("{} fake turns", self.turns)
    }
}

// --------------------------------------------------------------- Setup

fn discord(api: &str, extra: &[(&str, &str)]) -> DiscordConfig {
    let mut map: HashMap<String, String> = [
        ("JUNEES_DISCORD_TOKEN", TOKEN),
        ("JUNEES_DISCORD_ALLOWED_USERS", USER),
        ("JUNEES_DISCORD_CHANNELS", "600"),
        ("JUNEES_DISCORD_API_URL", api),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (key, value) in extra {
        map.insert(key.to_string(), value.to_string());
    }
    DiscordConfig::from_lookup(|key| map.get(key).cloned()).unwrap()
}

fn timing() -> Timing {
    Timing {
        backoff_start: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        invalid_session: (Duration::from_millis(50), Duration::from_millis(100)),
        connect_timeout: Duration::from_secs(5),
    }
}

/// Collects the log lines.
fn capture() -> (Log, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let log: Log = Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_string()));
    (log, lines)
}

struct Running {
    stop: Cancel,
    task: oneshot::Receiver<Result<()>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Running {
    async fn stop(self) -> Vec<String> {
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("the gateway did not stop")
            .unwrap()
            .unwrap();
        self.log.lock().unwrap().clone()
    }
}

fn start(
    config: DiscordConfig,
    engines: Arc<dyn Engines>,
    home: Option<PathBuf>,
    cwd: &Path,
) -> Running {
    let (log, lines) = capture();
    let gateway = Gateway {
        config,
        engines,
        home,
        cwd: cwd.to_path_buf(),
        memory: None,
        timing: timing(),
        typing_every: Duration::from_secs(3600),
        log,
    };
    let stop = Cancel::new();
    let cancel = stop.clone();
    // Its channels run on one thread (a LocalSet), so the future is not
    // Send: give it a thread and a runtime of its own, as `main` does.
    let (done, task) = oneshot::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = done.send(runtime.block_on(gateway.run(cancel)));
    });
    Running {
        stop,
        task,
        log: lines,
    }
}

fn fake(slow: Duration) -> Arc<dyn Engines> {
    Arc::new(FakeEngines { slow })
}

// --------------------------------------------------------------- Tests

#[tokio::test]
async fn identifies_heartbeats_resumes_and_starts_over() {
    let gateway = FakeGateway::bind().await;
    let resume = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );

    // Hello -> Identify -> Ready, then heartbeats with the last sequence.
    let mut conn = gateway.accept().await;
    conn.hello(100).await;
    let identify = conn.expect(2).await;
    assert_eq!(identify["d"]["token"], TOKEN);
    assert_eq!(identify["d"]["intents"], 1 + 512 + 4096 + 32768);
    assert_eq!(identify["d"]["properties"]["browser"], "junees");
    conn.dispatch(
        "READY",
        json!({"user": {"id": ME, "username": "junees"}, "session_id": "abc",
               "resume_gateway_url": resume.url}),
    )
    .await;
    conn.beat(1).await;
    conn.send(json!({"op": 11})).await;
    conn.dispatch("TYPING_START", json!({})).await;
    conn.beat(2).await;
    conn.send(json!({"op": 11})).await;

    // No ACK: the client closes (not with 1000) and resumes on the resume URL.
    let beat = conn.expect(1).await;
    assert_eq!(beat["d"], 2);
    let closed = loop {
        match tokio::time::timeout(WAIT, conn.ws.next()).await.unwrap() {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(Message::Text(text))) => panic!("still sending after a missed ACK: {text}"),
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(closed.map(|frame| u16::from(frame.code)), Some(4000));
    let mut conn = resume.accept().await;
    conn.hello(SLOW_BEAT).await;
    let resumed = conn.expect(6).await;
    assert_eq!(resumed["d"]["token"], TOKEN);
    assert_eq!(resumed["d"]["session_id"], "abc");
    assert_eq!(resumed["d"]["seq"], 2);
    conn.seq = 2;
    conn.dispatch("RESUMED", json!({})).await;
    // Discord asks for a heartbeat: one comes at once, not after the
    // (long) interval.
    conn.send(json!({"op": 1, "d": null})).await;
    let asked = tokio::time::Instant::now();
    assert_eq!(conn.expect(1).await["d"], 3);
    assert!(asked.elapsed() < Duration::from_secs(5));
    conn.send(json!({"op": 11})).await;

    // Reconnect (op 7): resume again, on the resume URL.
    conn.send(json!({"op": 7, "d": null})).await;
    let mut conn = resume.accept().await;
    conn.hello(SLOW_BEAT).await;
    assert_eq!(conn.expect(6).await["d"]["seq"], 3);

    // Invalid Session, not resumable: a new Identify on the first URL.
    conn.send(json!({"op": 9, "d": false})).await;
    let mut conn = gateway.accept().await;
    conn.hello(SLOW_BEAT).await;
    let identify = conn.expect(2).await;
    assert_eq!(identify["d"]["token"], TOKEN);

    // A close code that allows it resumes; 4009 starts over.
    conn.dispatch(
        "READY",
        json!({"user": {"id": ME}, "session_id": "def", "resume_gateway_url": resume.url}),
    )
    .await;
    conn.close(4000).await;
    let mut conn = resume.accept().await;
    conn.hello(SLOW_BEAT).await;
    assert_eq!(conn.expect(6).await["d"]["session_id"], "def");
    conn.close(4009).await;
    let mut conn = gateway.accept().await;
    conn.hello(SLOW_BEAT).await;
    conn.expect(2).await;

    let log = running.stop().await;
    // Shutting down closes with 1000, which ends the session.
    let closed = loop {
        match tokio::time::timeout(WAIT, conn.ws.next()).await.unwrap() {
            Some(Ok(Message::Close(frame))) => break frame,
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(closed.map(|frame| u16::from(frame.code)), Some(1000));
    let text = log.join("\n");
    assert!(text.contains("no heartbeat ACK"), "{text}");
    assert!(text.contains("invalid session"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn a_refused_token_stops_without_showing_it() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.hello(SLOW_BEAT).await;
    conn.expect(2).await;
    conn.close(4004).await;
    let err = tokio::time::timeout(WAIT, running.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("4004"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    let log = running.log.lock().unwrap().join("\n");
    assert!(!log.contains(TOKEN), "{log}");

    // A REST 401 at the start: the error names the route, not the token.
    rest.state.unauthorized.store(true, Ordering::SeqCst);
    let (log, lines) = capture();
    let gateway = Gateway {
        log,
        timing: timing(),
        ..Gateway::new(
            discord(&rest.url, &[]),
            fake(Duration::ZERO),
            None,
            dir.path().to_path_buf(),
            None,
        )
    };
    let err = gateway.run(Cancel::new()).await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("401"), "{text}");
    assert!(text.contains("GET /gateway/bot"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!lines.lock().unwrap().join("\n").contains(TOKEN));
    // The token went in the Authorization header, as Discord wants it.
    let request = &rest.requests()[0];
    assert_eq!(request.headers["authorization"], format!("Bot {TOKEN}"));
    assert!(
        request.headers["user-agent"]
            .starts_with("DiscordBot (https://github.com/seongj-un/junees, ")
    );
}

#[tokio::test]
async fn answers_only_allowed_users_where_it_is_addressed() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[("JUNEES_DISCORD_PARALLEL", "8")]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;

    // Ignored: a stranger (DM and mention), the bot itself, a server
    // channel without a mention, a mention of someone else.
    conn.dispatch("MESSAGE_CREATE", message("501", STRANGER, "hi", false, &[]))
        .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("502", STRANGER, &format!("<@{ME}> hi"), true, &[ME]),
    )
    .await;
    let mut own = message("503", ME, "echo", false, &[]);
    own["author"]["bot"] = json!(true);
    conn.dispatch("MESSAGE_CREATE", own).await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("504", USER, "no mention", true, &[]),
    )
    .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("504", USER, "<@1> other", true, &["1"]),
    )
    .await;
    // Answered: a DM, a mention (without the mention text), a listed
    // channel, and a thread under it.
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "dm", false, &[]))
        .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("504", USER, &format!("<@{ME}> mentioned"), true, &[ME]),
    )
    .await;
    conn.dispatch("MESSAGE_CREATE", message("600", USER, "listed", true, &[]))
        .await;
    conn.dispatch(
        "THREAD_CREATE",
        json!({"id": "601", "parent_id": "600", "owner_id": USER}),
    )
    .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("601", USER, "in thread", true, &[]),
    )
    .await;
    conn.dispatch(
        "THREAD_CREATE",
        json!({"id": "650", "parent_id": "649", "owner_id": ME}),
    )
    .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("650", USER, "own thread", true, &[]),
    )
    .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("651", USER, "other thread", true, &[]),
    )
    .await;

    rest.wait_post("505", |p| p == "reply 1: dm").await;
    rest.wait_post("504", |p| p == "reply 1: mentioned").await;
    rest.wait_post("600", |p| p == "reply 1: listed").await;
    rest.wait_post("601", |p| p == "reply 1: in thread").await;
    let posts = rest.wait_post("650", |p| p == "reply 1: own thread").await;
    // The tool line comes first, in a code block.
    assert_eq!(posts, ["```\n-> bash echo hi\n```", "reply 1: own thread"]);
    let log = running.stop().await;
    for channel in ["501", "502", "503", "651"] {
        assert!(
            rest.posts(channel).is_empty(),
            "{channel}: {:?}",
            rest.posts(channel)
        );
    }
    assert_eq!(rest.posts("504").len(), 2, "{:?}", rest.posts("504"));
    // Every post asks Discord not to ping anyone.
    for request in rest
        .requests()
        .iter()
        .filter(|r| r.path.ends_with("/messages"))
    {
        assert_eq!(request.body["allowed_mentions"], json!({"parse": []}));
    }
    let text = log.join("\n");
    assert_eq!(
        text.matches(&format!("user {STRANGER}")).count(),
        1,
        "{text}"
    );
}

#[tokio::test]
async fn long_replies_are_split_with_code_blocks_intact() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "long", false, &[]))
        .await;
    let posts = rest
        .wait_post("505", |p| p.ends_with("That was long."))
        .await;
    running.stop().await;
    assert!(posts.len() >= 3, "{posts:#?}");
    for post in &posts {
        assert!(post.chars().count() <= 2000);
        assert_eq!(post.matches("```").count() % 2, 0, "{post}");
    }
    assert!(posts[0].starts_with("Here it is:\n```rust\n"));
    assert!(posts[1].starts_with("```rust\nlet value_"));
}

#[tokio::test]
async fn queues_per_channel_runs_channels_side_by_side_and_stops() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[("JUNEES_DISCORD_PARALLEL", "2")]),
        fake(Duration::from_millis(1500)),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;

    // Three messages in one channel: answered one after the other, in order.
    for text in ["slow one", "two", "three"] {
        conn.dispatch("MESSAGE_CREATE", message("505", USER, text, false, &[]))
            .await;
    }
    // Another channel does not wait for the first.
    conn.dispatch("MESSAGE_CREATE", message("506", USER, "quick", false, &[]))
        .await;
    rest.wait_post("506", |p| p == "reply 1: quick").await;
    assert!(rest.posts("505").is_empty(), "{:?}", rest.posts("505"));
    let posts = rest.wait_post("505", |p| p == "reply 3: three").await;
    let replies: Vec<&String> = posts.iter().filter(|p| p.starts_with("reply")).collect();
    assert_eq!(
        replies,
        ["reply 1: slow one", "reply 2: two", "reply 3: three"]
    );

    // !stop cancels the running turn and drops what is queued.
    let typed = rest.typing("505");
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "wait", false, &[]))
        .await;
    rest.wait_typing("505", typed).await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("505", USER, "dropped", false, &[]),
    )
    .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("505", USER, "!status", false, &[]),
    )
    .await;
    let posts = rest
        .wait_post("505", |p| p.contains("a turn is running, 1 queued"))
        .await;
    assert!(
        posts.iter().any(|p| p.contains("fake backend")),
        "{posts:#?}"
    );
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "!stop", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "(stopping; dropped 1 queued)")
        .await;
    rest.wait_post("505", |p| p == "(stopped)").await;
    // The channel goes on afterwards.
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "after", false, &[]))
        .await;
    let posts = rest.wait_post("505", |p| p == "reply 4: after").await;
    assert!(
        !posts
            .iter()
            .any(|p| p.contains("dropped") && p.starts_with("reply"))
    );

    // !new starts over; !help and unknown commands answer at once.
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "!new", false, &[]))
        .await;
    rest.wait_post("505", |p| p.starts_with("(new conversation)"))
        .await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "again", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "reply 1: again").await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "!nope", false, &[]))
        .await;
    rest.wait_post("505", |p| p.contains("unknown command `!nope`"))
        .await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "!stop", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "(nothing is running here)")
        .await;
    running.stop().await;
}

#[tokio::test]
async fn a_turn_waiting_for_a_slot_can_be_stopped() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[("JUNEES_DISCORD_PARALLEL", "1")]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "wait", false, &[]))
        .await;
    rest.wait_typing("505", 0).await;
    conn.dispatch("MESSAGE_CREATE", message("506", USER, "next", false, &[]))
        .await;
    rest.wait_post("506", |p| {
        p == "(waiting for a free slot: at most 1 turns run at once)"
    })
    .await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("506", USER, "!status", false, &[]),
    )
    .await;
    rest.wait_post("506", |p| p.contains("a turn waits for a free slot"))
        .await;
    conn.dispatch("MESSAGE_CREATE", message("506", USER, "!stop", false, &[]))
        .await;
    rest.wait_post("506", |p| p == "(stopped before it started)")
        .await;
    // The slot frees once the first channel stops; the next turn runs.
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "!stop", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "(stopped)").await;
    conn.dispatch("MESSAGE_CREATE", message("506", USER, "later", false, &[]))
        .await;
    rest.wait_post("506", |p| p == "reply 1: later").await;
    running.stop().await;
    assert!(!rest.posts("506").iter().any(|p| p.contains(": next")));
}

#[tokio::test]
async fn a_shutdown_cancels_running_turns() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "wait", false, &[]))
        .await;
    rest.wait_typing("505", 0).await;
    running.stop().await;
    assert!(
        rest.posts("505")
            .contains(&"(stopped: Junees is shutting down)".to_string()),
        "{:?}",
        rest.posts("505")
    );
}

#[tokio::test]
async fn rate_limited_posts_are_retried() {
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        None,
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    *rest.state.limit_next.lock().unwrap() = 2;
    let started = tokio::time::Instant::now();
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "hello", false, &[]))
        .await;
    let posts = rest.wait_post("505", |p| p == "reply 1: hello").await;
    // retry_after is 0.2 s, twice.
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(posts, ["```\n-> bash echo hi\n```", "reply 1: hello"]);
    let limited = rest
        .requests()
        .iter()
        .filter(|r| r.headers.contains_key("x-fake-status"))
        .count();
    assert_eq!(limited, 2);
    let log = running.stop().await.join("\n");
    assert!(log.contains("rate limited, waiting 0.20s"), "{log}");
}

#[tokio::test]
async fn a_second_gateway_on_the_same_home_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let first = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        Some(home.path().into()),
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;

    let second = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        Some(home.path().into()),
        dir.path(),
    );
    let err = tokio::time::timeout(WAIT, second.task)
        .await
        .expect("the second gateway kept running")
        .unwrap()
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("another Junees gateway is running"), "{text}");
    assert!(text.contains("gateway/lock"), "{text}");
    // It stopped before asking Discord for anything.
    let asked = rest
        .requests()
        .iter()
        .filter(|r| r.path.ends_with("/gateway/bot"))
        .count();
    assert_eq!(asked, 1);

    // Once the first one has stopped, a gateway starts again.
    first.stop().await;
    let third = start(
        discord(&rest.url, &[]),
        fake(Duration::ZERO),
        Some(home.path().into()),
        dir.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    third.stop().await;
}

// -------------------------------------- With the real agent on a fake API

fn api_config(api: &str, home: &Path, approval: &str) -> Config {
    let map: HashMap<&str, String> = [
        ("ANTHROPIC_API_KEY", "test-key".to_string()),
        ("ANTHROPIC_BASE_URL", api.to_string()),
        ("JUNEES_HOME", home.display().to_string()),
        ("JUNEES_STREAM", "0".to_string()),
        ("JUNEES_APPROVAL", approval.to_string()),
    ]
    .into_iter()
    .collect();
    Config::from_lookup(|key| map.get(key).cloned()).unwrap()
}

/// The text of every user and assistant message of a model request.
fn texts(request: &str) -> Vec<String> {
    common::body_of(request)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| {
            let role = message["role"].as_str().unwrap().to_string();
            message["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(move |block| {
                    let text = block["text"]
                        .as_str()
                        .or(block["content"].as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| block["type"].to_string());
                    format!("{role}: {text}")
                })
        })
        .collect()
}

#[tokio::test]
async fn each_channel_continues_its_session_after_a_restart() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;

    let (api, served) = common::fake_api(vec![
        (200, common::text_reply("first answer")),
        (200, common::text_reply("other channel")),
    ])
    .await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        discord(&rest.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "hello", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "first answer").await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("506", USER, "elsewhere", false, &[]),
    )
    .await;
    rest.wait_post("506", |p| p == "other channel").await;
    running.stop().await;
    tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let index: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("gateway/discord-sessions.json")).unwrap(),
    )
    .unwrap();
    assert_ne!(index["505"], index["506"]);

    // A new gateway process: channel 505 goes on where it was.
    let (api, served) = common::fake_api(vec![(200, common::text_reply("second answer"))]).await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        discord(&rest.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "again", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "second answer").await;
    conn.dispatch(
        "MESSAGE_CREATE",
        message("505", USER, "!status", false, &[]),
    )
    .await;
    let posts = rest
        .wait_post("505", |p| p.contains("this channel: resumed"))
        .await;
    let status = posts.iter().find(|p| p.contains("this channel")).unwrap();
    assert!(status.contains(index["505"].as_str().unwrap()), "{status}");
    running.stop().await;
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert_eq!(
        texts(&requests[0]),
        ["user: hello", "assistant: first answer", "user: again"]
    );
    // The session is an ordinary journal of the gateway's folder.
    let id = index["505"].as_str().unwrap();
    let journal =
        std::fs::read_to_string(home.path().join(format!("sessions/{id}.jsonl"))).unwrap();
    assert!(journal.contains("second answer"));
}

#[tokio::test]
async fn a_session_in_use_elsewhere_stays_the_channels() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    let (api, served) = common::fake_api(vec![(200, common::text_reply("first answer"))]).await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        discord(&rest.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "hello", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "first answer").await;
    running.stop().await;
    tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let index_path = home.path().join("gateway/discord-sessions.json");
    let before = std::fs::read_to_string(&index_path).unwrap();
    let index: Value = serde_json::from_str(&before).unwrap();
    let id = index["505"].as_str().unwrap().to_string();

    // Another Junees (`junees --resume <id>`) has the session open.
    let held = Session::resume(&home.path().join(format!("sessions/{id}.jsonl"))).unwrap();
    let (api, served) = common::fake_api(vec![(200, common::text_reply("second answer"))]).await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        discord(&rest.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "again", false, &[]))
        .await;
    let posts = rest.wait_post("505", |p| p.starts_with("(not run)")).await;
    let refused = posts.iter().find(|p| p.starts_with("(not run)")).unwrap();
    assert!(refused.contains("in use by another Junees"), "{refused}");
    assert!(refused.contains(&id), "{refused}");
    assert_eq!(std::fs::read_to_string(&index_path).unwrap(), before);

    // Once the other Junees has let go, the next message continues it.
    drop(held);
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "again", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "second answer").await;
    running.stop().await;
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert_eq!(
        texts(&requests[0]),
        ["user: hello", "assistant: first answer", "user: again"]
    );
    assert_eq!(std::fs::read_to_string(&index_path).unwrap(), before);
}

fn tool_call(command: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "bash",
                     "input": {"command": command}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

#[tokio::test]
async fn risky_calls_wait_for_an_answer_in_the_channel() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let gateway = FakeGateway::bind().await;
    let rest = FakeRest::start(&gateway.url).await;
    // Absolute paths: bash runs in the test process's folder.
    let marker = cwd.path().join("marker");
    let command = format!(
        "rm -f {}/nothing-here; touch {}",
        cwd.path().display(),
        marker.display()
    );
    let command = command.as_str();
    let (api, served) = common::fake_api(vec![
        (200, tool_call(command)),
        (200, common::text_reply("declined, fine")),
        (200, tool_call(command)),
        (200, common::text_reply("ran it")),
        (200, tool_call(command)),
        (200, common::text_reply("timed out")),
    ])
    .await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "risky"),
        cwd.path().into(),
    ));
    let running = start(
        discord(&rest.url, &[("JUNEES_DISCORD_APPROVAL_TIMEOUT", "1s")]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let mut conn = gateway.accept().await;
    conn.start(&gateway.url).await;

    // A reason declines, and the model reads it.
    conn.dispatch(
        "MESSAGE_CREATE",
        message("505", USER, "clean up", false, &[]),
    )
    .await;
    let posts = rest
        .wait_post("505", |p| p.contains("Approve this tool call?"))
        .await;
    assert_eq!(posts.len(), 2, "{posts:#?}");
    // The tool line is shortened; the question shows the whole command.
    assert!(posts[0].starts_with("```\n-> bash rm -f "), "{}", posts[0]);
    assert!(
        posts[1].contains(&format!("approve bash: {command}")),
        "{}",
        posts[1]
    );
    conn.dispatch(
        "MESSAGE_CREATE",
        message("505", USER, "n not today", false, &[]),
    )
    .await;
    rest.wait_post("505", |p| p == "declined, fine").await;
    assert!(!cwd.path().join("marker").exists());

    // y runs it.
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "now", false, &[]))
        .await;
    rest.wait_posts("505", 2, |p| p.contains("Approve this tool call?"))
        .await;
    conn.dispatch("MESSAGE_CREATE", message("505", USER, "y", false, &[]))
        .await;
    rest.wait_post("505", |p| p == "ran it").await;
    assert!(cwd.path().join("marker").exists());
    std::fs::remove_file(cwd.path().join("marker")).unwrap();

    // No answer: declined after the timeout.
    conn.dispatch(
        "MESSAGE_CREATE",
        message("505", USER, "once more", false, &[]),
    )
    .await;
    rest.wait_post("505", |p| p == "(no answer within 1s: declined)")
        .await;
    rest.wait_post("505", |p| p == "timed out").await;
    assert!(!cwd.path().join("marker").exists());
    running.stop().await;

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let declined = texts(&requests[1]);
    assert_eq!(
        declined.last().unwrap(),
        "user: the user declined: not today"
    );
    let ran = texts(&requests[3]);
    assert!(!ran.last().unwrap().contains("declined"), "{ran:?}");
    let timed_out = texts(&requests[5]);
    assert_eq!(
        timed_out.last().unwrap(),
        "user: the user declined: no answer within 1s"
    );
}
