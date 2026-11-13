//! The Slack gateway against a fake Slack on loopback: a Web API
//! (`auth.test`, `apps.connections.open`, `chat.postMessage`,
//! `chat.update`, `chat.postEphemeral`, files) and a Socket Mode
//! WebSocket server that sends events and button presses and records the
//! acknowledgements. Nothing here reaches slack.com.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use junees::agent::Cancelled;
use junees::approval::Ask;
use junees::cancel::Cancel;
use junees::config::Config;
use junees::gateway::Log;
use junees::gateway::engine::{ApiEngines, Conversation, Engines, Progress};
use junees::session::{Journal, Transcript};
use junees::slack::config::SlackConfig;
use junees::slack::{CLOSE_IDLE, SLACK_NOTE, SlackGateway, Timing};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::{Notify, oneshot};
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(15);

const BOT_ID: &str = "UBOT";
const USER: &str = "U111";
const STRANGER: &str = "U222";
const DM: &str = "D1";
const CHANNEL: &str = "C1";

/// The tokens, built in pieces so no token-looking literal sits in the
/// source; their secret parts must never show.
fn bot_token() -> String {
    ["xoxb", "1-2-fake-slack-bot-must-not-leak"].join("-")
}

fn app_token() -> String {
    ["xapp", "1-A1-fake-slack-app-must-not-leak"].join("-")
}

const SECRET: &str = "must-not-leak";

// --------------------------------------------------------- Fake Slack

/// A Web API request the fake received.
#[derive(Debug, Clone)]
struct Received {
    path: String,
    auth: String,
    body: Value,
}

/// What the Socket Mode server does with the open connection.
enum Command {
    Send(String),
    /// Closes the TCP connection without a close frame.
    Drop,
    /// Keeps the connection open but stops reading and sending (no pongs
    /// either), like a connection that died somewhere on the way.
    Hold,
}

#[derive(Default)]
struct SlackState {
    requests: Mutex<Vec<Received>>,
    notify: Notify,
    /// Envelope ids acknowledged over the socket.
    acks: Mutex<Vec<String>>,
    /// WebSocket connections accepted so far.
    connections: AtomicUsize,
    /// While set, new connections get no `hello` and nothing else.
    mute: AtomicBool,
    /// Connections kept silent because of `mute`.
    muted: AtomicUsize,
    /// The newest connection's command channel.
    current: Mutex<Option<UnboundedSender<Command>>>,
    next: AtomicUsize,
    /// Answer `auth.test` with `invalid_auth`.
    unauthorized: AtomicBool,
    /// Answer this many `chat.postMessage` calls with 429 first.
    limit_next: Mutex<usize>,
    /// Refuse `chat.postMessage` calls with `blocks` (`invalid_blocks`).
    refuse_blocks: AtomicBool,
    /// The `ts` of the last message posted.
    last_ts: AtomicUsize,
    /// Path under `/files/` to bytes.
    files: Mutex<HashMap<String, Vec<u8>>>,
    ws_url: Mutex<String>,
}

struct FakeSlack {
    api: String,
    files: String,
    state: Arc<SlackState>,
}

impl FakeSlack {
    async fn start() -> Self {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", http.local_addr().unwrap());
        let state = Arc::new(SlackState::default());
        *state.ws_url.lock().unwrap() = format!("ws://{}/link", ws.local_addr().unwrap());
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((socket, _)) = http.accept().await {
                tokio::spawn(serve(socket, Arc::clone(&shared)));
            }
        });
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((socket, _)) = ws.accept().await {
                tokio::spawn(socket_mode(socket, Arc::clone(&shared)));
            }
        });
        Self {
            api: format!("{base}/api"),
            files: format!("{base}/files"),
            state,
        }
    }

    fn requests(&self) -> Vec<Received> {
        self.state.requests.lock().unwrap().clone()
    }

    fn calls(&self, method: &str) -> Vec<Received> {
        let path = format!("/api/{method}");
        self.requests()
            .into_iter()
            .filter(|r| r.path.split('#').next() == Some(path.as_str()))
            .collect()
    }

    /// The texts posted to `key` (`D1`, or `C1:<thread_ts>`).
    fn posts(&self, key: &str) -> Vec<String> {
        self.calls("chat.postMessage")
            .into_iter()
            .filter(|r| {
                post_key(&r.body) == key
                    && !r.path.contains("limited")
                    && !r.path.contains("refused")
            })
            .map(|r| r.body["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Sends a `block_actions` payload in an `interactive` envelope;
    /// returns the envelope id.
    fn press(&self, payload: Value) -> String {
        let n = self.state.next.fetch_add(1, Ordering::SeqCst);
        let envelope = format!("env-{n}");
        self.send(Command::Send(
            json!({"envelope_id": envelope, "type": "interactive",
                   "accepts_response_payload": true, "payload": payload})
            .to_string(),
        ));
        envelope
    }

    async fn until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let notified = self.state.notify.notified();
            if done() {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!(
                    "timed out waiting for {what}; requests: {:#?}",
                    self.requests()
                );
            }
        }
    }

    async fn wait_post(&self, key: &str, want: impl Fn(&str) -> bool) -> Vec<String> {
        self.until(&format!("a post to {key}"), || {
            self.posts(key).iter().any(|post| want(post))
        })
        .await;
        self.posts(key)
    }

    async fn wait_connections(&self, count: usize) {
        self.until(&format!("{count} connections"), || {
            self.state.connections.load(Ordering::SeqCst) >= count
                && self.state.current.lock().unwrap().is_some()
        })
        .await;
    }

    async fn wait_ack(&self, envelope: &str) {
        self.until(&format!("the ack of {envelope}"), || {
            self.state
                .acks
                .lock()
                .unwrap()
                .iter()
                .any(|a| a == envelope)
        })
        .await;
    }

    fn send(&self, command: Command) {
        let current = self.state.current.lock().unwrap();
        let sent = current
            .as_ref()
            .expect("no Socket Mode connection")
            .send(command);
        assert!(sent.is_ok(), "the connection is gone");
    }

    /// Sends `event` in an `events_api` envelope with its own event id;
    /// returns the envelope id.
    fn push(&self, event: Value) -> String {
        let n = self.state.next.fetch_add(1, Ordering::SeqCst);
        self.push_as(event, &format!("Ev{n}"), &format!("env-{n}"))
    }

    fn push_as(&self, event: Value, event_id: &str, envelope: &str) -> String {
        let payload = json!({
            "type": "event_callback", "team_id": "T1", "api_app_id": "A1",
            "event_id": event_id, "event_time": 1, "event": event,
        });
        self.send(Command::Send(
            json!({"envelope_id": envelope, "type": "events_api",
                   "accepts_response_payload": false, "payload": payload})
            .to_string(),
        ));
        envelope.to_string()
    }

    fn file(&self, path: &str, bytes: Vec<u8>) {
        self.state
            .files
            .lock()
            .unwrap()
            .insert(path.to_string(), bytes);
    }
}

