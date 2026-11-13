//! `junees dashboard --chat`: a conversation with Junees on the page,
//! with the trust of a terminal conversation. Approval questions go to the
//! page and wait for its buttons. The contract is in `docs/dashboard.md`
//! ("대화").
//!
//! - `POST /api/chat` `{"session_id"?}`: a new chat, or one of `--dir`'s
//!   sessions continued: 200 `{"session_id"}`.
//! - `POST /api/chat/<id>/messages` `{"text"}`: a turn, 202 `{"turn"}`;
//!   409 while any turn runs.
//! - `GET /api/chat/<id>/events?after=<seq>`: the chat's events as
//!   `text/event-stream` ([`stream`]).
//! - `POST /api/chat/<id>/approvals/<question_id>` `{"answer"}`: answers
//!   a question; 404 when there is none, 409 when it is closed.
//! - `POST /api/chat/<id>/cancel`: stops the running turn.
//!
//! The agents run on a thread of their own ([`runner`]): their futures are
//! not `Send`. Each chat is a [`Room`]: its events (the last
//! [`MAX_EVENTS`]), its questions and its cancel.

mod runner;
pub(super) mod stream;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use super::server::Answer;
use crate::approval::Answer as Approval;
use crate::cancel::Cancel;
use crate::config_file::Settings;
use crate::gateway::engine::ApiEngines;
use crate::mcp::Mcp;

pub use runner::DASHBOARD_NOTE;

/// Events kept per chat for `after` to replay.
pub const MAX_EVENTS: usize = 1000;
/// The longest message, in characters.
pub const MAX_TEXT_CHARS: usize = 32_000;
/// The body a message may announce: 32,000 characters, each escaped.
pub const MAX_MESSAGE_BODY: u64 = 512 * 1024;
/// How long a question waits for the page before it is declined.
pub const APPROVAL_TIME: Duration = Duration::from_secs(5 * 60);
/// Chats with an agent at once; opening one more closes the one unused
/// longest (it can be continued again by its session id).
pub const MAX_ROOMS: usize = 16;
/// Event streams open at once.
pub const MAX_STREAMS: usize = 8;
/// Questions remembered per chat, closed ones included.
const MAX_QUESTIONS: usize = 256;

/// The last event number given, over every chat of this process: a chat
/// opened again goes on from here, so a page's `after` from before still
/// means "what I have seen".
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// What `junees dashboard --chat` serves the chat with.
pub struct Setup {
    /// The agents: Junees's own on the API, with the tools, MCP servers
    /// and plugins of the terminal conversation.
    pub engines: ApiEngines,
    /// `--dir`, absolute: where the chat works and whose sessions it may
    /// continue.
    pub dir: PathBuf,
    /// The state folder (sessions, `dashboard.log`).
    pub home: Option<PathBuf>,
    pub model: String,
    /// For hiding secrets and the prices.
    pub settings: Settings,
    /// Stopped with the dashboard.
    pub mcp: Arc<Mcp>,
}

/// The chat of one dashboard.
pub struct Chat {
    dir: PathBuf,
    home: Option<PathBuf>,
    commands: mpsc::UnboundedSender<runner::Command>,
    rooms: Mutex<HashMap<String, (Arc<Room>, Instant)>>,
    /// The chat whose turn runs: one turn at a time, as in one terminal.
    running: Arc<Mutex<Option<String>>>,
    streams: Arc<Semaphore>,
    mcp: Arc<Mcp>,
    /// The write rules an undo goes through: the file tools'.
    access: crate::tools::FileAccess,
}

