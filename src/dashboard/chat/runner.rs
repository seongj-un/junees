//! The chat's agents, on a thread of their own: [`crate::agent::Agent`]'s
//! futures are not `Send`. Commands come over a channel; a turn runs as a
//! local task, so chats can be opened while it runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;

use super::Room;
use crate::agent::{Agent, Cancelled, Event, TurnLimit};
use crate::approval::{ASKED_EVERY_TIME, Answer, Ask, Request};
use crate::config_file::Settings;
use crate::dashboard::session_view::{PREVIEW_CHARS, cut, input_text};
use crate::gateway::engine::ApiEngines;
use crate::session::{Session, SessionWriter};

/// Added to the system prompt of every chat.
pub const DASHBOARD_NOTE: &str = "\n\nYou are talking with the user through the Junees \
    dashboard in their browser, not a terminal: replies are shown as plain text as they \
    arrive, and approval questions appear as buttons on the page.";

/// Text held before it is sent: up to this many bytes, or a line end, then
/// up to the last space, so a secret is never split between two events.
const TEXT_CHUNK: usize = 64;
/// Text sent even without a space once this long.
const TEXT_MOST: usize = 8 * 1024;
/// Characters of a tool's name in an event.
const NAME_CHARS: usize = 200;

/// What the thread is asked to do.
pub enum Command {
    /// A new chat (`resume: None`) or the session at `resume` continued.
    Open {
        resume: Option<PathBuf>,
        reply: tokio::sync::oneshot::Sender<Result<Arc<Room>, OpenError>>,
    },
    /// One turn of `room`.
    Turn {
        room: Arc<Room>,
        text: String,
        turn: u64,
    },
    /// Drops chat `id`'s agent, closing its journal.
    Close { id: String },
}

/// Why a chat did not open.
#[derive(Debug)]
pub enum OpenError {
    /// The session is written by another Junees.
    Busy(String),
    Failed(String),
}

/// What the thread shares with the server.
pub struct Shared {
    pub dir: PathBuf,
    pub home: Option<PathBuf>,
    pub model: String,
    pub settings: Arc<Settings>,
    /// How long a question waits ([`super::APPROVAL_TIME`]).
    pub approval_time: std::time::Duration,
    /// Cleared when a turn ends, before its `turn_end`.
    pub running: Arc<Mutex<Option<String>>>,
}

/// Starts the thread; it ends when the sender is dropped.
pub fn start(engines: ApiEngines, shared: Shared) -> mpsc::UnboundedSender<Command> {
    let (commands, mut received) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("dashboard-chat".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("dashboard: the chat cannot start: {err}");
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            let shared = Rc::new(shared);
            let agents: Rc<RefCell<HashMap<String, Agent>>> = Rc::default();
            local.block_on(&runtime, async move {
                while let Some(command) = received.recv().await {
                    match command {
                        Command::Open { resume, reply } => {
                            let opened = open(&engines, &shared, resume);
                            let _ = reply.send(opened.map(|(room, agent)| {
                                agents.borrow_mut().insert(room.id.clone(), agent);
                                room
                            }));
                        }
                        Command::Close { id } => {
                            agents.borrow_mut().remove(&id);
                        }
                        Command::Turn { room, text, turn } => {
                            let agent = agents.borrow_mut().remove(&room.id);
                            let (agents, shared) = (Rc::clone(&agents), Rc::clone(&shared));
                            tokio::task::spawn_local(async move {
                                let Some(mut agent) = agent else {
                                    end_turn(
                                        &shared,
                                        &room,
                                        turn,
                                        "error",
                                        Some("the chat is closed"),
                                    );
                                    return;
                                };
                                run_turn(&shared, &room, &mut agent, &text, turn).await;
                                agents.borrow_mut().insert(room.id.clone(), agent);
                            });
                        }
                    }
                }
            });
        })
        .expect("the chat thread starts");
    commands
}

