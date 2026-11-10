//! What the bot does with Discord's events: which messages are for it,
//! the `!` commands, and a worker per channel that runs its turns in order
//! (while other channels run at the same time, up to a limit).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use super::Log;
use super::ask::{AnswerSlot, DiscordAsk, fence_safe};
use super::config::DiscordConfig;
use super::engine::{Conversation, Engines, Progress};
use super::rest::Rest;
use super::sessions::{self, SessionIndex};
use super::shard::Dispatch;
use super::split::{MESSAGE_LIMIT, split_message};
use crate::agent::Cancelled;
use crate::cancel::Cancel;
use crate::claude_code::UsageLimit;
use crate::memory::Memory;
use crate::prompt_text::escape_hidden;
use crate::provider::ProviderError;
use crate::skills;

/// Message types that carry what a user wrote: DEFAULT and REPLY.
const USER_MESSAGE_TYPES: [u64; 2] = [0, 19];

/// What the commands do, for `!help`.
pub const HELP: &str = "\
`!new` start a new conversation in this channel
`!stop` stop the turn that runs here (and drop the queued messages)
`!status` what runs here: session, queue, tokens
`!memory` what the persistent memory holds
`!skills` the skills Junees knows
`!help` this list
In a server channel, mention the bot (`@Junees !stop`) unless the channel is one of discord_channels.";

/// What the gateway shares with every channel's worker.
pub struct Shared {
    pub rest: Arc<Rest>,
    pub engines: Arc<dyn Engines>,
    pub config: DiscordConfig,
    pub home: Option<PathBuf>,
    pub cwd: PathBuf,
    pub memory: Option<Memory>,
    pub index: Mutex<SessionIndex>,
    /// One per turn that may run at the same time.
    pub permits: Arc<Semaphore>,
    /// How often the typing indicator is renewed during a turn (it lasts
    /// 10 seconds).
    pub typing_every: Duration,
    pub log: Log,
    /// Set when the gateway stops: cancelled turns say why.
    pub closing: AtomicBool,
}

impl Shared {
    /// Posts `text`, cut into messages; failures go to the log.
    pub async fn say(&self, channel: &str, text: &str) {
        for part in split_message(text, MESSAGE_LIMIT) {
            if let Err(err) = self.rest.create_message(channel, &part).await {
                (self.log)(&format!("cannot post to channel {channel}: {err:#}"));
                return;
            }
        }
    }
}

/// A channel's live state, read by the commands.
#[derive(Default)]
pub struct ChannelState {
    running: AtomicBool,
    /// A turn waits for a free slot (`discord_parallel`).
    waiting: AtomicBool,
    queued: AtomicUsize,
    /// Raised by `!stop`; queued jobs of an older epoch are dropped.
    epoch: AtomicU64,
    answers: Arc<AnswerSlot>,
    session: Mutex<String>,
    usage: Mutex<String>,
}

impl ChannelState {
    /// A turn runs or waits for a slot.
    fn busy(&self) -> bool {
        self.running.load(Ordering::SeqCst) || self.waiting.load(Ordering::SeqCst)
    }
}

fn locked<T: Clone>(mutex: &Mutex<T>) -> T {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn store<T>(mutex: &Mutex<T>, value: T) {
    *mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
}

enum Job {
    Input { text: String, epoch: u64 },
    New { epoch: u64 },
}

impl Job {
    fn epoch(&self) -> u64 {
        match self {
            Job::Input { epoch, .. } | Job::New { epoch } => *epoch,
        }
    }
}

struct Channel {
    jobs: UnboundedSender<Job>,
    state: Arc<ChannelState>,
    cancel: Cancel,
    worker: JoinHandle<()>,
}

/// A `!` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    New,
    Stop,
    Status,
    Memory,
    Skills,
    Help,
    Task,
    Unknown(String),
}

/// The command `text` is, if it starts with `!` and a word.
pub fn parse_command(text: &str) -> Option<Command> {
    let rest = text.trim().strip_prefix('!')?;
    let word = rest
        .split_whitespace()
        .next()
        .filter(|_| rest.starts_with(|c: char| !c.is_whitespace()))?;
    if !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(match word.to_ascii_lowercase().as_str() {
        "new" | "reset" => Command::New,
        "stop" => Command::Stop,
        "status" => Command::Status,
        "memory" => Command::Memory,
        "skills" => Command::Skills,
        "help" => Command::Help,
        "task" => Command::Task,
        _ => Command::Unknown(word.to_string()),
    })
}

