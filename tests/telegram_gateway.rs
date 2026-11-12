//! The Telegram gateway against a fake Bot API on loopback: long polling
//! and offsets, who is answered where, commands, long replies, flood
//! control, approval questions, images, sessions, a refused token, and
//! the binary.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use junees::agent::Cancelled;
use junees::approval::Ask;
use junees::cancel::Cancel;
use junees::config::Config;
use junees::gateway::Log;
use junees::gateway::engine::{ApiEngines, Conversation, Engines, Progress};
use junees::session::{Journal, Transcript};
use junees::telegram::config::TelegramConfig;
use junees::telegram::{TELEGRAM_NOTE, TelegramGateway, Timing};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, oneshot};

const WAIT: Duration = Duration::from_secs(15);
/// A token that is easy to look for in logs and errors.
const TOKEN: &str = "424242:fake-telegram-token-must-not-leak";
const SECRET: &str = "fake-telegram-token-must-not-leak";
const ME: i64 = 900;
const BOT_NAME: &str = "junees_bot";
const USER: i64 = 111;
const STRANGER: i64 = 222;
const GROUP: i64 = -100_700;

// ------------------------------------------------------------ Bot API

/// A request the fake Bot API received.
#[derive(Debug, Clone)]
struct Received {
    /// The method (`sendMessage`), or `file` for a download.
    method: String,
    path: String,
    body: Value,
}

#[derive(Default)]
struct BotState {
    requests: Mutex<Vec<Received>>,
    /// Updates not confirmed yet.
    updates: Mutex<Vec<Value>>,
    next_id: Mutex<i64>,
    /// Sent along with the next getUpdates answer, whatever the offset:
    /// a server that sends an update again.
    resend: Mutex<Vec<Value>>,
    notify: Notify,
    /// Answer getMe with 401 (and the token in the description).
    unauthorized: AtomicBool,
    /// Answer this many sendMessage calls with 429 first.
    limit_next: Mutex<usize>,
    /// Answer this many getUpdates calls with 409 first.
    conflict_next: Mutex<usize>,
    /// file_id to (file_path, bytes).
    files: Mutex<HashMap<String, (String, Vec<u8>)>>,
}

struct FakeBot {
    url: String,
    state: Arc<BotState>,
}

impl FakeBot {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/tg", listener.local_addr().unwrap());
        let state = Arc::new(BotState::default());
        *state.next_id.lock().unwrap() = 500;
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(socket, Arc::clone(&shared)));
            }
        });
        Self { url, state }
    }

    /// Queues an update holding `message`; returns its id.
    fn push(&self, message: Value) -> i64 {
        let id = {
            let mut next = self.state.next_id.lock().unwrap();
            *next += 1;
            *next
        };
        self.state
            .updates
            .lock()
            .unwrap()
            .push(json!({"update_id": id, "message": message}));
        self.state.notify.notify_waiters();
        id
    }

    fn file(&self, id: &str, path: &str, bytes: Vec<u8>) {
        self.state
            .files
            .lock()
            .unwrap()
            .insert(id.to_string(), (path.to_string(), bytes));
    }

    fn requests(&self) -> Vec<Received> {
        self.state.requests.lock().unwrap().clone()
    }

    fn calls(&self, method: &str) -> Vec<Received> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method)
            .collect()
    }

    /// The texts posted to `chat` (`<chat>` or `<chat>:<topic>`).
    fn posts(&self, chat: &str) -> Vec<String> {
        self.calls("sendMessage")
            .into_iter()
            .filter(|r| key(&r.body) == chat && !r.path.contains("limited"))
            .map(|r| r.body["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    async fn wait_post(&self, chat: &str, want: impl Fn(&str) -> bool) -> Vec<String> {
        self.wait_posts(chat, 1, want).await
    }

    async fn wait_posts(
        &self,
        chat: &str,
        count: usize,
        want: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let notified = self.state.notify.notified();
            let posts = self.posts(chat);
            if posts.iter().filter(|post| want(post)).count() >= count {
                return posts;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!("no such post to chat {chat}; posts: {posts:#?}");
            }
        }
    }

    fn typing(&self, chat: &str) -> usize {
        self.calls("sendChatAction")
            .iter()
            .filter(|r| key(&r.body) == chat)
            .count()
    }

    async fn wait_typing(&self, chat: &str, before: usize) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let notified = self.state.notify.notified();
            if self.typing(chat) > before {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!("no turn started in chat {chat}");
            }
        }
    }

    /// Waits until a getUpdates asked from `offset` on.
    async fn wait_offset(&self, offset: i64) {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let notified = self.state.notify.notified();
            if self
                .calls("getUpdates")
                .iter()
                .any(|r| r.body["offset"] == offset)
            {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                panic!("no getUpdates from offset {offset}");
            }
        }
    }
}