/// The conversation key of a `chat.postMessage` body.
fn post_key(body: &Value) -> String {
    let channel = body["channel"].as_str().unwrap_or_default();
    match body["thread_ts"].as_str() {
        Some(thread) => format!("{channel}:{thread}"),
        None => channel.to_string(),
    }
}

fn answer(socket_status: u16, headers: &str, body: Value) -> (u16, String, Vec<u8>) {
    (
        socket_status,
        headers.to_string(),
        body.to_string().into_bytes(),
    )
}

async fn serve(mut socket: TcpStream, state: Arc<SlackState>) {
    let Some((mut path, auth, mut body)) = read_http(&mut socket).await else {
        return;
    };
    let bot = format!("Bearer {}", bot_token());
    let app = format!("Bearer {}", app_token());
    let (status, headers, bytes) = if let Some(file) = path.strip_prefix("/files/") {
        match state.files.lock().unwrap().get(file) {
            Some(bytes) if auth == bot => (200, String::new(), bytes.clone()),
            Some(_) => (200, String::new(), b"<html>sign in</html>".to_vec()),
            None => (404, String::new(), b"not found".to_vec()),
        }
    } else {
        match path.as_str() {
            "/api/auth.test" if state.unauthorized.load(Ordering::SeqCst) => {
                answer(200, "", json!({"ok": false, "error": "invalid_auth"}))
            }
            "/api/auth.test" if auth == bot => answer(
                200,
                "",
                json!({"ok": true, "url": "https://team.slack.com/", "team": "Team",
                       "user": "junees", "team_id": "T1", "user_id": BOT_ID,
                       "bot_id": "BBOT"}),
            ),
            "/api/apps.connections.open" if auth == app => {
                let n = state.connections.load(Ordering::SeqCst);
                let url = format!("{}?ticket={n}", state.ws_url.lock().unwrap());
                answer(200, "", json!({"ok": true, "url": url}))
            }
            "/api/chat.postMessage"
                if auth == bot
                    && body.get("blocks").is_some()
                    && state.refuse_blocks.load(Ordering::SeqCst) =>
            {
                path.push_str("#refused");
                answer(200, "", json!({"ok": false, "error": "invalid_blocks"}))
            }
            "/api/chat.update" | "/api/chat.postEphemeral" if auth == bot => {
                answer(200, "", json!({"ok": true, "channel": body["channel"]}))
            }
            "/api/chat.postMessage" if auth == bot => {
                let limited = {
                    let mut left = state.limit_next.lock().unwrap();
                    let limited = *left > 0;
                    *left = left.saturating_sub(1);
                    limited
                };
                if limited {
                    path.push_str("#limited");
                    answer(
                        429,
                        "retry-after: 1\r\n",
                        json!({"ok": false, "error": "ratelimited"}),
                    )
                } else {
                    // Each message its own ts, which the test can press.
                    let n = state.last_ts.fetch_add(1, Ordering::SeqCst) + 1;
                    let ts = format!("1800000000.{n:06}");
                    body["_sent_ts"] = json!(ts);
                    answer(
                        200,
                        "",
                        json!({"ok": true, "channel": body["channel"], "ts": ts}),
                    )
                }
            }
            _ => answer(200, "", json!({"ok": false, "error": "invalid_auth"})),
        }
    };
    state
        .requests
        .lock()
        .unwrap()
        .push(Received { path, auth, body });
    let head = format!(
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n{headers}content-length: \
         {}\r\nconnection: close\r\n\r\n",
        bytes.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(&bytes).await;
    let _ = socket.shutdown().await;
    state.notify.notify_waiters();
}

async fn read_http(socket: &mut TcpStream) -> Option<(String, String, Value)> {
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
    let path = head.split_whitespace().nth(1)?.to_string();
    let auth = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default();
    let body = &buf[head_end + 4..head_end + 4 + length];
    Some((
        path,
        auth,
        serde_json::from_slice(body).unwrap_or(Value::Null),
    ))
}

/// One Socket Mode connection: `hello`, then the test's commands, and
/// every acknowledgement recorded.
async fn socket_mode(socket: TcpStream, state: Arc<SlackState>) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
        return;
    };
    if state.mute.load(Ordering::SeqCst) {
        state.muted.fetch_add(1, Ordering::SeqCst);
        state.notify.notify_waiters();
        // Hold the socket without reading until the gateway gives up.
        tokio::time::sleep(WAIT).await;
        drop(ws);
        return;
    }
    let (commands, mut received) = mpsc::unbounded_channel();
    let hello = json!({"type": "hello", "num_connections": 1,
        "connection_info": {"app_id": "A1"},
        "debug_info": {"host": "fake", "approximate_connection_time": 3600}});
    if ws
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    *state.current.lock().unwrap() = Some(commands);
    state.connections.fetch_add(1, Ordering::SeqCst);
    state.notify.notify_waiters();
    loop {
        tokio::select! {
            command = received.recv() => match command {
                Some(Command::Send(text)) => {
                    if ws.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Some(Command::Drop) | None => return,
                Some(Command::Hold) => {
                    // Wait (not reading) until a newer connection replaces
                    // this one's command channel.
                    while let Some(command) = received.recv().await {
                        if matches!(command, Command::Drop) {
                            break;
                        }
                    }
                    drop(ws);
                    return;
                }
            },
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    let ack: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    if let Some(id) = ack["envelope_id"].as_str() {
                        state.acks.lock().unwrap().push(id.to_string());
                        state.notify.notify_waiters();
                    }
                }
                Some(Ok(_)) => {}
                _ => return,
            },
        }
    }
}

// ----------------------------------------------------------- Events

static TS: AtomicUsize = AtomicUsize::new(1);