/// `text` without mentions of the bot (`<@id>`, `<@!id>`).
pub fn strip_mention(text: &str, bot: &str) -> String {
    text.replace(&format!("<@{bot}>"), "")
        .replace(&format!("<@!{bot}>"), "")
        .trim()
        .to_string()
}

/// The bot: routes events to the channels.
pub struct Bot {
    shared: Arc<Shared>,
    channels: HashMap<String, Channel>,
    /// The bot's own user id, from Ready.
    me: Option<String>,
    /// Threads the bot answers in without a mention: its own, and those
    /// under a channel of `discord_channels`.
    threads: HashSet<String>,
    /// Users already reported as not allowed (reported once).
    strangers: HashSet<String>,
}

impl Bot {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            channels: HashMap::new(),
            me: None,
            threads: HashSet::new(),
            strangers: HashSet::new(),
        }
    }

    pub fn handle(&mut self, dispatch: Dispatch) {
        let data = &dispatch.data;
        match dispatch.name.as_str() {
            "READY" => {
                self.me = data
                    .pointer("/user/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "THREAD_CREATE" => self.thread(data),
            "GUILD_CREATE" => {
                for thread in data
                    .get("threads")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.thread(thread);
                }
            }
            "MESSAGE_CREATE" => self.message(data),
            _ => {}
        }
    }

    fn thread(&mut self, thread: &Value) {
        let id = thread.get("id").and_then(Value::as_str);
        let owner = thread.get("owner_id").and_then(Value::as_str);
        let parent = thread.get("parent_id").and_then(Value::as_str);
        let ours = owner.is_some() && owner == self.me.as_deref();
        let listed = parent.is_some_and(|parent| self.shared.config.channels.contains(parent));
        if let (Some(id), true) = (id, ours || listed) {
            self.threads.insert(id.to_string());
        }
    }

    fn message(&mut self, data: &Value) {
        let text = |pointer: &str| data.pointer(pointer).and_then(Value::as_str);
        if data.pointer("/author/bot").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let (Some(user), Some(channel)) = (text("/author/id"), text("/channel_id")) else {
            return;
        };
        let kind = data.get("type").and_then(Value::as_u64).unwrap_or(0);
        if !USER_MESSAGE_TYPES.contains(&kind) {
            return;
        }
        if !self.shared.config.allowed_users.contains(user) {
            if self.strangers.insert(user.to_string()) {
                (self.shared.log)(&format!(
                    "ignoring messages of user {user}: not in discord_allowed_users"
                ));
            }
            return;
        }
        let direct = data.get("guild_id").is_none_or(Value::is_null);
        let mentioned = self.me.as_deref().is_some_and(|me| {
            data.get("mentions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|user| user.get("id").and_then(Value::as_str) == Some(me))
        });
        let listed =
            self.shared.config.channels.contains(channel) || self.threads.contains(channel);
        let answering = self
            .channels
            .get(channel)
            .is_some_and(|known| known.state.answers.waiting());
        if !(direct || mentioned || listed || answering) {
            return;
        }
        let content = text("/content").unwrap_or_default();
        let content = match &self.me {
            Some(me) => strip_mention(content, me),
            None => content.trim().to_string(),
        };
        if content.is_empty() {
            return;
        }
        let channel = channel.to_string();
        if let Some(command) = parse_command(&content) {
            self.command(&channel, command);
            return;
        }
        if let Some(known) = self.channels.get(&channel)
            && known.state.answers.answer(content.clone())
        {
            return;
        }
        let known = self.channel(&channel);
        let epoch = known.state.epoch.load(Ordering::SeqCst);
        known.state.queued.fetch_add(1, Ordering::SeqCst);
        let _ = known.jobs.send(Job::Input {
            text: content,
            epoch,
        });
    }

    /// The channel's worker, started on first use.
    fn channel(&mut self, id: &str) -> &Channel {
        // A worker that could not start its conversation has ended; the
        // next message tries again.
        if self
            .channels
            .get(id)
            .is_some_and(|known| known.worker.is_finished())
        {
            self.channels.remove(id);
        }
        self.channels.entry(id.to_string()).or_insert_with(|| {
            let (jobs, receiver) = mpsc::unbounded_channel();
            let state = Arc::new(ChannelState::default());
            let cancel = Cancel::new();
            let worker = tokio::task::spawn_local(worker(
                Arc::clone(&self.shared),
                id.to_string(),
                Arc::clone(&state),
                cancel.clone(),
                receiver,
            ));
            Channel {
                jobs,
                state,
                cancel,
                worker,
            }
        })
    }

    fn command(&mut self, channel: &str, command: Command) {
        let reply = match command {
            Command::New => {
                let known = self.channel(channel);
                let epoch = known.state.epoch.load(Ordering::SeqCst);
                known.state.queued.fetch_add(1, Ordering::SeqCst);
                let _ = known.jobs.send(Job::New { epoch });
                return;
            }
            Command::Stop => match self.channels.get(channel) {
                Some(known)
                    if known.state.busy() || known.state.queued.load(Ordering::SeqCst) > 0 =>
                {
                    known.state.epoch.fetch_add(1, Ordering::SeqCst);
                    let dropped = known.state.queued.swap(0, Ordering::SeqCst);
                    let busy = known.state.busy();
                    known.cancel.cancel();
                    match (busy, dropped) {
                        // The worker says when the turn stopped.
                        (true, 0) => return,
                        (true, n) => format!("(stopping; dropped {n} queued)"),
                        (false, n) => format!("(dropped {n} queued)"),
                    }
                }
                _ => "(nothing is running here)".to_string(),
            },
            Command::Status => self.status(channel),
            Command::Memory => match &self.shared.memory {
                Some(memory) => memory.overview(),
                None => "memory is off: neither JUNEES_HOME nor HOME is set".to_string(),
            },
            Command::Skills => self.skills(),
            Command::Help => HELP.to_string(),
            Command::Task => {
                "`!task` is not available in the gateway yet; run `junees task \"<goal>\"` \
                              on the machine (task progress can go to a Discord webhook, \
                              notify_webhook)."
                    .to_string()
            }
            Command::Unknown(word) => format!(
                "unknown command `!{}`; `!help` lists them",
                fence_safe(&escape_hidden(&word))
            ),
        };
        let shared = Arc::clone(&self.shared);
        let channel = channel.to_string();
        tokio::spawn(async move { shared.say(&channel, &reply).await });
    }

    fn status(&self, channel: &str) -> String {
        let shared = &self.shared;
        let running = self
            .channels
            .values()
            .filter(|known| known.state.running.load(Ordering::SeqCst))
            .count();
        let mut lines = vec![format!(
            "Junees {} on the {}; turns running: {running} (at most {} at once)",
            env!("CARGO_PKG_VERSION"),
            shared.engines.describe(),
            shared.config.parallel
        )];
        lines.push(if shared.engines.asks() {
            format!(
                "risky tool calls are asked about here (approval {})",
                super::ask::human(shared.config.approval_timeout)
            )
        } else {
            "no approval questions on this backend or setting; only the command policy applies"
                .to_string()
        });
        match self.channels.get(channel) {
            Some(known) => {
                let state = &known.state;
                lines.push(format!(
                    "this channel: {}; {}, {} queued{}",
                    locked(&state.session),
                    if state.running.load(Ordering::SeqCst) {
                        "a turn is running"
                    } else if state.waiting.load(Ordering::SeqCst) {
                        "a turn waits for a free slot"
                    } else {
                        "idle"
                    },
                    state.queued.load(Ordering::SeqCst),
                    if state.answers.waiting() {
                        "; an approval question waits for its answer"
                    } else {
                        ""
                    }
                ));
                let usage = locked(&state.usage);
                if !usage.is_empty() {
                    lines.push(usage);
                }
            }
            None => lines.push("this channel: no conversation yet".to_string()),
        }
        lines.join("\n")
    }

    fn skills(&self) -> String {
        let Some(home) = &self.shared.home else {
            return "no skills: neither JUNEES_HOME nor HOME is set".to_string();
        };
        let index = skills::Index::load(&skills::dir(home));
        if index.skills.is_empty() {
            return "(no skills)".to_string();
        }
        let mut text = format!("{} skills:", index.skills.len());
        for entry in &index.skills {
            text.push_str(&format!(
                "\n- {}: {}",
                escape_hidden(&entry.name),
                escape_hidden(&entry.description)
            ));
        }
        text
    }

    /// Stops every channel: running turns are cancelled (and recorded as
    /// such), queued messages dropped, and the workers waited for.
    pub async fn shutdown(self, grace: Duration) {
        self.shared.closing.store(true, Ordering::SeqCst);
        let mut workers = Vec::new();
        for (_, known) in self.channels {
            known.state.epoch.fetch_add(1, Ordering::SeqCst);
            known.cancel.cancel();
            drop(known.jobs);
            workers.push(known.worker);
        }
        let all = async {
            for worker in workers {
                let _ = worker.await;
            }
        };
        if tokio::time::timeout(grace, all).await.is_err() {
            (self.shared.log)("some channels did not stop in time");
        }
    }
}

/// Runs a channel's jobs one after the other.
async fn worker(
    shared: Arc<Shared>,
    channel: String,
    state: Arc<ChannelState>,
    cancel: Cancel,
    mut jobs: UnboundedReceiver<Job>,
) {
    let (out, mut progress) = mpsc::unbounded_channel();
    let ask = DiscordAsk::new(
        out.clone(),
        Arc::clone(&state.answers),
        shared.config.approval_timeout,
    );
    let mut conversation = match shared.engines.open(Box::new(ask), cancel.clone()) {
        Ok(conversation) => conversation,
        Err(err) => {
            (shared.log)(&format!("channel {channel}: cannot start: {err:#}"));
            shared
                .say(
                    &channel,
                    &format!("error: cannot start a conversation: {err:#}"),
                )
                .await;
            return;
        }
    };
    // The channel's session is opened before the first input, and again
    // before each later one while it is unavailable for now.
    let mut ready = false;
    while let Some(job) = jobs.recv().await {
        if job.epoch() < state.epoch.load(Ordering::SeqCst) {
            continue;
        }
        let mut queued = state.queued.load(Ordering::SeqCst);
        while queued > 0 {
            match state.queued.compare_exchange(
                queued,
                queued - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(now) => queued = now,
            }
        }
        match job {
            Job::New { .. } => {
                let status = match shared.home.as_deref() {
                    Some(home) => {
                        let mut index = shared
                            .index
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        sessions::start(
                            &mut index,
                            home,
                            &shared.cwd,
                            shared.engines.as_ref(),
                            &channel,
                            conversation.as_mut(),
                        )
                    }
                    None => {
                        conversation.reset(None);
                        "session not saved".to_string()
                    }
                };
                ready = true;
                (shared.log)(&format!("channel {channel}: new conversation, {status}"));
                shared
                    .say(&channel, &format!("(new conversation) {status}"))
                    .await;
                store(&state.session, status);
                store(&state.usage, String::new());
            }
            Job::Input { text, .. } => {
                if !ready {
                    let opened = {
                        let mut index = shared
                            .index
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        sessions::open(
                            &mut index,
                            shared.home.as_deref(),
                            &shared.cwd,
                            shared.engines.as_ref(),
                            &channel,
                            conversation.as_mut(),
                        )
                    };
                    (shared.log)(&format!("channel {channel}: {}", opened.status));
                    store(&state.session, opened.status.clone());
                    ready = opened.ready;
                    if !ready {
                        shared
                            .say(
                                &channel,
                                &format!(
                                    "(not run) {}. It stays this channel's conversation: send \
                                     the message again once the other Junees has closed it, or \
                                     `!new` to start a new one here.",
                                    opened.status
                                ),
                            )
                            .await;
                        continue;
                    }
                }
                turn(
                    &shared,
                    &channel,
                    &state,
                    &cancel,
                    conversation.as_mut(),
                    &text,
                    &out,
                    &mut progress,
                )
                .await;
            }
        }
    }
}

/// Runs one input in the channel: waits for a free slot, shows typing,
/// posts the reply's text blocks as they finish and the tool calls as
/// short lines, and ends with the error if there is one.
#[allow(clippy::too_many_arguments)]
async fn turn(
    shared: &Shared,
    channel: &str,
    state: &ChannelState,
    cancel: &Cancel,
    conversation: &mut dyn Conversation,
    input: &str,
    out: &UnboundedSender<Progress>,
    progress: &mut UnboundedReceiver<Progress>,
) {
    cancel.reset();
    let permits = Arc::clone(&shared.permits);
    let permit = match permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            state.waiting.store(true, Ordering::SeqCst);
            shared
                .say(
                    channel,
                    &format!(
                        "(waiting for a free slot: at most {} turns run at once)",
                        shared.config.parallel
                    ),
                )
                .await;
            let permit = tokio::select! {
                permit = permits.acquire_owned() => permit.ok(),
                _ = cancel.cancelled() => None,
            };
            state.waiting.store(false, Ordering::SeqCst);
            match permit {
                Some(permit) => permit,
                None => {
                    shared.say(channel, "(stopped before it started)").await;
                    return;
                }
            }
        }
    };
    state.running.store(true, Ordering::SeqCst);
    let mut tools: Vec<String> = Vec::new();
    let result = {
        let run = conversation.turn(input, out);
        tokio::pin!(run);
        let mut typing = tokio::time::interval(shared.typing_every);
        // Posting can wait out a rate limit; no burst of typing after it.
        typing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                Some(item) = progress.recv() => show(shared, channel, &mut tools, item).await,
                result = &mut run => break result,
                _ = typing.tick() => {
                    if !state.answers.waiting()
                        && let Err(err) = shared.rest.typing(channel).await
                    {
                        (shared.log)(&format!("channel {channel}: typing: {err:#}"));
                    }
                }
            }
        }
    };
    while let Ok(item) = progress.try_recv() {
        show(shared, channel, &mut tools, item).await;
    }
    flush_tools(shared, channel, &mut tools).await;
    store(&state.usage, conversation.usage());
    state.running.store(false, Ordering::SeqCst);
    drop(permit);
    match result {
        Ok(()) => {}
        Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
            let why = if shared.closing.load(Ordering::SeqCst) {
                "(stopped: Junees is shutting down)"
            } else {
                "(stopped)"
            };
            shared.say(channel, why).await;
        }
        Err(err) => {
            (shared.log)(&format!("channel {channel}: turn failed: {err:#}"));
            shared.say(channel, &error_text(&err)).await;
        }
    }
}