/// The chat key of a sendMessage or sendChatAction body.
fn key(body: &Value) -> String {
    match body["message_thread_id"].as_i64() {
        Some(thread) => format!("{}:{thread}", body["chat_id"]),
        None => body["chat_id"].to_string(),
    }
}

fn ok(result: Value) -> (u16, Vec<u8>) {
    (
        200,
        json!({"ok": true, "result": result})
            .to_string()
            .into_bytes(),
    )
}

fn refused(code: u16, description: &str, parameters: Option<Value>) -> (u16, Vec<u8>) {
    let mut body = json!({"ok": false, "error_code": code, "description": description});
    if let Some(parameters) = parameters {
        body["parameters"] = parameters;
    }
    (code, body.to_string().into_bytes())
}

async fn serve(mut socket: TcpStream, state: Arc<BotState>) {
    let Some((path, body)) = read_http(&mut socket).await else {
        return;
    };
    let file_prefix = format!("/tg/file/bot{TOKEN}/");
    let method_prefix = format!("/tg/bot{TOKEN}/");
    let (method, (status, answer)) = if let Some(file) = path.strip_prefix(&file_prefix) {
        let found = state
            .files
            .lock()
            .unwrap()
            .values()
            .find(|(path, _)| path == file)
            .map(|(_, bytes)| bytes.clone());
        (
            "file".to_string(),
            match found {
                Some(bytes) => (200, bytes),
                None => (404, b"not found".to_vec()),
            },
        )
    } else if let Some(method) = path.strip_prefix(&method_prefix) {
        let answer = match method {
            "getMe" if state.unauthorized.load(Ordering::SeqCst) => {
                refused(401, &format!("Unauthorized for {TOKEN}"), None)
            }
            "getMe" => ok(json!({"id": ME, "is_bot": true, "first_name": "Junees",
                                 "username": BOT_NAME})),
            "getUpdates" => {
                // Recorded as it comes: a poll the gateway drops stays
                // open here until its time is up.
                state.requests.lock().unwrap().push(Received {
                    method: method.to_string(),
                    path: path.clone(),
                    body: body.clone(),
                });
                state.notify.notify_waiters();
                let conflict = {
                    let mut left = state.conflict_next.lock().unwrap();
                    let conflict = *left > 0;
                    *left = left.saturating_sub(1);
                    conflict
                };
                if conflict {
                    refused(
                        409,
                        "Conflict: terminated by other getUpdates request",
                        None,
                    )
                } else {
                    ok(Value::Array(get_updates(&state, &body).await))
                }
            }
            "sendMessage" => {
                let limited = {
                    let mut left = state.limit_next.lock().unwrap();
                    let limited = *left > 0;
                    *left = left.saturating_sub(1);
                    limited
                };
                if limited {
                    refused(
                        429,
                        "Too Many Requests: retry after 1",
                        Some(json!({"retry_after": 1})),
                    )
                } else {
                    ok(json!({"message_id": 1, "date": 0, "chat": {"id": body["chat_id"]}}))
                }
            }
            "sendChatAction" => ok(json!(true)),
            "getFile" => {
                let id = body["file_id"].as_str().unwrap_or_default();
                match state.files.lock().unwrap().get(id) {
                    Some((path, bytes)) => ok(json!({"file_id": id, "file_unique_id": id,
                        "file_size": bytes.len(), "file_path": path})),
                    None => refused(400, "Bad Request: invalid file_id", None),
                }
            }
            _ => refused(404, "Not Found", None),
        };
        (method.to_string(), answer)
    } else {
        ("?".to_string(), refused(404, "Not Found", None))
    };
    let mut path = path;
    if method == "sendMessage" && status == 429 {
        path.push_str("#limited");
    }
    let head = format!(
        "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n",
        answer.len()
    );
    if method != "getUpdates" {
        state
            .requests
            .lock()
            .unwrap()
            .push(Received { method, path, body });
    }
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(&answer).await;
    let _ = socket.shutdown().await;
    state.notify.notify_waiters();
}

/// getUpdates as the Bot API does it: updates below `offset` are
/// confirmed and gone; the rest come at once, else after up to `timeout`
/// seconds of waiting.
async fn get_updates(state: &BotState, body: &Value) -> Vec<Value> {
    let wait = Duration::from_secs(body["timeout"].as_u64().unwrap_or(0));
    let limit = body["limit"].as_u64().unwrap_or(100) as usize;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let notified = state.notify.notified();
        let mut pending: Vec<Value> = {
            let mut updates = state.updates.lock().unwrap();
            if let Some(offset) = body["offset"].as_i64() {
                updates.retain(|update| update["update_id"].as_i64().unwrap() >= offset);
            }
            updates.iter().take(limit).cloned().collect()
        };
        let resent: Vec<Value> = std::mem::take(&mut *state.resend.lock().unwrap());
        if !pending.is_empty() || !resent.is_empty() || tokio::time::Instant::now() >= deadline {
            pending.extend(resent);
            return pending;
        }
        let _ = tokio::time::timeout_at(deadline, notified).await;
    }
}