fn next_ts() -> String {
    format!("1700000000.{:06}", TS.fetch_add(1, Ordering::SeqCst))
}

/// A message from `user` in the DM with the bot.
fn dm(user: &str, text: &str) -> Value {
    json!({"type": "message", "channel": DM, "channel_type": "im", "user": user,
           "text": text, "ts": next_ts(), "event_ts": "1"})
}

/// A message in the channel, in `thread` if given.
fn in_channel(user: &str, text: &str, thread: Option<&str>) -> Value {
    let mut event = json!({"type": "message", "channel": CHANNEL, "channel_type": "channel",
        "user": user, "text": text, "ts": next_ts(), "event_ts": "1"});
    if let Some(thread) = thread {
        event["thread_ts"] = json!(thread);
    }
    event
}

/// An `app_mention` of the bot in the channel.
fn mention(user: &str, text: &str, thread: Option<&str>) -> Value {
    let mut event = in_channel(user, &format!("<@{BOT_ID}> {text}"), thread);
    event["type"] = json!("app_mention");
    event.as_object_mut().unwrap().remove("channel_type");
    event
}

fn thread_key(ts: &Value) -> String {
    format!("{CHANNEL}:{}", ts.as_str().unwrap())
}

// ------------------------------------------------------------ Engines

/// Conversations that answer at once, with what they were asked.
struct FakeEngines;

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
        Ok(Box::new(FakeConversation { cancel, turns: 0 }))
    }
}

struct FakeConversation {
    cancel: Cancel,
    turns: usize,
}