/// A chat's room and agent: a new session, or the one at `resume`.
fn open(
    engines: &ApiEngines,
    shared: &Shared,
    resume: Option<PathBuf>,
) -> Result<(Arc<Room>, Agent), OpenError> {
    let failed = |err: anyhow::Error| OpenError::Failed(hidden(shared, format!("{err:#}")));
    match resume {
        Some(path) => {
            let (session, writer) = Session::resume(&path)
                .map_err(|err| OpenError::Busy(hidden(shared, format!("{err:#}"))))?;
            let room = Arc::new(Room::new(session.meta.id.clone()));
            let mut agent = engines
                .agent(Box::new(ChatAsk::new(&room, shared)), room.cancel.clone())
                .map_err(failed)?;
            agent.resume(session.transcript, Some(Box::new(writer)));
            Ok((room, agent))
        }
        None => {
            let (id, writer) = match &shared.home {
                Some(home) => {
                    let (meta, writer) =
                        SessionWriter::create(home, &shared.dir, &shared.model).map_err(failed)?;
                    (meta.id, Some(writer))
                }
                None => (
                    crate::mcp::oauth::pkce::random_text(9)
                        .map_err(|err| failed(anyhow::anyhow!(err)))?,
                    None,
                ),
            };
            let room = Arc::new(Room::new(id));
            let mut agent = engines
                .agent(Box::new(ChatAsk::new(&room, shared)), room.cancel.clone())
                .map_err(failed)?;
            if let Some(writer) = writer {
                agent = agent.with_journal(Box::new(writer));
            }
            Ok((room, agent))
        }
    }
}

fn hidden(shared: &Shared, mut text: String) -> String {
    crate::doctor::hide_secrets_in_user_text(&shared.settings, std::iter::once(&mut text));
    text
}

fn preview(shared: &Shared, text: &str, chars: usize) -> String {
    cut(&shared.settings, text, chars).0
}

/// Runs one turn of `room`, as events.
async fn run_turn(shared: &Shared, room: &Room, agent: &mut Agent, text: &str, turn: u64) {
    room.cancel.reset();
    room.push(json!({ "type": "turn_start", "turn": turn }));
    let before = agent.usage();
    let mut out = TextOut::default();
    let result = agent
        .run(text, |event| on_event(shared, room, &mut out, event))
        .await;
    out.flush(shared, room);
    let usage = agent.usage().since(&before);
    let cost = crate::usage::prices::Prices::from_setting(
        shared.settings.get(crate::usage::PRICES_ENV).as_deref(),
    )
    .ok()
    .and_then(|prices| prices.get(&shared.model))
    .map(|price| crate::usage::prices::cost(&price, &usage, false) as f64 / 1e9);
    room.push(json!({
        "type": "usage",
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cost_usd": cost,
    }));
    match result {
        Ok(_) => end_turn(shared, room, turn, "end_turn", None),
        Err(err) if err.downcast_ref::<TurnLimit>().is_some() => {
            end_turn(shared, room, turn, "max_turns", None)
        }
        Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
            end_turn(shared, room, turn, "cancelled", None)
        }
        Err(err) => end_turn(shared, room, turn, "error", Some(&format!("{err:#}"))),
    }
}

/// Frees the chat for the next message, then says the turn ended.
fn end_turn(shared: &Shared, room: &Room, turn: u64, stop_reason: &str, error: Option<&str>) {
    if let Some(message) = error {
        room.push(json!({ "type": "error", "message": preview(shared, message, PREVIEW_CHARS) }));
    }
    *shared
        .running
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    room.push(json!({ "type": "turn_end", "turn": turn, "stop_reason": stop_reason }));
}

fn on_event(shared: &Shared, room: &Room, out: &mut TextOut, event: Event<'_>) {
    match event {
        Event::TextDelta(delta) => out.push(shared, room, delta),
        Event::TextEnd => out.flush(shared, room),
        Event::Thinking => {}
        // What was shown of the reply is void; the page is told in words.
        Event::Retrying => {
            out.clear();
            out.note(shared, room, "(the reply broke off; asking for it again)");
        }
        Event::Warning(warning) => out.note(shared, room, &format!("({warning})")),
        Event::Compacted {
            before_messages,
            after_messages,
        } => out.note(
            shared,
            room,
            &format!(
                "(compacted the conversation: {before_messages} messages -> {after_messages})"
            ),
        ),
        Event::Sources(urls) => {
            out.flush(shared, room);
            out.note(shared, room, &crate::web_search::sources_text(urls));
        }
        Event::ToolCall { name, input } | Event::ServerToolUse { name, input } => {
            out.flush(shared, room);
            room.push(json!({
                "type": "tool_use",
                "name": preview(shared, name, NAME_CHARS),
                "input_preview": preview(shared, &input_text(input), PREVIEW_CHARS),
            }));
        }
        Event::Subagent { index, line } => {
            out.flush(shared, room);
            room.push(json!({
                "type": "tool_use",
                "name": format!("subagent {index}"),
                "input_preview": preview(shared, line, PREVIEW_CHARS),
            }));
        }
        Event::ToolResult { name, output } => {
            room.push(json!({
                "type": "tool_result",
                "name": preview(shared, name, NAME_CHARS),
                "is_error": output.is_error,
                "text_preview": preview(shared, &output.content, PREVIEW_CHARS),
            }));
        }
    }
}