async fn read_http(socket: &mut TcpStream) -> Option<(String, Value)> {
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
    let body = &buf[head_end + 4..head_end + 4 + length];
    Some((path, serde_json::from_slice(body).unwrap_or(Value::Null)))
}

// ----------------------------------------------------------- Messages

fn user(id: i64) -> Value {
    json!({"id": id, "is_bot": false, "first_name": "Someone"})
}

/// A message from `from` in a private chat with the bot.
fn private(from: i64, text: &str) -> Value {
    json!({
        "message_id": 1, "date": 0, "from": user(from),
        "chat": {"id": from, "type": "private", "first_name": "Someone"},
        "text": text,
    })
}

/// A message from `from` in the group, with a mention entity for every
/// `@name` in it.
fn group(from: i64, text: &str) -> Value {
    let mut entities = Vec::new();
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut at = 0;
    while at < units.len() {
        if units[at] == u16::from(b'@') {
            let len = units[at + 1..]
                .iter()
                .take_while(|&&u| {
                    u < 128 && (u as u8 as char).is_ascii_alphanumeric() || u == u16::from(b'_')
                })
                .count();
            entities.push(json!({"type": "mention", "offset": at, "length": len + 1}));
            at += len + 1;
        } else {
            at += 1;
        }
    }
    json!({
        "message_id": 2, "date": 0, "from": user(from),
        "chat": {"id": GROUP, "type": "supergroup", "title": "Team"},
        "text": text, "entities": entities,
    })
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
        if input == "wait" {
            self.cancel.cancelled().await;
            return Err(Cancelled.into());
        }
        if input == "long" {
            let lines: Vec<String> = (0..900).map(|n| format!("줄 {n}: 값은 {n}")).collect();
            out.send(Progress::Text(lines.join("\n"))).unwrap();
            return Ok(());
        }
        if input == "long code" {
            out.send(Progress::Text(long_code())).unwrap();
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

/// A code block longer than one message.
fn long_code() -> String {
    let lines: Vec<String> = (0..900).map(|n| format!("let x{n} = {n};")).collect();
    format!("```rust\n{}\n```", lines.join("\n"))
}

// -------------------------------------------------------------- Setup

fn telegram(api: &str, extra: &[(&str, &str)]) -> TelegramConfig {
    let mut map: HashMap<String, String> = [
        ("JUNEES_TELEGRAM_TOKEN", TOKEN),
        ("JUNEES_TELEGRAM_ALLOWED_USERS", "111"),
        ("JUNEES_TELEGRAM_API_URL", api),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (key, value) in extra {
        map.insert(key.to_string(), value.to_string());
    }
    TelegramConfig::from_lookup(|key| map.get(key).cloned()).unwrap()
}

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
    config: TelegramConfig,
    engines: Arc<dyn Engines>,
    home: Option<PathBuf>,
    cwd: &Path,
) -> Running {
    let (log, lines) = capture();
    let gateway = TelegramGateway {
        config,
        engines,
        home,
        cwd: cwd.to_path_buf(),
        memory: None,
        timing: Timing {
            poll: Duration::from_secs(1),
            backoff_start: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
        },
        typing_every: Duration::from_secs(3600),
        log,
    };
    let stop = Cancel::new();
    let cancel = stop.clone();
    // Its chats run on one thread (a LocalSet), so the future is not
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

fn fake() -> Arc<dyn Engines> {
    Arc::new(FakeEngines)
}

// -------------------------------------------------------------- Tests

#[tokio::test]
async fn answers_only_allowed_users_where_it_is_addressed() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let me = USER.to_string();
    let chat = GROUP.to_string();

    // A stranger is ignored (and logged once); an allowed user is answered.
    bot.push(private(STRANGER, "hello"));
    bot.push(private(STRANGER, "hello again"));
    bot.push(private(USER, "hello"));
    let posts = bot.wait_post(&me, |p| p == "reply 1: hello").await;
    // The tool line comes as plain text before the reply.
    assert_eq!(posts, ["-> bash echo hi", "reply 1: hello"]);
    assert!(bot.posts(&STRANGER.to_string()).is_empty());

    // In a group: not without a mention or a reply to the bot.
    bot.push(group(USER, "just chatting"));
    bot.push(group(USER, "@Junees_Bot what is up?"));
    bot.wait_post(&chat, |p| p == "reply 1: what is up?").await;
    let mut reply = group(USER, "and now?");
    reply["reply_to_message"] = json!({"message_id": 1, "date": 0, "from":
        {"id": ME, "is_bot": true, "first_name": "Junees"}, "chat": {"id": GROUP,
        "type": "supergroup"}, "text": "reply 1"});
    bot.push(reply);
    bot.wait_post(&chat, |p| p == "reply 2: and now?").await;
    // Commands: another bot's and unknown ones are left alone in a group.
    bot.push(group(USER, "/status@other_bot"));
    bot.push(group(USER, "/weather"));
    bot.push(group(USER, "/status@junees_bot"));
    let posts = bot
        .wait_post(&chat, |p| p.contains("this chat: session not saved"))
        .await;
    assert!(
        !posts.iter().any(|p| p.contains("just chatting")
            || p.contains("unknown command")
            || p.matches("this chat:").count() > 1),
        "{posts:#?}"
    );
    assert_eq!(posts.iter().filter(|p| p.contains("this chat:")).count(), 1);
    // In private an unknown command is answered, and /start is the help;
    // a command naming another bot is not run (nor sent to the model).
    bot.push(private(USER, "/new@other_bot"));
    bot.push(private(USER, "/model opus"));
    bot.wait_post(&me, |p| p == "unknown command /model; /help lists them")
        .await;
    // A one-word path that starts a sentence is a message (#58).
    bot.push(private(USER, "/tmp is full"));
    bot.wait_post(&me, |p| p.ends_with(": /tmp is full")).await;
    // Inputs run in order: once this one is answered, an input made of
    // the other bot's command would have been too.
    bot.push(private(USER, "ping"));
    let posts = bot.wait_post(&me, |p| p.ends_with(": ping")).await;
    assert!(
        !posts
            .iter()
            .any(|p| p.contains("other_bot") || p.contains("new conversation")),
        "{posts:#?}"
    );
    bot.push(private(USER, "/start"));
    bot.wait_post(&me, |p| p.starts_with("/new start a new conversation"))
        .await;

    // A forum topic is a conversation of its own, answered in the topic.
    let mut topic = group(USER, "@junees_bot in the topic");
    topic["is_topic_message"] = json!(true);
    topic["message_thread_id"] = json!(77);
    bot.push(topic);
    bot.wait_post(&format!("{GROUP}:77"), |p| p == "reply 1: in the topic")
        .await;

    let log = running.stop().await;
    let ignored: Vec<&String> = log
        .iter()
        .filter(|line| line.contains("ignoring messages of user 222"))
        .collect();
    assert_eq!(ignored.len(), 1, "{log:#?}");
    assert!(log.iter().all(|line| !line.contains(SECRET)), "{log:#?}");
    assert!(
        log.iter().all(|line| !line.contains("what is up")),
        "the log holds no message text: {log:#?}"
    );
}

#[tokio::test]
async fn updates_are_confirmed_and_never_run_twice() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let me = USER.to_string();
    let first = bot.push(private(USER, "one"));
    bot.wait_post(&me, |p| p == "reply 1: one").await;
    // The next poll confirms it.
    bot.wait_offset(first + 1).await;

    // A server that sends an update again: it is not run again.
    let second = bot.push(private(USER, "two"));
    bot.wait_post(&me, |p| p == "reply 2: two").await;
    bot.state.resend.lock().unwrap().push(json!({
        "update_id": second, "message": private(USER, "two")
    }));
    bot.state.notify.notify_waiters();
    let third = bot.push(private(USER, "three"));
    bot.wait_post(&me, |p| p == "reply 3: three").await;
    let log = running.stop().await;
    assert!(log.iter().any(|line| line.contains("stopping")), "{log:#?}");
    // Stopping confirms the last batch without waiting for more.
    let polls = bot.calls("getUpdates");
    let last = polls.last().unwrap();
    assert_eq!(last.body["offset"], third + 1);
    assert_eq!(last.body["timeout"], 0);
    assert!(
        polls
            .iter()
            .all(|r| r.body["allowed_updates"] == json!(["message"]))
    );
    let posts = bot.posts(&me);
    assert_eq!(posts.iter().filter(|p| p.ends_with(": two")).count(), 1);

    // A restarted gateway runs none of them again.
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let fourth = bot.push(private(USER, "four"));
    let posts = bot.wait_post(&me, |p| p == "reply 1: four").await;
    running.stop().await;
    assert_eq!(
        posts.iter().filter(|p| p.starts_with("reply ")).count(),
        4,
        "{posts:#?}"
    );
    let first_poll = bot
        .calls("getUpdates")
        .into_iter()
        .find(|r| r.body["offset"] == fourth + 1);
    assert!(first_poll.is_some());
}

#[tokio::test]
async fn long_replies_are_split_and_flood_control_is_waited_out() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let me = USER.to_string();
    *bot.state.limit_next.lock().unwrap() = 1;
    bot.push(private(USER, "long"));
    let deadline = tokio::time::Instant::now() + WAIT;
    let posts = loop {
        let posts = bot.posts(&me);
        if posts.iter().any(|p| p.ends_with("줄 899: 값은 899")) {
            break posts;
        }
        assert!(tokio::time::Instant::now() < deadline, "{posts:#?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    running.stop().await;
    assert!(posts.len() >= 4, "{} posts", posts.len());
    for post in &posts {
        assert!(post.encode_utf16().count() <= 4096, "{}", post.len());
    }
    let lines: Vec<String> = (0..900).map(|n| format!("줄 {n}: 값은 {n}")).collect();
    assert_eq!(posts.join("\n"), lines.join("\n"));

    // Plain text: a code block cut between messages gets no fences added
    // (Telegram would show them as typed).
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let before = posts.len();
    bot.push(private(USER, "long code"));
    let posts = bot
        .wait_post(&me, |p| p.ends_with("let x899 = 899;\n```"))
        .await;
    running.stop().await;
    let code = &posts[before..];
    assert!(code.len() >= 2, "{} posts", code.len());
    assert_eq!(code.join("\n"), long_code());
    // The first post was refused once by flood control, then sent once.
    let limited = bot
        .calls("sendMessage")
        .iter()
        .filter(|r| r.path.contains("limited"))
        .count();
    assert_eq!(limited, 1);
}

#[tokio::test]
async fn stop_status_and_new() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let me = USER.to_string();
    bot.push(private(USER, "/stop"));
    bot.wait_post(&me, |p| p == "(nothing is running here)")
        .await;
    let before = bot.typing(&me);
    bot.push(private(USER, "wait"));
    bot.wait_typing(&me, before).await;
    bot.push(private(USER, "/status"));
    let posts = bot
        .wait_post(&me, |p| p.contains("this chat: session not saved"))
        .await;
    let status = posts.iter().find(|p| p.contains("this chat:")).unwrap();
    assert!(status.contains("a turn is running, 0 queued"), "{status}");
    bot.push(private(USER, "!stop"));
    bot.wait_post(&me, |p| p == "(stopped)").await;
    bot.push(private(USER, "after"));
    bot.wait_post(&me, |p| p == "reply 1: after").await;
    bot.push(private(USER, "/new"));
    bot.wait_post(&me, |p| p == "(new conversation) session not saved")
        .await;
    bot.push(private(USER, "again"));
    bot.wait_post(&me, |p| p == "reply 1: again").await;
    bot.push(private(USER, "/memory"));
    bot.wait_post(&me, |p| p.starts_with("memory is off")).await;
    running.stop().await;
}

#[tokio::test]
async fn a_command_after_the_mention_is_a_command_in_a_group() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let chat = GROUP.to_string();
    let before = bot.typing(&chat);
    bot.push(group(USER, "@junees_bot wait"));
    bot.wait_typing(&chat, before).await;
    // The running turn stops; "/stop" never reaches the model.
    bot.push(group(USER, "@junees_bot /stop"));
    bot.wait_post(&chat, |p| p == "(stopped)").await;
    bot.push(group(USER, "@Junees_Bot !new"));
    bot.wait_post(&chat, |p| p == "(new conversation) session not saved")
        .await;
    // Naming the bot, an unknown command is answered as in private; one
    // for another bot is not run.
    bot.push(group(USER, "@junees_bot /model opus"));
    bot.wait_post(&chat, |p| p == "unknown command /model; /help lists them")
        .await;
    bot.push(group(USER, "@junees_bot /new@other_bot"));
    bot.push(group(USER, "@junees_bot after"));
    let posts = bot.wait_post(&chat, |p| p == "reply 1: after").await;
    running.stop().await;
    assert!(
        !posts
            .iter()
            .any(|p| p.contains("/stop") || p.contains("other_bot")),
        "{posts:#?}"
    );
    assert_eq!(
        posts
            .iter()
            .filter(|p| p.starts_with("(new conversation)"))
            .count(),
        1,
        "{posts:#?}"
    );
}