/// One chat: its events, questions and cancel.
pub struct Room {
    pub id: String,
    log: Mutex<Log>,
    /// The last event's seq, for streams to wait on.
    seq: watch::Sender<u64>,
    turns: AtomicU64,
    cancel: Cancel,
    questions: Mutex<Questions>,
    /// Set when the chat is closed: its streams end.
    closed: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Log {
    /// `(seq, JSON line)`, oldest first.
    events: VecDeque<(u64, String)>,
    last: u64,
}

#[derive(Default)]
struct Questions {
    open: HashMap<String, Question>,
    /// Ids in the order they were asked, to forget the oldest.
    order: VecDeque<String>,
}

struct Question {
    /// Taken when it closes.
    reply: Option<oneshot::Sender<Approval>>,
    /// The answer and who gave it, once closed.
    closed: Option<(String, String)>,
    /// Whether `a` is offered.
    always: bool,
}

/// Why a question could not be answered.
#[derive(Debug, PartialEq, Eq)]
pub enum Closing {
    Missing,
    Closed,
}

impl Room {
    fn new(id: String) -> Self {
        Self {
            id,
            log: Mutex::default(),
            seq: watch::channel(0).0,
            turns: AtomicU64::new(0),
            cancel: Cancel::new(),
            questions: Mutex::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Ends the chat's streams and stops its turn.
    fn shut(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancel.cancel();
        let last = *self.seq.borrow();
        self.seq.send_replace(last);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Records `event` (an object with `type`), numbered from the process
    /// counter; streams wake.
    pub fn push(&self, event: Value) -> u64 {
        let seq = {
            let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
            // Under the lock: numbers rise in the order events are kept.
            let seq = LAST_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
            log.last = seq;
            log.events.push_back((seq, event.to_string()));
            while log.events.len() > MAX_EVENTS {
                log.events.pop_front();
            }
            seq
        };
        self.seq.send_replace(seq);
        seq
    }

    /// Where a stream asked to start after `after` starts: past this
    /// chat's last event, `after` is from another run of the dashboard,
    /// and the stream starts from the oldest kept event.
    pub fn start_after(&self, after: u64) -> u64 {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        if after > log.last { 0 } else { after }
    }

    /// The kept events after `after`, oldest first.
    pub fn after(&self, after: u64) -> Vec<(u64, String)> {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        log.events
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect()
    }

    /// Opens question `id`; `a` is offered when `always`.
    fn ask(&self, id: &str, always: bool) -> oneshot::Receiver<Approval> {
        let (reply, answer) = oneshot::channel();
        let mut questions = self
            .questions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        questions.open.insert(
            id.to_string(),
            Question {
                reply: Some(reply),
                closed: None,
                always,
            },
        );
        questions.order.push_back(id.to_string());
        // Forget the oldest closed ones.
        while questions.order.len() > MAX_QUESTIONS {
            let Some(oldest) = questions.order.pop_front() else {
                break;
            };
            if questions
                .open
                .get(&oldest)
                .is_some_and(|question| question.closed.is_none())
            {
                questions.order.push_back(oldest);
                break;
            }
            questions.open.remove(&oldest);
        }
        answer
    }

    /// Closes question `id` with `answer` (`y`, `a`, `n`), given `by` the
    /// page, a timeout or a cancel, and says so in an event. The sender
    /// of the answer, when it is still waited for.
    fn close(
        &self,
        id: &str,
        answer: &str,
        by: &str,
    ) -> Result<Option<oneshot::Sender<Approval>>, Closing> {
        let reply = {
            let mut questions = self
                .questions
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let question = questions.open.get_mut(id).ok_or(Closing::Missing)?;
            if question.closed.is_some() {
                return Err(Closing::Closed);
            }
            question.closed = Some((answer.to_string(), by.to_string()));
            question.reply.take()
        };
        self.push(json!({
            "type": "approval_closed",
            "question_id": id,
            "answer": answer,
            "by": by,
        }));
        Ok(reply)
    }

    /// Whether question `id` offers `a`; `None` when there is none.
    fn offers_always(&self, id: &str) -> Option<bool> {
        let questions = self
            .questions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        questions.open.get(id).map(|question| question.always)
    }
}

/// A request of the chat API, from its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route<'a> {
    Open,
    Messages(&'a str),
    Events(&'a str),
    Approval(&'a str, &'a str),
    Cancel(&'a str),
    /// `GET /api/chat/<id>/checkpoints`.
    Checkpoints(&'a str),
    /// `POST /api/chat/<id>/undo`.
    Undo(&'a str),
}

impl Route<'_> {
    /// Whether it is a `POST`.
    pub fn writes(&self) -> bool {
        !matches!(self, Route::Events(_) | Route::Checkpoints(_))
    }
}

/// The chat request `path` makes, if any. Ids are checked by the handlers.
pub fn route(path: &str) -> Option<Route<'_>> {
    if path == "/api/chat" {
        return Some(Route::Open);
    }
    let rest = path.strip_prefix("/api/chat/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [id, "messages"] => Some(Route::Messages(id)),
        [id, "events"] => Some(Route::Events(id)),
        [id, "cancel"] => Some(Route::Cancel(id)),
        [id, "checkpoints"] => Some(Route::Checkpoints(id)),
        [id, "undo"] => Some(Route::Undo(id)),
        [id, "approvals", question] => Some(Route::Approval(id, question)),
        _ => None,
    }
}

/// Whether `path` is under the chat API (served only with `--chat`).
pub fn is_chat_path(path: &str) -> bool {
    path == "/api/chat" || path.starts_with("/api/chat/")
}

/// The largest body `path` may carry.
pub fn body_limit(path: &str) -> Option<u64> {
    matches!(route(path), Some(Route::Messages(_))).then_some(MAX_MESSAGE_BODY)
}

/// A question id: 1 to 64 of `A-Z`, `a-z`, `0-9`, `_`, `-`.
fn valid_question(id: &str) -> bool {
    super::session_view::valid_id(id)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody {
    session_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UndoBody {
    turn: u64,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageBody {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalBody {
    answer: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// `body` as `T`; an empty body is `{}`.
fn body<T: for<'de> Deserialize<'de>>(body: &[u8], shape: &str) -> Result<T, Answer> {
    let text = std::str::from_utf8(body).map(str::trim).unwrap_or("\u{0}");
    let text = if text.is_empty() { "{}" } else { text };
    serde_json::from_str(text)
        .map_err(|_| Answer::error(400, "bad_body", &format!("the body is {shape}")))
}

/// One turn of `GET /api/chat/<id>/checkpoints`.
fn turn_json(manifest: &crate::checkpoints::Manifest) -> Value {
    json!({
        "turn": manifest.turn,
        "files": manifest.files.iter().map(|file| json!({
            "path": file.path,
            "change": file.change().word(),
            "restorable": file.restorable,
        })).collect::<Vec<_>>(),
    })
}

fn no_chat(id: &str) -> Answer {
    Answer::error(
        404,
        "not_found",
        &format!("there is no open chat `{id}`; open it with POST /api/chat"),
    )
}

impl Chat {
    /// Starts the chat's thread.
    pub fn start(setup: Setup) -> Arc<Self> {
        let running = Arc::new(Mutex::new(None));
        let access = setup.engines.file_access();
        let commands = runner::start(
            setup.engines,
            runner::Shared {
                dir: setup.dir.clone(),
                home: setup.home.clone(),
                model: setup.model,
                settings: Arc::new(setup.settings),
                approval_time: APPROVAL_TIME,
                running: Arc::clone(&running),
            },
        );
        Arc::new(Self {
            dir: setup.dir,
            home: setup.home,
            commands,
            rooms: Mutex::default(),
            running,
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            mcp: setup.mcp,
            access,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stops the running turn, ends the streams and stops the MCP servers.
    pub async fn shutdown(&self) {
        for (room, _) in self.rooms().values() {
            room.shut();
        }
        self.mcp.shutdown().await;
    }

    fn rooms(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Arc<Room>, Instant)>> {
        self.rooms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The open chat `id`, marked used now.
    fn room(&self, id: &str) -> Option<Arc<Room>> {
        let mut rooms = self.rooms();
        let (room, used) = rooms.get_mut(id)?;
        *used = Instant::now();
        Some(Arc::clone(room))
    }

    fn log(&self, line: &str) {
        if let Some(home) = &self.home {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            super::actions::log_line(
                home,
                &format!("{} {line}\n", crate::session::format_utc_seconds(now)),
            );
        }
    }

    /// Writes "chat started" to the log.
    pub fn log_started(&self) {
        self.log(&format!("chat started in {}", self.dir.display()));
    }

    /// A write of the chat API, its checks passed.
    pub async fn write(&self, route: &Route<'_>, raw: &[u8]) -> Answer {
        match route {
            Route::Open => self.open(raw).await,
            Route::Messages(id) => self.message(id, raw),
            Route::Approval(id, question) => self.approve(id, question, raw),
            Route::Cancel(id) => self.cancel(id, raw),
            Route::Undo(id) => self.undo(id, raw).await,
            Route::Events(_) | Route::Checkpoints(_) => {
                Answer::error(405, "method_not_allowed", "only GET is served here")
            }
        }
    }

    async fn open(&self, raw: &[u8]) -> Answer {
        let wanted = match body::<OpenBody>(raw, "{} or {\"session_id\": \"<id>\"}") {
            Ok(body) => body.session_id,
            Err(answer) => return answer,
        };
        let resume = match &wanted {
            None => None,
            Some(id) => {
                if self.room(id).is_some() {
                    return Answer::json(200, &json!({ "session_id": id }));
                }
                match self.session_path(id) {
                    Ok(path) => Some(path),
                    Err(answer) => return answer,
                }
            }
        };
        self.make_room_for_one();
        let (reply, opened) = oneshot::channel();
        if self
            .commands
            .send(runner::Command::Open { resume, reply })
            .is_err()
        {
            return stopped();
        }
        match opened.await {
            Ok(Ok(room)) => {
                let id = room.id.clone();
                self.rooms().insert(id.clone(), (room, Instant::now()));
                self.log(&format!("chat {id} opened"));
                Answer::json(200, &json!({ "session_id": id }))
            }
            Ok(Err(runner::OpenError::Busy(message))) => {
                Answer::error(409, "session_busy", &message)
            }
            Ok(Err(runner::OpenError::Failed(message))) => {
                Answer::error(503, "chat_unavailable", &message)
            }
            Err(_) => stopped(),
        }
    }

    /// With [`MAX_ROOMS`] open, closes the one unused longest whose turn
    /// does not run.
    fn make_room_for_one(&self) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut rooms = self.rooms();
        if rooms.len() < MAX_ROOMS {
            return;
        }
        let oldest = rooms
            .iter()
            .filter(|(id, _)| running.as_deref() != Some(id.as_str()))
            .min_by_key(|(_, (_, used))| *used)
            .map(|(id, _)| id.clone());
        if let Some(id) = oldest
            && let Some((room, _)) = rooms.remove(&id)
        {
            room.shut();
            let _ = self.commands.send(runner::Command::Close { id });
        }
    }

    /// The journal of session `id` when the chat may continue it: a
    /// regular file of the sessions folder, of `--dir`, not a claude-code
    /// session.
    fn session_path(&self, id: &str) -> Result<PathBuf, Answer> {
        let missing = || {
            Answer::error(
                404,
                "not_found",
                &format!("there is no session `{id}` of {}", self.dir.display()),
            )
        };
        if !super::session_view::valid_id(id) {
            return Err(Answer::error(
                400,
                "bad_body",
                "session_id is 1 to 64 letters, digits, `_` or `-`",
            ));
        }
        let home = self.home.as_deref().ok_or_else(missing)?;
        let path = crate::session::sessions_dir(home).join(format!("{id}.jsonl"));
        let is_file = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
        let meta = is_file
            .then(|| crate::session::read_meta(&path))
            .flatten()
            .ok_or_else(missing)?;
        if Path::new(&meta.cwd) != self.dir {
            return Err(missing());
        }
        if meta.backend.is_some() {
            return Err(Answer::error(
                409,
                "conflict",
                &format!("session `{id}` is a claude-code session; the chat runs the api backend"),
            ));
        }
        Ok(path)
    }

    fn message(&self, id: &str, raw: &[u8]) -> Answer {
        let Some(room) = self.room(id) else {
            return no_chat(id);
        };
        let text = match body::<MessageBody>(raw, "{\"text\": \"<message>\"}") {
            Ok(body) => body.text,
            Err(answer) => return answer,
        };
        if text.trim().is_empty() || text.chars().count() > MAX_TEXT_CHARS {
            return Answer::error(
                400,
                "bad_body",
                &format!("text is 1 to {MAX_TEXT_CHARS} characters, not only spaces"),
            );
        }
        {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(other) = running.as_deref() {
                let which = if other == id {
                    "this chat"
                } else {
                    "another chat"
                };
                return Answer::error(
                    409,
                    "turn_running",
                    &format!("a turn of {which} is running; wait for it or cancel it"),
                );
            }
            *running = Some(id.to_string());
        }
        // Here, not on the chat's thread: a cancel sent right after the 202
        // must not be cleared when the turn starts.
        room.cancel.reset();
        let turn = room.turns.fetch_add(1, Ordering::SeqCst) + 1;
        if self
            .commands
            .send(runner::Command::Turn {
                room: Arc::clone(&room),
                text,
                turn,
            })
            .is_err()
        {
            *self.running.lock().unwrap_or_else(PoisonError::into_inner) = None;
            return stopped();
        }
        self.log(&format!("chat {id} message {turn}"));
        Answer::json(202, &json!({ "turn": turn }))
    }

    fn approve(&self, id: &str, question: &str, raw: &[u8]) -> Answer {
        let Some(room) = self.room(id) else {
            return no_chat(id);
        };
        let answer = match body::<ApprovalBody>(raw, "{\"answer\": \"y\" | \"a\" | \"n\"}") {
            Ok(body) => body.answer,
            Err(answer) => return answer,
        };
        if !matches!(answer.as_str(), "y" | "a" | "n") {
            return Answer::error(400, "bad_body", "answer is \"y\", \"a\" or \"n\"");
        }
        let missing = || {
            Answer::error(
                404,
                "not_found",
                &format!("there is no question `{question}` in this chat"),
            )
        };
        if !valid_question(question) {
            return missing();
        }
        if answer == "a" && room.offers_always(question) == Some(false) {
            return Answer::error(
                400,
                "bad_body",
                "this question is asked every time: answer \"y\" or \"n\"",
            );
        }
        match room.close(question, &answer, "browser") {
            Ok(reply) => {
                if let Some(reply) = reply {
                    let _ = reply.send(match answer.as_str() {
                        "y" => Approval::Yes,
                        "a" => Approval::Always,
                        _ => Approval::No,
                    });
                }
                self.log(&format!("chat {id} approval {question} {answer}"));
                Answer::json(200, &json!({ "question_id": question, "answer": answer }))
            }
            Err(Closing::Missing) => missing(),
            Err(Closing::Closed) => Answer::error(
                409,
                "conflict",
                &format!("question `{question}` is already closed"),
            ),
        }
    }

    fn cancel(&self, id: &str, raw: &[u8]) -> Answer {
        let Some(room) = self.room(id) else {
            return no_chat(id);
        };
        if let Err(answer) = body::<Empty>(raw, "{}") {
            return answer;
        }
        let runs = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_deref()
            == Some(id);
        if runs {
            room.cancel.cancel();
            self.log(&format!("chat {id} cancel"));
        }
        Answer::json(200, &json!({ "cancelled": runs }))
    }

    /// `GET /api/chat/<id>/checkpoints`: the chat's turns that changed
    /// files, oldest first (none when checkpoints are off).
    pub fn checkpoints(&self, id: &str) -> Answer {
        if self.room(id).is_none() {
            return no_chat(id);
        }
        let Some(session) = crate::checkpoints::store().and_then(|store| store.session(id)) else {
            return Answer::json(200, &json!([]));
        };
        match session.list() {
            Ok(list) => Answer::json(200, &Value::Array(list.iter().map(turn_json).collect())),
            Err(_) => Answer::error(
                500,
                "checkpoints_unreadable",
                "cannot read the file checkpoints",
            ),
        }
    }

    /// `POST /api/chat/<id>/undo`: undoes turn `turn` and every later one
    /// while no turn runs (none starts meanwhile).
    async fn undo(&self, id: &str, raw: &[u8]) -> Answer {
        if self.room(id).is_none() {
            return no_chat(id);
        }
        let wanted = match body::<UndoBody>(raw, "{\"turn\": <n>, \"force\"?: <bool>}") {
            Ok(body) if body.turn > 0 => body,
            Ok(_) => return Answer::error(400, "bad_body", "turn is a turn number from 1"),
            Err(answer) => return answer,
        };
        let Some(session) = crate::checkpoints::store().and_then(|store| store.session(id)) else {
            return Answer::error(
                409,
                "checkpoints_off",
                "file checkpoints are off (checkpoints = false)",
            );
        };
        {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(other) = running.as_deref() {
                let which = if other == id {
                    "this chat"
                } else {
                    "another chat"
                };
                return Answer::error(
                    409,
                    "turn_running",
                    &format!("a turn of {which} is running; wait for it or cancel it"),
                );
            }
            *running = Some(id.to_string());
        }
        // No turn starts until the undo is done, also if it panics.
        struct Free(Arc<Mutex<Option<String>>>);
        impl Drop for Free {
            fn drop(&mut self) {
                *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
        }
        let _free = Free(Arc::clone(&self.running));
        let access = self.access.clone();
        let done = tokio::task::spawn_blocking(move || {
            let list = session.list().map_err(anyhow::Error::from)?;
            if !list.iter().any(|manifest| manifest.turn >= wanted.turn) {
                return Ok(None);
            }
            crate::checkpoints::cli::undo(&session, wanted.turn, wanted.force, &access).map(Some)
        })
        .await;
        match done {
            Ok(Ok(Some(undone))) => {
                self.log(&format!(
                    "chat {id} undo from turn {}{}: {} restored, {} skipped",
                    wanted.turn,
                    if wanted.force { " (forced)" } else { "" },
                    undone.restored.len(),
                    undone.skipped.len()
                ));
                Answer::json(
                    200,
                    &json!({
                        "restored": undone.restored,
                        "skipped": undone.skipped.iter().map(|(path, reason)| {
                            json!({ "path": path, "reason": reason })
                        }).collect::<Vec<_>>(),
                    }),
                )
            }
            Ok(Ok(None)) => Answer::error(
                404,
                "not_found",
                &format!("there is no checkpoint at turn {} or later", wanted.turn),
            ),
            Ok(Err(_)) | Err(_) => {
                Answer::error(500, "undo_failed", "cannot read the file checkpoints")
            }
        }
    }

    /// The stream of `GET /api/chat/<id>/events`, or why there is none.
    pub fn events(&self, id: &str, query: &str) -> Result<stream::Stream, Answer> {
        let Some(room) = self.room(id) else {
            return Err(no_chat(id));
        };
        let mut after = 0;
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if name == "after" {
                after = value
                    .parse::<u64>()
                    .ok()
                    .filter(|_| value.bytes().all(|byte| byte.is_ascii_digit()))
                    .ok_or_else(|| {
                        Answer::error(400, "bad_query", "after is a whole number, 0 or more")
                    })?;
            }
        }
        let permit = Arc::clone(&self.streams).try_acquire_owned().map_err(|_| {
            Answer::error(
                429,
                "too_many_streams",
                &format!("{MAX_STREAMS} event streams are open already"),
            )
        })?;
        Ok(stream::Stream {
            after: room.start_after(after),
            room,
            _permit: permit,
        })
    }
}

fn stopped() -> Answer {
    Answer::error(
        503,
        "chat_stopped",
        "the chat stopped; restart `junees dashboard --chat`",
    )
}