/// The reply's text on its way to `text` events: held until a space so
/// that hiding secrets sees each word whole.
#[derive(Default)]
pub(super) struct TextOut {
    held: String,
}

impl TextOut {
    pub(super) fn push(&mut self, shared: &Shared, room: &Room, delta: &str) {
        self.held.push_str(delta);
        if self.held.len() < TEXT_CHUNK && !delta.contains('\n') {
            return;
        }
        let cut_at = self
            .held
            .char_indices()
            .rfind(|(_, c)| c.is_whitespace())
            .map(|(at, c)| at + c.len_utf8());
        match cut_at {
            Some(at) => {
                let rest = self.held.split_off(at);
                let ready = std::mem::replace(&mut self.held, rest);
                send_text(shared, room, ready);
            }
            None if self.held.len() >= TEXT_MOST => self.flush(shared, room),
            None => {}
        }
    }

    pub(super) fn flush(&mut self, shared: &Shared, room: &Room) {
        let ready = std::mem::take(&mut self.held);
        send_text(shared, room, ready);
    }

    fn clear(&mut self) {
        self.held.clear();
    }

    /// A line of Junees's own between the reply's text.
    fn note(&mut self, shared: &Shared, room: &Room, text: &str) {
        self.flush(shared, room);
        send_text(shared, room, format!("\n{text}\n"));
    }
}

fn send_text(shared: &Shared, room: &Room, text: String) {
    if text.is_empty() {
        return;
    }
    let text = hidden(shared, text);
    room.push(json!({ "type": "text", "delta": text }));
}

/// Asks the page: an `approval` event, then the answer the page posts,
/// or a decline after [`super::APPROVAL_TIME`] or a cancel.
pub(super) struct ChatAsk {
    room: Arc<Room>,
    settings: Arc<Settings>,
    time: std::time::Duration,
}

impl ChatAsk {
    pub(super) fn new(room: &Arc<Room>, shared: &Shared) -> Self {
        Self {
            room: Arc::clone(room),
            settings: Arc::clone(&shared.settings),
            time: shared.approval_time,
        }
    }
}

/// Closes a question that is still open when its asker goes away (the
/// turn was dropped).
struct Closer<'a> {
    room: &'a Room,
    id: &'a str,
}

impl Drop for Closer<'_> {
    fn drop(&mut self) {
        let _ = self.room.close(self.id, "n", "cancel");
    }
}

#[async_trait]
impl Ask for ChatAsk {
    async fn ask(&mut self, request: &Request) -> Answer {
        let room = Arc::clone(&self.room);
        let id = match crate::mcp::oauth::pkce::random_text(12) {
            Ok(id) => id,
            Err(_) => return Answer::NoBecause("the question could not be asked".into()),
        };
        let always = request
            .kinds
            .iter()
            .any(|kind| !kind.ends_with(ASKED_EVERY_TIME));
        let answer = room.ask(&id, always);
        let summary = cut(
            &self.settings,
            &crate::approval::render(request),
            PREVIEW_CHARS,
        )
        .0;
        let options: Vec<&str> = if always {
            vec!["y", "a", "n"]
        } else {
            vec!["y", "n"]
        };
        room.push(json!({
            "type": "approval",
            "question_id": id,
            "kind": request.tool,
            "summary": summary,
            "options": options,
        }));
        let _closer = Closer {
            room: &room,
            id: &id,
        };
        tokio::select! {
            answer = answer => answer.unwrap_or(Answer::No),
            () = tokio::time::sleep(self.time) => {
                let _ = room.close(&id, "n", "timeout");
                Answer::NoBecause(format!(
                    "no answer on the dashboard within {} seconds",
                    self.time.as_secs()
                ))
            }
            () = room.cancel.cancelled() => {
                let _ = room.close(&id, "n", "cancel");
                Answer::NoBecause("the turn was cancelled".into())
            }
        }
    }
}