/// An error of a turn, with the hint the terminal would give.
fn error_text(err: &anyhow::Error) -> String {
    let mut text = format!("error: {err:#}");
    if let Some(provider) = err.downcast_ref::<ProviderError>() {
        if provider.is_context_overflow() {
            text.push_str(
                "\n(the conversation no longer fits the context window; `!new` starts a new one)",
            );
        } else if provider.is_auth() {
            text.push_str("\n(check the API key on the machine running the gateway)");
        }
    }
    if err.downcast_ref::<UsageLimit>().is_some() {
        text.push_str(
            "\n(the subscription's usage limit stopped the turn; once it resets, the next \
             message continues the same conversation)",
        );
    }
    text
}

async fn show(shared: &Shared, channel: &str, tools: &mut Vec<String>, item: Progress) {
    match item {
        Progress::Tool(line) => tools.push(line),
        Progress::Text(text) | Progress::Note(text) | Progress::Question(text) => {
            flush_tools(shared, channel, tools).await;
            shared.say(channel, &text).await;
        }
    }
}

/// Posts the tool calls since the last text as one code block.
async fn flush_tools(shared: &Shared, channel: &str, tools: &mut Vec<String>) {
    if tools.is_empty() {
        return;
    }
    let lines = std::mem::take(tools).join("\n");
    shared
        .say(channel, &format!("```\n{}\n```", fence_safe(&lines)))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_commands() {
        assert_eq!(parse_command("!new"), Some(Command::New));
        assert_eq!(parse_command("  !STOP now"), Some(Command::Stop));
        assert_eq!(parse_command("!status"), Some(Command::Status));
        assert_eq!(parse_command("!memory"), Some(Command::Memory));
        assert_eq!(parse_command("!skills"), Some(Command::Skills));
        assert_eq!(parse_command("!task build it"), Some(Command::Task));
        assert_eq!(parse_command("!foo"), Some(Command::Unknown("foo".into())));
        for text in ["new", "!", "! new", "!!!", "!123", "hello !new"] {
            assert_eq!(parse_command(text), None, "{text}");
        }
    }

    #[test]
    fn strips_the_bot_mention() {
        assert_eq!(strip_mention("<@42> hi", "42"), "hi");
        assert_eq!(strip_mention("hi <@!42>", "42"), "hi");
        assert_eq!(strip_mention("<@43> hi", "42"), "<@43> hi");
    }
}