#[async_trait(?Send)]
impl Conversation for FakeConversation {
    async fn turn(&mut self, input: &str, out: &UnboundedSender<Progress>) -> Result<()> {
        match input {
            "wait" => {
                out.send(Progress::Text("waiting".into())).unwrap();
                self.cancel.cancelled().await;
                return Err(Cancelled.into());
            }
            "long" => {
                let lines: Vec<String> = (0..900).map(|n| format!("줄 {n}: 값은 {n}")).collect();
                out.send(Progress::Text(lines.join("\n"))).unwrap();
                return Ok(());
            }
            "markdown" => {
                out.send(Progress::Text(
                    "**Done** <!channel> & see [docs](https://docs.example/a?b=1&c=2)\n\
                     ```rust\nlet x = a < b;\n```"
                        .into(),
                ))
                .unwrap();
                return Ok(());
            }
            _ => {}
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

// -------------------------------------------------------------- Setup

fn slack(fake: &FakeSlack, extra: &[(&str, &str)]) -> SlackConfig {
    let mut map: HashMap<String, String> = [
        ("JUNEES_SLACK_BOT_TOKEN", bot_token()),
        ("JUNEES_SLACK_APP_TOKEN", app_token()),
        ("JUNEES_SLACK_ALLOWED_USERS", USER.to_string()),
        ("JUNEES_SLACK_API_URL", fake.api.clone()),
        ("JUNEES_SLACK_FILES_URL", fake.files.clone()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    for (key, value) in extra {
        map.insert(key.to_string(), value.to_string());
    }
    SlackConfig::from_lookup(|key| map.get(key).cloned()).unwrap()
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

    /// Waits for the gateway to end by itself.
    async fn ended(self) -> (Result<()>, Vec<String>) {
        let result = tokio::time::timeout(WAIT, self.task)
            .await
            .expect("the gateway did not end")
            .unwrap();
        (result, self.log.lock().unwrap().clone())
    }
}

fn start(
    config: SlackConfig,
    engines: Arc<dyn Engines>,
    home: Option<PathBuf>,
    cwd: &Path,
) -> Running {
    start_voice(config, engines, home, cwd, None)
}

fn start_voice(
    config: SlackConfig,
    engines: Arc<dyn Engines>,
    home: Option<PathBuf>,
    cwd: &Path,
    transcriber: Option<Arc<junees::transcribe::Transcriber>>,
) -> Running {
    let timing = Timing {
        ping_every: Duration::from_secs(30),
        idle: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(5),
        backoff_start: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
    };
    start_timed(config, engines, home, cwd, timing, CLOSE_IDLE, transcriber)
}

fn start_timed(
    config: SlackConfig,
    engines: Arc<dyn Engines>,
    home: Option<PathBuf>,
    cwd: &Path,
    timing: Timing,
    close_idle: Duration,
    transcriber: Option<Arc<junees::transcribe::Transcriber>>,
) -> Running {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    let log: Log = Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_string()));
    let gateway = SlackGateway {
        config,
        engines,
        home,
        cwd: cwd.to_path_buf(),
        memory: None,
        timing,
        close_idle,
        log,
        transcriber,
    };
    let stop = Cancel::new();
    let cancel = stop.clone();
    // Its conversations run on one thread (a LocalSet), so the future is
    // not Send: give it a thread and a runtime of its own, as `main` does.
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

fn fake() -> Arc<dyn Engines> {
    Arc::new(FakeEngines)
}

// -------------------------------------------------------------- Tests

#[tokio::test]
async fn answers_allowed_users_in_dms_mentions_and_its_threads() {
    let slack_server = FakeSlack::start().await;
    let s = &slack_server;
    let dir = tempfile::tempdir().unwrap();
    let running = start(slack(s, &[]), fake(), None, dir.path());
    s.wait_connections(1).await;

    // Every envelope is acknowledged, a stranger's too; only an allowed
    // user is answered.
    let stranger = s.push(dm(STRANGER, "hello"));
    let envelope = s.push(dm(USER, "hello"));
    let posts = s.wait_post(DM, |p| p == "reply 1: hello").await;
    // The tool line comes as a code block before the reply.
    assert_eq!(posts, ["```\n-&gt; bash echo hi\n```", "reply 1: hello"]);
    s.wait_ack(&stranger).await;
    s.wait_ack(&envelope).await;

    // In a channel: not without a mention.
    s.push(in_channel(USER, "just chatting", None));
    let asked = mention(USER, "what is up?", None);
    let root = asked["ts"].clone();
    s.push(asked);
    let thread = thread_key(&root);
    s.wait_post(&thread, |p| p == "reply 1: what is up?").await;
    // A reply in the bot's thread needs no mention.
    s.push(in_channel(USER, "and now?", Some(root.as_str().unwrap())));
    s.wait_post(&thread, |p| p == "reply 2: and now?").await;
    // Another thread without a mention, an edit and the bot's own post
    // are left alone.
    let other = in_channel(USER, "elsewhere", None);
    let other_root = other["ts"].as_str().unwrap().to_string();
    s.push(in_channel(USER, "not for the bot", Some(&other_root)));
    let mut edit = in_channel(USER, "edited", Some(root.as_str().unwrap()));
    edit["subtype"] = json!("message_changed");
    s.push(edit);
    let mut own = in_channel(BOT_ID, "own post", Some(root.as_str().unwrap()));
    own["bot_id"] = json!("BBOT");
    s.push(own);
    // A mention inside someone else's thread starts the bot there.
    s.push(mention(USER, "join in", Some(&other_root)));
    let other_thread = format!("{CHANNEL}:{other_root}");
    s.wait_post(&other_thread, |p| p == "reply 1: join in")
        .await;
    s.push(in_channel(USER, "last", Some(root.as_str().unwrap())));
    let posts = s.wait_post(&thread, |p| p == "reply 3: last").await;
    assert!(
        !posts
            .iter()
            .any(|p| p.contains("edited") || p.contains("own post"))
    );
    assert_eq!(
        s.posts(&other_thread),
        ["```\n-&gt; bash echo hi\n```", "reply 1: join in"]
    );
    // Nothing went to the channel outside a thread.
    assert!(s.posts(CHANNEL).is_empty());

    // Model Markdown becomes mrkdwn, escaped: no mention reaches anyone.
    s.push(dm(USER, "markdown"));
    let posts = s.wait_post(DM, |p| p.starts_with("*Done*")).await;
    assert_eq!(
        posts.last().unwrap(),
        "*Done* &lt;!channel&gt; &amp; see <https://docs.example/a?b=1&amp;c=2|docs>\n\
         ```\nlet x = a &lt; b;\n```"
    );
    let posted = s.calls("chat.postMessage");
    let body = &posted.last().unwrap().body;
    assert_eq!(body["unfurl_links"], false);
    assert!(body.get("link_names").is_none());

    let log = running.stop().await;
    let ignored: Vec<&String> = log.iter().filter(|l| l.contains(STRANGER)).collect();
    assert_eq!(ignored.len(), 1, "{log:#?}");
    assert!(
        log.iter()
            .any(|l| l.contains("slack gateway: junees (UBOT) in Team"))
    );
}

#[tokio::test]
async fn retried_and_doubled_events_run_once() {
    let s = FakeSlack::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(slack(&s, &[]), fake(), None, dir.path());
    s.wait_connections(1).await;
    // Slack sends an event again (same event id, new envelope)...
    let event = dm(USER, "once");
    s.push_as(event.clone(), "EvSame", "env-a");
    s.push_as(event, "EvSame", "env-b");
    // ...and a mention in a channel the app reads comes as `message` and
    // as `app_mention`, with different event ids.
    let asked = mention(USER, "twice?", None);
    let mut as_message = asked.clone();
    as_message["type"] = json!("message");
    as_message["channel_type"] = json!("channel");
    s.push(as_message);
    s.push(asked.clone());
    s.wait_post(&thread_key(&asked["ts"]), |p| p == "reply 1: twice?")
        .await;
    s.push(dm(USER, "next"));
    let posts = s.wait_post(DM, |p| p == "reply 2: next").await;
    assert_eq!(posts.iter().filter(|p| p.starts_with("reply 1")).count(), 1);
    // Both envelopes were acknowledged.
    s.wait_ack("env-a").await;
    s.wait_ack("env-b").await;
    running.stop().await;
    assert_eq!(s.posts(&thread_key(&asked["ts"])).len(), 2);
}

#[tokio::test]
async fn reconnects_when_slack_asks_and_when_the_connection_breaks() {
    let s = FakeSlack::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(slack(&s, &[]), fake(), None, dir.path());
    s.wait_connections(1).await;
    s.send(Command::Send(
        json!({"type": "disconnect", "reason": "refresh_requested",
               "debug_info": {"host": "fake"}})
        .to_string(),
    ));
    s.wait_connections(2).await;
    s.push(dm(USER, "after refresh"));
    s.wait_post(DM, |p| p == "reply 1: after refresh").await;
    s.send(Command::Drop);
    s.wait_connections(3).await;
    s.push(dm(USER, "after a break"));
    s.wait_post(DM, |p| p == "reply 2: after a break").await;
    let log = running.stop().await;
    assert_eq!(s.calls("apps.connections.open").len(), 3);
    assert!(
        log.iter()
            .any(|l| l.contains("Slack asks to reconnect (refresh_requested)")),
        "{log:#?}"
    );
    // The app-level token went only to apps.connections.open.
    for request in s.requests() {
        let app = request.auth.contains("xapp");
        assert_eq!(
            app,
            request.path == "/api/apps.connections.open",
            "{request:?}"
        );
    }
}

#[tokio::test]
async fn a_silent_connection_is_dropped_although_pings_go_out() {
    let s = FakeSlack::start().await;
    let dir = tempfile::tempdir().unwrap();
    // Pings go out more often than the silence limits, as with the
    // defaults (30 s pings, 90 s idle): they must not keep it open.
    let timing = Timing {
        ping_every: Duration::from_millis(50),
        idle: Duration::from_millis(300),
        connect_timeout: Duration::from_millis(300),
        backoff_start: Duration::from_millis(20),
        backoff_max: Duration::from_millis(50),
    };
    s.state.mute.store(true, Ordering::SeqCst);
    let running = start_timed(
        slack(&s, &[]),
        fake(),
        None,
        dir.path(),
        timing,
        CLOSE_IDLE,
        None,
    );
    // No `hello`: given up after the connect timeout, then tried again.
    s.until("two silent connections", || {
        s.state.muted.load(Ordering::SeqCst) >= 2
    })
    .await;
    s.state.mute.store(false, Ordering::SeqCst);
    s.wait_connections(1).await;
    // After `hello` the connection goes quiet and stops reading.
    s.send(Command::Hold);
    s.wait_connections(2).await;
    s.push(dm(USER, "after the silence"));
    s.wait_post(DM, |p| p == "reply 1: after the silence").await;
    let log = running.stop().await;
    assert!(
        log.iter()
            .any(|l| l.contains("nothing from the connection for 0.3s")),
        "{log:#?}"
    );
}

#[tokio::test]
async fn long_replies_are_split_and_rate_limits_waited_out() {
    let s = FakeSlack::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(slack(&s, &[]), fake(), None, dir.path());
    s.wait_connections(1).await;
    *s.state.limit_next.lock().unwrap() = 1;
    s.push(dm(USER, "long"));
    s.wait_post(DM, |p| p.ends_with("줄 899: 값은 899")).await;
    let posts = s.posts(DM);
    assert!(posts.len() >= 3, "{}", posts.len());
    for post in &posts {
        assert!(post.encode_utf16().count() <= 4000, "{}", post.len());
    }
    let lines: Vec<String> = (0..900).map(|n| format!("줄 {n}: 값은 {n}")).collect();
    assert_eq!(posts.join("\n"), lines.join("\n"));
    // The first post was refused once (429) and sent again.
    let limited = s
        .calls("chat.postMessage")
        .into_iter()
        .filter(|r| r.path.contains("limited"))
        .count();
    assert_eq!(limited, 1);
    let log = running.stop().await;
    assert!(
        log.iter().any(|l| l.contains("rate limited, waiting 1s")),
        "{log:#?}"
    );
}

#[tokio::test]
async fn stop_status_and_new() {
    let s = FakeSlack::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(slack(&s, &[]), fake(), None, dir.path());
    s.wait_connections(1).await;
    s.push(dm(USER, "!stop"));
    s.wait_post(DM, |p| p == "(nothing is running here)").await;
    s.push(dm(USER, "wait"));
    s.wait_post(DM, |p| p == "waiting").await;
    s.push(dm(USER, "!status"));
    let posts = s
        .wait_post(DM, |p| p.contains("this chat: session not saved"))
        .await;
    let status = posts.iter().find(|p| p.contains("this chat:")).unwrap();
    assert!(status.contains("a turn is running, 0 queued"), "{status}");
    s.push(dm(USER, "!stop"));
    s.wait_post(DM, |p| p == "(stopped)").await;
    s.push(dm(USER, "!new"));
    s.wait_post(DM, |p| p == "(new conversation) session not saved")
        .await;
    // In a channel the command follows the mention.
    let asked = mention(USER, "!help", None);
    s.push(asked.clone());
    s.wait_post(&thread_key(&asked["ts"]), |p| p.starts_with("`!new` start"))
        .await;
    s.push(dm(USER, "!nope"));
    s.wait_post(DM, |p| p.starts_with("unknown command `!nope`"))
        .await;
    running.stop().await;
}

#[tokio::test]
async fn a_refused_token_stops_without_showing_it() {
    let s = FakeSlack::start().await;
    s.state.unauthorized.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let running = start(slack(&s, &[]), fake(), None, dir.path());
    let (result, log) = running.ended().await;
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("invalid_auth"), "{err}");
    assert!(err.contains("slack_bot_token"), "{err}");
    assert!(!err.contains(SECRET), "{err}");
    assert!(!log.iter().any(|l| l.contains(SECRET)), "{log:#?}");

    // A wrong app-level token: apps.connections.open is refused.
    let s = FakeSlack::start().await;
    let mut config = slack(&s, &[]);
    let wrong = ["xapp", "1-A1-other-fake-slack-app-must-not-leak"].join("-");
    config.app_token =
        junees::slack::config::Token::new(&wrong, junees::slack::config::Kind::App).unwrap();
    let running = start(config, fake(), None, dir.path());
    let (result, _) = running.ended().await;
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("apps.connections.open: invalid_auth"), "{err}");
    assert!(err.contains("connections:write"), "{err}");
    assert!(!err.contains(SECRET), "{err}");

    // Nobody listening: the error names no URL and no token.
    let mut config = slack(&s, &[]);
    config.api_url = "http://127.0.0.1:9/api".into();
    let (result, _) = start(config, fake(), None, dir.path()).ended().await;
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("cannot connect"), "{err}");
    assert!(!err.contains(SECRET) && !err.contains("127.0.0.1"), "{err}");
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

fn write_call(path: &Path) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "write_file",
                     "input": {"path": path.display().to_string(), "content": "written\n"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

#[tokio::test]
async fn risky_calls_wait_for_an_answer_in_the_dm_and_the_thread() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let s = FakeSlack::start().await;
    // No shell runs: `all` asks before every file write too.
    let file = cwd.path().join("note.txt");
    let (api, served) = common::fake_api(vec![
        (200, write_call(&file)),
        (200, common::text_reply("declined, fine")),
        (200, write_call(&file)),
        (200, common::text_reply("wrote it")),
        (200, write_call(&file)),
        (200, common::text_reply("timed out")),
    ])
    .await;
    let engines = Arc::new(
        ApiEngines::new(api_config(&api, home.path(), "all"), cwd.path().into())
            .with_note(SLACK_NOTE),
    );
    let running = start(
        slack(&s, &[("JUNEES_SLACK_APPROVAL_TIMEOUT", "1s")]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    s.wait_connections(1).await;
    s.push(dm(USER, "write a note"));
    let posts = s
        .wait_post(DM, |p| p.starts_with("*Approve this tool call?*"))
        .await;
    let question = posts.last().unwrap();
    assert!(question.contains("approve write_file:"), "{question}");
    s.push(dm(USER, "n not today"));
    s.wait_post(DM, |p| p == "declined, fine").await;
    assert!(!file.exists());

    // In a thread the answer needs no mention.
    let asked = mention(USER, "now in the channel", None);
    let thread = thread_key(&asked["ts"]);
    s.push(asked.clone());
    s.wait_post(&thread, |p| p.starts_with("*Approve this tool call?*"))
        .await;
    s.push(in_channel(USER, "y", Some(asked["ts"].as_str().unwrap())));
    s.wait_post(&thread, |p| p == "wrote it").await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "written\n");
    std::fs::remove_file(&file).unwrap();

    s.push(dm(USER, "once more"));
    s.wait_post(DM, |p| p == "(no answer within 1s: declined)")
        .await;
    s.wait_post(DM, |p| p == "timed out").await;
    assert!(!file.exists());
    running.stop().await;

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let system = common::body_of(&requests[0])["system"].to_string();
    assert!(system.contains("through Slack"), "{system}");
    assert_eq!(
        texts(&requests[1]).last().unwrap(),
        "user: the user declined: not today"
    );
    assert_eq!(texts(&requests[2]), ["user: now in the channel"]);
    assert_eq!(
        texts(&requests[5]).last().unwrap(),
        "user: the user declined: no answer within 1s"
    );
    // Each conversation's session is an ordinary journal, marked as
    // Slack's.
    let index: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("gateway/slack-sessions.json")).unwrap(),
    )
    .unwrap();
    for key in [DM.to_string(), thread] {
        let id = index[&key].as_str().unwrap();
        let journal =
            std::fs::read_to_string(home.path().join(format!("sessions/{id}.jsonl"))).unwrap();
        let meta: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(
            (&meta["source"], &meta["channel"]),
            (&json!("slack"), &json!(key))
        );
    }
    assert!(home.path().join("gateway/slack.lock").exists());
}

/// The `chat.postMessage` bodies to `key` that carried blocks.
fn buttoned(s: &FakeSlack, key: &str) -> Vec<Value> {
    s.calls("chat.postMessage")
        .into_iter()
        .filter(|r| post_key(&r.body) == key && r.body.get("blocks").is_some())
        .map(|r| r.body)
        .collect()
}

/// The texts `chat.postEphemeral` showed `user`.
fn ephemerals(s: &FakeSlack, user: &str) -> Vec<String> {
    s.calls("chat.postEphemeral")
        .into_iter()
        .filter(|r| r.body["user"] == user)
        .map(|r| r.body["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The `chat.update` bodies so far.
fn updates(s: &FakeSlack) -> Vec<Value> {
    s.calls("chat.update").into_iter().map(|r| r.body).collect()
}

/// A `block_actions` payload: `user` pressed the button with `value` on
/// the message `post` (a `chat.postMessage` body) made.
fn press_on(user: &str, post: &Value, value: &str) -> Value {
    let channel = post["channel"].as_str().unwrap();
    let ts = post["_sent_ts"].as_str().unwrap();
    let mut message = json!({"type": "message", "bot_id": "BBOT", "user": BOT_ID, "ts": ts,
                             "text": post["text"]});
    let mut container = json!({"type": "message", "message_ts": ts, "channel_id": channel,
                               "is_ephemeral": false});
    if let Some(thread) = post["thread_ts"].as_str() {
        message["thread_ts"] = json!(thread);
        container["thread_ts"] = json!(thread);
    }
    // The name holds what mrkdwn must escape.
    let name = if user == USER { "sam<&>" } else { "eve" };
    json!({
        "type": "block_actions", "team": {"id": "T1", "domain": "team"},
        "user": {"id": user, "username": name, "name": name, "team_id": "T1"},
        "api_app_id": "A1", "container": container, "trigger_id": "1.2.3",
        "channel": {"id": channel, "name": "somewhere"}, "message": message,
        "response_url": "https://hooks.slack.com/actions/T1/1/fake",
        "actions": [{"type": "button", "action_id": value, "block_id": "b1", "value": value,
                     "text": {"type": "plain_text", "text": "Allow"},
                     "action_ts": "1700000001.000001"}],
    })
}

/// The button values of a question post: Allow, (Always allow,) Decline.
fn button_values(post: &Value) -> Vec<String> {
    post["blocks"][1]["elements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|button| button["value"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn buttons_answer_questions_of_allowed_users_only() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let s = FakeSlack::start().await;
    let file = cwd.path().join("note.txt");
    let (api, served) = common::fake_api(vec![
        (200, write_call(&file)),
        (200, common::text_reply("declined, fine")),
        (200, write_call(&file)),
        (200, common::text_reply("wrote it")),
        (200, write_call(&file)),
        (200, common::text_reply("timed out")),
        (200, write_call(&file)),
        (200, common::text_reply("wrote again")),
        (200, write_call(&file)),
        (200, common::text_reply("plain")),
    ])
    .await;
    let engines = Arc::new(
        ApiEngines::new(api_config(&api, home.path(), "all"), cwd.path().into())
            .with_note(SLACK_NOTE),
    );
    let running = start(
        slack(&s, &[("JUNEES_SLACK_APPROVAL_TIMEOUT", "5s")]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    s.wait_connections(1).await;

    // The question: a section and the buttons.
    s.push(dm(USER, "write a note"));
    s.until("a question with buttons", || !buttoned(&s, DM).is_empty())
        .await;
    let question = buttoned(&s, DM).remove(0);
    let blocks = question["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 2, "{question}");
    assert_eq!(blocks[0]["type"], "section");
    assert_eq!(blocks[0]["text"]["type"], "mrkdwn");
    assert_eq!(blocks[0]["text"]["verbatim"], true);
    let section = blocks[0]["text"]["text"].as_str().unwrap();
    assert!(
        section.starts_with("*Approve this tool call?*"),
        "{section}"
    );
    assert!(
        section.contains("Press a button, or reply `y`"),
        "{section}"
    );
    assert_eq!(question["text"].as_str(), Some(section));
    assert_eq!(blocks[1]["type"], "actions");
    let buttons = blocks[1]["elements"].as_array().unwrap();
    let labels: Vec<&str> = buttons
        .iter()
        .map(|b| b["text"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["Allow", "Always allow", "Decline"]);
    for button in buttons {
        assert_eq!(button["type"], "button");
        assert_eq!(button["action_id"], button["value"]);
    }
    assert_eq!(buttons[0]["style"], "primary");
    assert_eq!(buttons[2]["style"], "danger");
    let values = button_values(&question);
    let id = values[0]
        .strip_prefix("junees:")
        .and_then(|rest| rest.strip_suffix(":y"))
        .unwrap()
        .to_string();
    assert_eq!(id.len(), 32, "{id}");
    assert_eq!(values[2], format!("junees:{id}:n"));

    // Presses that answer nothing: a stranger's (twice, logged once), a
    // guessed id, another message, a button that is not Junees's.
    let mut envelopes = vec![
        s.press(press_on(STRANGER, &question, &values[0])),
        s.press(press_on(STRANGER, &question, &values[0])),
        s.press(press_on(
            USER,
            &question,
            &format!("junees:{}:y", "0123456789abcdef".repeat(2)),
        )),
    ];
    let mut elsewhere = question.clone();
    elsewhere["_sent_ts"] = json!("1800000000.999999");
    envelopes.push(s.press(press_on(USER, &elsewhere, &values[0])));
    envelopes.push(s.press(press_on(USER, &question, "approve")));
    for envelope in &envelopes {
        s.wait_ack(envelope).await;
    }
    s.until("five notes", || {
        ephemerals(&s, STRANGER).len() + ephemerals(&s, USER).len() == 5
    })
    .await;
    assert_eq!(
        ephemerals(&s, STRANGER),
        ["Only the users in slack_allowed_users can answer Junees."; 2]
    );
    let already =
        "This question was already answered, timed out or stopped; the button does nothing now.";
    assert_eq!(
        ephemerals(&s, USER),
        [already, already, "Junees does not know this button."]
    );
    assert!(updates(&s).is_empty());
    assert!(!file.exists());

    // Decline: the buttons go, the result shows, escaped.
    s.wait_ack(&s.press(press_on(USER, &question, &values[2])))
        .await;
    s.wait_post(DM, |p| p == "declined, fine").await;
    assert!(!file.exists());
    let update = updates(&s).remove(0);
    assert_eq!(update["channel"], DM);
    assert_eq!(update["ts"], question["_sent_ts"]);
    assert_eq!(update["parse"], "none");
    let closed = update["blocks"].as_array().unwrap();
    assert_eq!(closed.len(), 2, "{update}");
    assert_eq!(closed[0], blocks[0]);
    assert_eq!(closed[1]["text"]["text"], "*Declined by sam&lt;&amp;&gt;.*");
    assert!(!update.to_string().contains("\"actions\""), "{update}");
    // The old question's buttons do nothing now.
    s.wait_ack(&s.press(press_on(USER, &question, &values[0])))
        .await;
    s.until("a note", || ephemerals(&s, USER).len() == 4).await;
    assert_eq!(ephemerals(&s, USER)[3], already);

    // In a thread: a press from another conversation does nothing; Allow
    // runs the call; the presser's `y` right after is a late answer.
    let asked = mention(USER, "now in the channel", None);
    let root = asked["ts"].as_str().unwrap().to_string();
    let thread = thread_key(&asked["ts"]);
    s.push(asked);
    s.until("a question in the thread", || {
        !buttoned(&s, &thread).is_empty()
    })
    .await;
    let question = buttoned(&s, &thread).remove(0);
    assert_eq!(question["thread_ts"], root.as_str());
    let values = button_values(&question);
    let mut in_dm = press_on(USER, &question, &values[0]);
    in_dm["channel"]["id"] = json!(DM);
    in_dm["container"]["channel_id"] = json!(DM);
    in_dm["message"]
        .as_object_mut()
        .unwrap()
        .remove("thread_ts");
    in_dm["container"]
        .as_object_mut()
        .unwrap()
        .remove("thread_ts");
    s.wait_ack(&s.press(in_dm)).await;
    s.until("a note", || ephemerals(&s, USER).len() == 5).await;
    s.wait_ack(&s.press(press_on(USER, &question, &values[0])))
        .await;
    s.wait_post(&thread, |p| p == "wrote it").await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "written\n");
    std::fs::remove_file(&file).unwrap();
    s.until("the thread's question closed", || updates(&s).len() == 2)
        .await;
    let update = updates(&s).remove(1);
    assert_eq!(update["channel"], CHANNEL);
    assert_eq!(update["ts"], question["_sent_ts"]);
    let line = update["blocks"][1]["text"]["text"].as_str().unwrap();
    assert!(line.starts_with("*Allowed"), "{line}");
    s.push(in_channel(USER, "y", Some(&root)));
    s.wait_post(&thread, |p| p.contains("already answered"))
        .await;

    // No answer: declined, and a press afterwards does nothing.
    s.push(dm(USER, "once more"));
    s.wait_post(DM, |p| p == "(no answer within 5s: declined)")
        .await;
    s.wait_post(DM, |p| p == "timed out").await;
    s.until("the timed out question closed", || updates(&s).len() == 3)
        .await;
    assert_eq!(
        updates(&s)[2]["blocks"][1]["text"]["text"],
        "*No answer within 5s: declined.*"
    );
    let question = buttoned(&s, DM).remove(1);
    s.wait_ack(&s.press(press_on(USER, &question, &button_values(&question)[0])))
        .await;
    s.until("a note", || ephemerals(&s, USER).len() == 6).await;
    assert_eq!(ephemerals(&s, USER)[5], already);
    assert!(!file.exists());

    // A text answer still works, and takes the buttons off too.
    s.push(dm(USER, "again"));
    s.until("a third question", || buttoned(&s, DM).len() == 3)
        .await;
    s.push(dm(USER, "y"));
    s.wait_post(DM, |p| p == "wrote again").await;
    s.until("the third question closed", || updates(&s).len() == 4)
        .await;
    assert_eq!(updates(&s)[3]["blocks"][1]["text"]["text"], "*Allowed.*");
    std::fs::remove_file(&file).unwrap();

    // Buttons refused: the question comes as text, answered in text.
    s.state.refuse_blocks.store(true, Ordering::SeqCst);
    s.push(dm(USER, "plain please"));
    let posts = s.wait_post(DM, |p| p.contains("Reply `y` to run it")).await;
    assert!(!posts.last().unwrap().contains("Press a button"));
    s.push(dm(USER, "n"));
    s.wait_post(DM, |p| p == "plain").await;
    let log = running.stop().await;
    assert_eq!(updates(&s).len(), 4);
    assert!(!file.exists());
    let strangers = log
        .iter()
        .filter(|l| l.contains("ignoring button presses of user U222"))
        .count();
    assert_eq!(strangers, 1, "{log:#?}");
    assert!(
        log.iter()
            .any(|l| l.contains("cannot post the question with buttons")
                && l.contains("invalid_blocks")
                && l.contains("posted it without buttons")),
        "{log:#?}"
    );
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert!(
        texts(&requests[1]).last().unwrap().contains("declined"),
        "{:?}",
        texts(&requests[1])
    );
}

/// A PNG header of `width` x `height`: enough for Junees, which never
/// decodes the pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xaeB`\x82");
    bytes
}

/// A DM from `from` with `files` (and `text`).
fn with_files(from: &str, text: &str, files: Vec<Value>) -> Value {
    let mut event = dm(from, text);
    event["subtype"] = json!("file_share");
    event["files"] = json!(files);
    event
}

fn file(s: &FakeSlack, name: &str, mimetype: &str) -> Value {
    json!({"id": "F1", "name": name, "mimetype": mimetype, "filetype": "png", "size": 70,
           "mode": "hosted", "is_external": false,
           "url_private": format!("{}/T1-F1/{name}", s.files),
           "url_private_download": format!("{}/T1-F1/download/{name}", s.files)})
}

#[tokio::test]
async fn idle_threads_close_and_open_their_session_again() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let s = FakeSlack::start().await;
    let (api, served) = common::fake_api(vec![
        (200, common::text_reply("first answer")),
        (200, common::text_reply("second answer")),
        (200, common::text_reply("dm answer")),
    ])
    .await;
    let engines = Arc::new(
        ApiEngines::new(api_config(&api, home.path(), "off"), cwd.path().into())
            .with_note(SLACK_NOTE),
    );
    let timing = Timing {
        ping_every: Duration::from_secs(30),
        idle: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(5),
        backoff_start: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
    };
    let running = start_timed(
        slack(&s, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
        timing,
        Duration::from_millis(200),
        None,
    );
    s.wait_connections(1).await;
    let asked = mention(USER, "remember the word kiwi", None);
    let thread = thread_key(&asked["ts"]);
    s.push(asked.clone());
    s.wait_post(&thread, |p| p == "first answer").await;
    let closed = |log: &[String]| {
        log.iter()
            .any(|l| l.contains("slack: closed 1 idle conversation(s), 0 open"))
    };
    let deadline = tokio::time::Instant::now() + WAIT;
    while !closed(&running.log.lock().unwrap()) {
        assert!(tokio::time::Instant::now() < deadline, "no worker closed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The thread's reply (no mention) reaches a new worker, which resumes
    // the saved session: the model sees the first exchange.
    s.push(in_channel(
        USER,
        "which word?",
        Some(asked["ts"].as_str().unwrap()),
    ));
    s.wait_post(&thread, |p| p == "second answer").await;
    s.push(dm(USER, "hello"));
    s.wait_post(DM, |p| p == "dm answer").await;
    running.stop().await;
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let second = texts(&requests[1]);
    assert!(
        second.iter().any(|t| t.contains("remember the word kiwi"))
            && second.iter().any(|t| t == "assistant: first answer"),
        "{second:#?}"
    );
    // Each conversation has its own history.
    assert!(
        !texts(&requests[2]).iter().any(|t| t.contains("kiwi")),
        "{:#?}",
        texts(&requests[2])
    );
}

#[tokio::test]
async fn images_reach_the_model_from_the_files_host_only() {
    use base64::Engine as _;
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let s = FakeSlack::start().await;
    let picture = png(20, 10);
    s.file("T1-F1/download/shot.png", picture.clone());
    let (api, served) = common::fake_api(vec![(200, common::text_reply("A small chart."))]).await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        slack(&s, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    s.wait_connections(1).await;
    // A stranger's file is never downloaded.
    s.push(with_files(
        STRANGER,
        "look",
        vec![file(&s, "shot.png", "image/png")],
    ));
    // A remote file points anywhere: no token goes there.
    let mut remote = file(&s, "remote.png", "image/png");
    remote["mode"] = json!("external");
    remote["is_external"] = json!(true);
    remote["url_private"] = json!("https://evil.example/remote.png");
    s.push(with_files(
        USER,
        "what is this? &lt;b&gt;",
        vec![
            file(&s, "shot.png", "image/png"),
            remote,
            file(&s, "notes.txt", "text/plain"),
        ],
    ));
    let posts = s.wait_post(DM, |p| p == "A small chart.").await;
    let notes = posts
        .iter()
        .find(|p| p.starts_with("(attachments left out:"))
        .unwrap();
    assert!(
        notes.contains("stored outside Slack") && notes.contains("notes.txt"),
        "{notes}"
    );
    running.stop().await;

    let downloads: Vec<Received> = s
        .requests()
        .into_iter()
        .filter(|r| r.path.starts_with("/files/"))
        .collect();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert_eq!(downloads[0].path, "/files/T1-F1/download/shot.png");
    assert_eq!(downloads[0].auth, format!("Bearer {}", bot_token()));
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let content = &common::body_of(&requests[0])["messages"][0]["content"];
    assert_eq!(content[0]["type"], "image", "{content}");
    assert_eq!(
        content[0]["source"]["data"],
        base64::engine::general_purpose::STANDARD.encode(&picture)
    );
    let text = content[1]["text"].as_str().unwrap();
    assert!(text.starts_with("what is this? <b>"), "{text}");
}

// ------------------------------------------------------------- Binary

#[tokio::test]
async fn the_binary_answers_and_stops_on_sigterm() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let s = FakeSlack::start().await;
    let (api, served) = common::fake_api(vec![(200, common::text_reply("binary answer"))]).await;

    // Without allowed users it refuses to start.
    let refused = common::junees_binary(&cwd, home.path())
        .args(["gateway", "slack"])
        .env("JUNEES_HOME", home.path().join(".junees"))
        .env("JUNEES_SLACK_BOT_TOKEN", bot_token())
        .env("JUNEES_SLACK_APP_TOKEN", app_token())
        .env("JUNEES_SLACK_API_URL", &s.api)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("no allowed Slack users"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");

    let child = common::junees_binary(&cwd, home.path())
        .args(["gateway", "slack"])
        .env("JUNEES_HOME", home.path().join(".junees"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", &api)
        .env("JUNEES_STREAM", "0")
        .env("JUNEES_SLACK_BOT_TOKEN", bot_token())
        .env("JUNEES_SLACK_APP_TOKEN", app_token())
        .env("JUNEES_SLACK_ALLOWED_USERS", USER)
        .env("JUNEES_SLACK_API_URL", &s.api)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    s.wait_connections(1).await;
    let envelope = s.push(dm(USER, "hello from the binary"));
    s.wait_post(DM, |p| p == "binary answer").await;
    s.wait_ack(&envelope).await;
    let pid = i32::try_from(child.id().unwrap()).unwrap();
    // SAFETY: kill only sends a signal to the child started above.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("the gateway did not stop on SIGTERM")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("slack gateway: junees (UBOT) in Team"),
        "{stderr}"
    );
    assert!(
        stderr.contains("slack: connected (Socket Mode)"),
        "{stderr}"
    );
    assert!(stderr.contains("slack gateway stopped"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert_eq!(texts(&requests[0]), ["user: hello from the binary"]);
}