#[tokio::test]
async fn forwarded_messages_are_marked_and_never_commands() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let me = USER.to_string();
    let mut forwarded = private(USER, "/stop");
    forwarded["forward_origin"] = json!({"type": "user", "date": 0, "sender_user":
        {"id": STRANGER, "is_bot": false, "first_name": "Mallory"}});
    bot.push(forwarded);
    let posts = bot
        .wait_post(&me, |p| {
            p == "reply 1: (forwarded message from Mallory; they wrote it, not the user)\n/stop"
        })
        .await;
    assert!(
        !posts.iter().any(|p| p.contains("nothing is running")),
        "{posts:#?}"
    );
    // The user's own words are not marked.
    bot.push(private(USER, "my own"));
    bot.wait_post(&me, |p| p == "reply 2: my own").await;
    running.stop().await;
}

#[tokio::test]
async fn a_conflict_is_waited_out_a_few_times() {
    let bot = FakeBot::start().await;
    let dir = tempfile::tempdir().unwrap();
    // Someone else's one-off getUpdates: the gateway polls on.
    *bot.state.conflict_next.lock().unwrap() = 3;
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let me = USER.to_string();
    bot.push(private(USER, "still here?"));
    bot.wait_post(&me, |p| p == "reply 1: still here?").await;
    let log = running.stop().await;
    assert_eq!(
        log.iter().filter(|line| line.contains("error 409")).count(),
        3,
        "{log:#?}"
    );

    // A second gateway on the same token keeps taking the updates: the
    // gateway ends after a few tries and says why.
    *bot.state.conflict_next.lock().unwrap() = usize::MAX;
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let (result, log) = running.ended().await;
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("error 409") && err.contains("deleteWebhook"),
        "{err}"
    );
    assert_eq!(
        log.iter().filter(|line| line.contains("error 409")).count(),
        5,
        "{log:#?}"
    );
}

#[tokio::test]
async fn a_refused_token_stops_without_showing_it() {
    let bot = FakeBot::start().await;
    bot.state.unauthorized.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let running = start(telegram(&bot.url, &[]), fake(), None, dir.path());
    let (result, log) = running.ended().await;
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("getMe: error 401"), "{err}");
    assert!(err.contains("check the bot token"), "{err}");
    assert!(!err.contains(SECRET), "{err}");
    assert!(log.iter().all(|line| !line.contains(SECRET)), "{log:#?}");

    // Nobody listening: the error names neither the token nor the URL.
    let running = start(
        telegram("http://127.0.0.1:9", &[]),
        fake(),
        None,
        dir.path(),
    );
    let (result, _) = running.ended().await;
    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("cannot connect"), "{err}");
    assert!(!err.contains(SECRET) && !err.contains("/bot"), "{err}");
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
async fn risky_calls_wait_for_an_answer_in_the_chat() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let bot = FakeBot::start().await;
    // No shell runs: `all` asks before every file write too.
    let file = cwd.path().join("note.txt");
    let (api, served) = common::fake_api(vec![
        (200, write_call(&file)),
        (200, common::text_reply("declined, fine")),
        (200, write_call(&file)),
        (200, common::text_reply("wrote it")),
        (200, common::text_reply("about the forward")),
        (200, write_call(&file)),
        (200, common::text_reply("timed out")),
    ])
    .await;
    let engines = Arc::new(
        ApiEngines::new(api_config(&api, home.path(), "all"), cwd.path().into())
            .with_note(TELEGRAM_NOTE),
    );
    let running = start(
        telegram(&bot.url, &[("JUNEES_TELEGRAM_APPROVAL_TIMEOUT", "1s")]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let me = USER.to_string();
    bot.push(private(USER, "write a note"));
    let posts = bot
        .wait_post(&me, |p| p.starts_with("Approve this tool call?"))
        .await;
    let question = posts.last().unwrap();
    assert!(question.contains("approve write_file:"), "{question}");
    assert!(!question.contains("```"), "{question}");
    bot.push(private(USER, "n not today"));
    bot.wait_post(&me, |p| p == "declined, fine").await;
    assert!(!file.exists());

    bot.push(private(USER, "now"));
    bot.wait_posts(&me, 2, |p| p.starts_with("Approve this tool call?"))
        .await;
    // A forwarded message is someone else's words: no answer to the
    // question (#58), but an input for after the turn.
    let mut forwarded = private(USER, "n");
    forwarded["forward_origin"] = json!({"type": "user", "date": 0, "sender_user":
        {"id": STRANGER, "is_bot": false, "first_name": "Mallory"}});
    bot.push(forwarded);
    bot.push(private(USER, "y"));
    bot.wait_post(&me, |p| p == "wrote it").await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "written\n");
    std::fs::remove_file(&file).unwrap();
    bot.wait_post(&me, |p| p == "about the forward").await;

    bot.push(private(USER, "once more"));
    bot.wait_post(&me, |p| p == "(no answer within 1s: declined)")
        .await;
    bot.wait_post(&me, |p| p == "timed out").await;
    assert!(!file.exists());
    running.stop().await;

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let system = common::body_of(&requests[0])["system"].to_string();
    assert!(system.contains("through Telegram"), "{system}");
    assert_eq!(
        texts(&requests[1]).last().unwrap(),
        "user: the user declined: not today"
    );
    assert!(!texts(&requests[3]).last().unwrap().contains("declined"));
    assert_eq!(
        texts(&requests[4]).last().unwrap(),
        "user: (forwarded message from Mallory; they wrote it, not the user)\nn"
    );
    assert_eq!(
        texts(&requests[6]).last().unwrap(),
        "user: the user declined: no answer within 1s"
    );
    // The chat's session is an ordinary journal, marked as Telegram's.
    let index: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("gateway/telegram-sessions.json")).unwrap(),
    )
    .unwrap();
    let id = index[&me].as_str().unwrap();
    let journal =
        std::fs::read_to_string(home.path().join(format!("sessions/{id}.jsonl"))).unwrap();
    let meta: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
    assert_eq!(
        (&meta["source"], &meta["channel"]),
        (&json!("telegram"), &json!(me))
    );
    assert!(home.path().join("gateway/telegram.lock").exists());
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

fn photo(from: i64, file_id: &str, caption: Option<&str>, album: Option<&str>) -> Value {
    let mut message = private(from, "");
    message.as_object_mut().unwrap().remove("text");
    message["photo"] = json!([
        {"file_id": format!("{file_id}-thumb"), "file_unique_id": "t", "width": 9, "height": 6,
         "file_size": 100},
        {"file_id": file_id, "file_unique_id": "p", "width": 20, "height": 10, "file_size": 45}
    ]);
    if let Some(caption) = caption {
        message["caption"] = json!(caption);
    }
    if let Some(album) = album {
        message["media_group_id"] = json!(album);
    }
    message
}

#[tokio::test]
async fn photos_and_albums_reach_the_model() {
    use base64::Engine as _;
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let bot = FakeBot::start().await;
    let picture = png(20, 10);
    bot.file("ph1", "photos/file_1.png", picture.clone());
    bot.file("ph2", "photos/file_2.png", png(30, 20));
    let (api, served) = common::fake_api(vec![
        (200, common::text_reply("A small chart.")),
        (200, common::text_reply("Two pictures.")),
    ])
    .await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        telegram(&bot.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let me = USER.to_string();
    // A stranger's photo is never downloaded.
    bot.push(photo(STRANGER, "ph1", Some("look"), None));
    bot.push(photo(USER, "ph1", Some("what is this?"), None));
    bot.wait_post(&me, |p| p == "A small chart.").await;
    assert_eq!(bot.calls("getFile").len(), 1);
    // A document that is no image is named; an album is one input.
    let mut document = private(USER, "");
    document.as_object_mut().unwrap().remove("text");
    document["document"] = json!({"file_id": "doc", "file_unique_id": "d",
        "file_name": "notes.txt", "mime_type": "text/plain", "file_size": 10});
    bot.push(document);
    bot.wait_post(&me, |p| {
        p == "(attachments left out: \"notes.txt\" is not an image; only images are read)"
    })
    .await;
    // Both parts of the album in one batch: hold the poll until both are
    // queued.
    {
        let mut updates = bot.state.updates.lock().unwrap();
        for (n, message) in [
            photo(USER, "ph1", Some("compare these"), Some("album-1")),
            photo(USER, "ph2", None, Some("album-1")),
        ]
        .into_iter()
        .enumerate()
        {
            updates.push(json!({"update_id": 900 + n as i64, "message": message}));
        }
    }
    bot.state.notify.notify_waiters();
    bot.wait_post(&me, |p| p == "Two pictures.").await;
    running.stop().await;

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let first = common::body_of(&requests[0]);
    let content = &first["messages"][0]["content"];
    // Images go before the text.
    assert_eq!(content[1]["text"], "what is this?");
    assert_eq!(content[0]["type"], "image");
    assert_eq!(
        content[0]["source"]["data"],
        base64::engine::general_purpose::STANDARD.encode(&picture)
    );
    let second = common::body_of(&requests[1]);
    let last = second["messages"].as_array().unwrap().last().unwrap();
    let kinds: Vec<&str> = last["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["image", "image", "text"], "{last}");
    assert_eq!(last["content"][2]["text"], "compare these");
    // Every download came from the Bot API's file URL.
    let downloads = bot.calls("file");
    assert_eq!(downloads.len(), 3);
    assert!(
        downloads
            .iter()
            .all(|r| r.path.starts_with(&format!("/tg/file/bot{TOKEN}/photos/")))
    );
}

#[tokio::test]
async fn a_caption_is_never_a_command() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let bot = FakeBot::start().await;
    bot.file("ph1", "photos/file_1.png", png(20, 10));
    let (api, served) =
        common::fake_api(vec![(200, common::text_reply("A photo with a caption."))]).await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        telegram(&bot.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let me = USER.to_string();
    bot.push(photo(USER, "ph1", Some("/new"), None));
    let posts = bot.wait_post(&me, |p| p == "A photo with a caption.").await;
    running.stop().await;
    assert!(
        !posts.iter().any(|p| p.starts_with("(new conversation)")),
        "{posts:#?}"
    );
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let content = &common::body_of(&requests[0])["messages"][0]["content"];
    assert_eq!(content[0]["type"], "image", "{content}");
    assert_eq!(content[1]["text"], "/new", "{content}");
}

/// A photo from `from` in the group: part of `album`, with a caption (and
/// its mention entities) or none.
fn group_photo(from: i64, file_id: &str, caption: Option<&str>, album: &str) -> Value {
    let mut message = photo(from, file_id, None, Some(album));
    let in_group = group(from, caption.unwrap_or_default());
    message["chat"] = in_group["chat"].clone();
    if let Some(caption) = caption {
        message["caption"] = json!(caption);
        message["caption_entities"] = in_group["entities"].clone();
    }
    message
}

#[tokio::test]
async fn a_group_album_split_over_two_answers_is_taken_whole() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let bot = FakeBot::start().await;
    bot.file("ph1", "photos/file_1.png", png(20, 10));
    bot.file("ph2", "photos/file_2.png", png(30, 20));
    bot.file("ph3", "photos/file_3.png", png(40, 20));
    let (api, served) = common::fake_api(vec![
        (200, common::text_reply("First picture.")),
        (200, common::text_reply("Second picture.")),
        (200, common::text_reply("Done.")),
    ])
    .await;
    let engines = Arc::new(ApiEngines::new(
        api_config(&api, home.path(), "off"),
        cwd.path().into(),
    ));
    let running = start(
        telegram(&bot.url, &[]),
        engines,
        Some(home.path().into()),
        cwd.path(),
    );
    let chat = GROUP.to_string();
    bot.push(group_photo(
        USER,
        "ph1",
        Some("@junees_bot compare these"),
        "album-9",
    ));
    bot.wait_post(&chat, |p| p == "First picture.").await;
    // The rest of the album comes in a later answer, with no caption.
    bot.push(group_photo(USER, "ph2", None, "album-9"));
    bot.wait_post(&chat, |p| p == "Second picture.").await;
    // An album nobody addressed to the bot is still left alone.
    bot.push(group_photo(USER, "ph3", None, "album-10"));
    bot.push(group(USER, "@junees_bot done"));
    bot.wait_post(&chat, |p| p == "Done.").await;
    running.stop().await;

    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    let second = common::body_of(&requests[1]);
    let last = second["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["content"][0]["type"], "image", "{last}");
    let downloads: Vec<String> = bot
        .calls("getFile")
        .iter()
        .map(|r| r.body["file_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(downloads, ["ph1", "ph2"]);
}

// ------------------------------------------------------------- Binary

#[tokio::test]
async fn the_binary_answers_and_stops_on_sigterm() {
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let bot = FakeBot::start().await;
    let (api, served) = common::fake_api(vec![(200, common::text_reply("binary answer"))]).await;

    // Without allowed users it refuses to start.
    let refused = common::junees_binary(&cwd, home.path())
        .args(["gateway", "telegram"])
        .env("JUNEES_HOME", home.path().join(".junees"))
        .env("JUNEES_TELEGRAM_TOKEN", TOKEN)
        .env("JUNEES_TELEGRAM_API_URL", &bot.url)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("no allowed Telegram users"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");

    let child = common::junees_binary(&cwd, home.path())
        .args(["gateway", "telegram"])
        .env("JUNEES_HOME", home.path().join(".junees"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("ANTHROPIC_BASE_URL", &api)
        .env("JUNEES_STREAM", "0")
        .env("JUNEES_TELEGRAM_TOKEN", TOKEN)
        .env("JUNEES_TELEGRAM_ALLOWED_USERS", "111")
        .env("JUNEES_TELEGRAM_API_URL", &bot.url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pushed = bot.push(private(USER, "hello from the binary"));
    bot.wait_post(&USER.to_string(), |p| p == "binary answer")
        .await;
    bot.wait_offset(pushed + 1).await;
    let pid = i32::try_from(child.id().unwrap()).unwrap();
    // SAFETY: kill only sends a signal to the child started above.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("the gateway did not stop on SIGTERM")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("telegram gateway: @junees_bot"), "{stderr}");
    assert!(stderr.contains("telegram gateway stopped"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    let requests = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert_eq!(texts(&requests[0]), ["user: hello from the binary"]);
    // It confirmed the update before it stopped.
    let last = bot.calls("getUpdates").pop().unwrap();
    assert_eq!(last.body["offset"], pushed + 1);
    assert_eq!(last.body["timeout"], 0);
}
