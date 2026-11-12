//! The channel engine both gateways share: a worker per channel that runs
//! its turns in order (while other channels run at the same time, up to a
//! limit), the gateway commands, approval questions in the channel, and
//! the channel's saved session. The platform ([`Platform`]) posts, shows
//! typing and downloads images; the routers ([`super::bot`],
//! [`crate::telegram::bot`]) decide which messages are for the bot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use super::Log;
use super::ask::{AnswerSlot, DiscordAsk, fence_safe};
use super::engine::{Conversation, Engines, Progress};
use super::platform::Platform;
use super::sessions::{self, SessionIndex};
use super::split::{split_message, split_plain};
use crate::agent::Cancelled;
use crate::cancel::Cancel;
use crate::claude_code::UsageLimit;
use crate::images::discord as attached;
use crate::memory::Memory;
use crate::prompt_text::escape_hidden;
use crate::provider::ProviderError;
use crate::skills;

/// What the gateway shares with every channel's worker.
pub struct Shared<P: Platform> {
    pub platform: P,
    pub engines: Arc<dyn Engines>,
    /// How many channels may run a turn at the same time.
    pub parallel: usize,
    /// How long an approval question waits for its answer.
    pub approval_timeout: Duration,
    pub home: Option<PathBuf>,
    pub cwd: PathBuf,
    pub memory: Option<Memory>,
    pub index: Mutex<SessionIndex>,
    /// One per turn that may run at the same time.
    pub permits: Arc<Semaphore>,
    /// How often the typing indicator is renewed during a turn.
    pub typing_every: Duration,
    pub log: Log,
    /// Set when the gateway stops: cancelled turns say why.
    pub closing: AtomicBool,
}

impl<P: Platform> Shared<P> {
    /// Posts `text`, cut into messages; failures go to the log.
    pub async fn say(&self, channel: &str, text: &str) {
        let limit = self.platform.message_limit();
        let parts = if self.platform.code_blocks() {
            split_message(text, limit)
        } else {
            split_plain(text, limit)
        };
        for part in parts {
            if let Err(err) = self.platform.post(channel, &part).await {
                (self.log)(&format!(
                    "cannot post to {} {channel}: {err:#}",
                    self.platform.place()
                ));
                return;
            }
        }
    }
}

/// A channel's live state, read by the commands.
#[derive(Default)]
pub struct ChannelState {
    running: AtomicBool,
    /// A turn waits for a free slot.
    waiting: AtomicBool,
    /// A turn downloads the images attached to its message, before it
    /// waits for a slot.
    preparing: AtomicBool,
    queued: AtomicUsize,
    /// Raised by the stop command; queued jobs of an older epoch are
    /// dropped.
    epoch: AtomicU64,
    answers: Arc<AnswerSlot>,
    session: Mutex<String>,
    usage: Mutex<String>,
    /// When the channel last had something to do: a message queued for
    /// it or a job of its worker started or ended.
    touched: Mutex<Option<Instant>>,
}

impl ChannelState {
    /// A turn runs, waits for a slot or downloads its attachments.
    fn busy(&self) -> bool {
        self.running.load(Ordering::SeqCst)
            || self.waiting.load(Ordering::SeqCst)
            || self.preparing.load(Ordering::SeqCst)
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

enum Job<A> {
    Input {
        text: String,
        /// Images attached to the message, downloaded when its turn starts.
        attachments: Vec<A>,
        /// Attachments left out, and why.
        notes: Vec<String>,
        epoch: u64,
    },
    New {
        epoch: u64,
    },
}

impl<A> Job<A> {
    fn epoch(&self) -> u64 {
        match self {
            Job::Input { epoch, .. } | Job::New { epoch } => *epoch,
        }
    }
}

struct Channel<A> {
    jobs: UnboundedSender<Job<A>>,
    state: Arc<ChannelState>,
    cancel: Cancel,
    worker: JoinHandle<()>,
}

/// A gateway command (`!new` on Discord, `/new` on Telegram).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    New,
    Stop,
    Status,
    Memory,
    Skills,
    Cron,
    Webhooks,
    Help,
    Task,
    Unknown(String),
}

/// The channels of one gateway, each with its worker.
pub struct Channels<P: Platform> {
    shared: Arc<Shared<P>>,
    channels: HashMap<String, Channel<P::Attachment>>,
}

impl<P: Platform> Channels<P> {
    pub fn new(shared: Arc<Shared<P>>) -> Self {
        Self {
            shared,
            channels: HashMap::new(),
        }
    }

    pub fn shared(&self) -> &Arc<Shared<P>> {
        &self.shared
    }

    /// Whether an approval question in `channel` waits for its answer.
    pub fn answering(&self, channel: &str) -> bool {
        self.channels
            .get(channel)
            .is_some_and(|known| known.state.answers.waiting())
    }

    /// A message for the bot that is no command: the answer to a waiting
    /// approval question, or an input queued for the channel's worker.
    /// `notes` say which attachments were left out.
    pub fn input(
        &mut self,
        channel: &str,
        content: String,
        attachments: Vec<P::Attachment>,
        notes: Vec<String>,
    ) {
        if attachments.is_empty()
            && !content.is_empty()
            && let Some(known) = self.channels.get(channel)
            && known.state.answers.answer(content.clone())
        {
            return;
        }
        self.queue(channel, content, attachments, notes);
    }

    /// An input for the channel's worker that never answers a waiting
    /// approval question (someone else's words, such as a forwarded
    /// message): it runs after the turn, like any queued input.
    pub fn queue(
        &mut self,
        channel: &str,
        content: String,
        attachments: Vec<P::Attachment>,
        notes: Vec<String>,
    ) {
        if content.is_empty() && attachments.is_empty() {
            // Only files that are not images: nothing to run.
            let shared = Arc::clone(&self.shared);
            let text = notes_text(&notes);
            let channel = channel.to_string();
            tokio::spawn(async move { shared.say(&channel, &text).await });
            return;
        }
        let known = self.channel(channel);
        store(&known.state.touched, Some(Instant::now()));
        let epoch = known.state.epoch.load(Ordering::SeqCst);
        known.state.queued.fetch_add(1, Ordering::SeqCst);
        let _ = known.jobs.send(Job::Input {
            text: content,
            attachments,
            notes,
            epoch,
        });
    }

    /// The channel's worker, started on first use.
    fn channel(&mut self, id: &str) -> &Channel<P::Attachment> {
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

    /// Ends the workers of the channels that had nothing to do for
    /// `idle` (no turn running, waiting or asking, nothing queued), and
    /// with them their open conversations; returns how many. The
    /// channel's session stays saved: its next message starts a worker
    /// that opens it again. For gateways whose channels come and go
    /// (Slack threads), so their workers do not pile up.
    pub fn close_idle(&mut self, idle: Duration) -> usize {
        let before = self.channels.len();
        self.channels.retain(|_, known| {
            let state = &known.state;
            let quiet = !state.busy()
                && state.queued.load(Ordering::SeqCst) == 0
                && !state.answers.waiting()
                && locked(&state.touched).is_some_and(|at| at.elapsed() >= idle);
            // Dropping the channel drops its job sender: the worker's
            // next receive ends it.
            !quiet
        });
        before - self.channels.len()
    }

    /// How many channels have a worker.
    pub fn open_count(&self) -> usize {
        self.channels.len()
    }

    /// Runs a command; `direct` when it came in a private conversation
    /// with the bot.
    pub fn command(&mut self, channel: &str, command: Command, direct: bool) {
        let platform = &self.shared.platform;
        let reply = match command {
            Command::New => {
                let known = self.channel(channel);
                store(&known.state.touched, Some(Instant::now()));
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
                Some(memory) if direct => memory.overview(),
                Some(memory) => memory_counts(platform, memory),
                None => "memory is off: neither JUNEES_HOME nor HOME is set".to_string(),
            },
            Command::Skills => self.skills(),
            // Prompts and folders only in private, like the memory.
            Command::Cron => crate::cron::listing_from_env(self.shared.home.as_deref(), direct),
            Command::Webhooks => {
                crate::webhook::listing_from_env(self.shared.home.as_deref(), direct)
            }
            Command::Help => platform.help(),
            Command::Task => platform.task_note(),
            Command::Unknown(word) => format!(
                "unknown command {}; {} lists them",
                platform.command(&fence_safe(&escape_hidden(&word))),
                platform.command("help")
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
            shared.parallel
        )];
        lines.push(if shared.engines.asks() {
            format!(
                "risky tool calls are asked about here (approval {})",
                super::ask::human(shared.approval_timeout)
            )
        } else {
            "no approval questions on this backend or setting; only the command policy applies"
                .to_string()
        });
        let place = shared.platform.place();
        match self.channels.get(channel) {
            Some(known) => {
                let state = &known.state;
                lines.push(format!(
                    "this {place}: {}; {}, {} queued{}",
                    locked(&state.session),
                    if state.running.load(Ordering::SeqCst) {
                        "a turn is running"
                    } else if state.waiting.load(Ordering::SeqCst) {
                        "a turn waits for a free slot"
                    } else if state.preparing.load(Ordering::SeqCst) {
                        "a turn downloads its attachments"
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
            None => lines.push(format!("this {place}: no conversation yet")),
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

/// What the memory command shows where more people than the user read
/// the answer: how much each store holds, not what (the user store holds
/// facts about the user), and no paths.
fn memory_counts<P: Platform>(platform: &P, memory: &Memory) -> String {
    let mut lines: Vec<String> = memory
        .snapshot()
        .stores
        .iter()
        .map(|store| match &store.error {
            Some(_) => format!("{}: cannot read", store.target.name()),
            None => format!(
                "{}: {} entries, {}/{} characters",
                store.target.name(),
                store.entries,
                store.chars(),
                store.limit
            ),
        })
        .collect();
    lines.push(format!(
        "The contents show only in {}: send {} to the bot there.",
        platform.private_place(),
        platform.command("memory")
    ));
    lines.join("\n")
}

/// Runs a channel's jobs one after the other.
async fn worker<P: Platform>(
    shared: Arc<Shared<P>>,
    channel: String,
    state: Arc<ChannelState>,
    cancel: Cancel,
    mut jobs: UnboundedReceiver<Job<P::Attachment>>,
) {
    let (out, mut progress) = mpsc::unbounded_channel();
    let ask = DiscordAsk::new(
        out.clone(),
        Arc::clone(&state.answers),
        shared.approval_timeout,
    )
    .with_format(P::question);
    let mut conversation = match shared.engines.open(Box::new(ask), cancel.clone()) {
        Ok(conversation) => conversation,
        Err(err) => {
            let place = shared.platform.place();
            (shared.log)(&format!("{place} {channel}: cannot start: {err:#}"));
            shared
                .say(
                    &channel,
                    &format!("error: cannot start a conversation: {err:#}"),
                )
                .await;
            return;
        }
    };
    let place = shared.platform.place();
    // The channel's session is opened before the first input, and again
    // before each later one while it is unavailable for now.
    let mut ready = false;
    while let Some(job) = jobs.recv().await {
        store(&state.touched, Some(Instant::now()));
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
                (shared.log)(&format!("{place} {channel}: new conversation, {status}"));
                shared
                    .say(&channel, &format!("(new conversation) {status}"))
                    .await;
                store(&state.session, status);
                store(&state.usage, String::new());
            }
            Job::Input {
                text,
                attachments,
                notes,
                ..
            } => {
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
                    (shared.log)(&format!("{place} {channel}: {}", opened.status));
                    store(&state.session, opened.status.clone());
                    ready = opened.ready;
                    if !ready {
                        shared
                            .say(
                                &channel,
                                &format!(
                                    "(not run) {}. It stays this {place}'s conversation: send \
                                     the message again once the other Junees has closed it, or \
                                     {} to start a new one here.",
                                    opened.status,
                                    shared.platform.command("new")
                                ),
                            )
                            .await;
                        continue;
                    }
                }
                let input = Input {
                    text,
                    attachments,
                    notes,
                };
                turn(
                    &shared,
                    &channel,
                    &state,
                    &cancel,
                    conversation.as_mut(),
                    input,
                    &out,
                    &mut progress,
                )
                .await;
            }
        }
        store(&state.touched, Some(Instant::now()));
    }
}

/// A message for the conversation: its text and attachments.
struct Input<A> {
    text: String,
    attachments: Vec<A>,
    notes: Vec<String>,
}

/// The attachments left out of a message, as one post.
fn notes_text(notes: &[String]) -> String {
    format!("(attachments left out: {})", notes.join("; "))
}

/// Downloads the images of `input`, says which attachments were left out,
/// and returns the text and images to run; `None` when nothing is left
/// to run or a stop came meanwhile.
async fn prepare<P: Platform>(
    shared: &Shared<P>,
    channel: &str,
    state: &ChannelState,
    cancel: &Cancel,
    input: Input<P::Attachment>,
) -> Option<(String, Vec<crate::images::Image>)> {
    let Input {
        text,
        attachments,
        mut notes,
    } = input;
    let images = if attachments.is_empty() {
        Vec::new()
    } else {
        // Marked busy meanwhile, so the stop command cancels the download
        // (it can take 30 seconds per attachment) and status tells.
        state.preparing.store(true, Ordering::SeqCst);
        let got = tokio::select! {
            got = shared.platform.fetch_all(&attachments) => Some(got),
            _ = cancel.cancelled() => None,
        };
        state.preparing.store(false, Ordering::SeqCst);
        let Some((images, failed)) = got else {
            shared.say(channel, "(stopped before it started)").await;
            return None;
        };
        notes.extend(failed);
        // The kind and size only: the log never holds what users sent.
        let place = shared.platform.place();
        for image in &images {
            (shared.log)(&format!(
                "{place} {channel}: attached {}",
                attached::log_line(image)
            ));
        }
        images
    };
    if !notes.is_empty() {
        shared.say(channel, &notes_text(&notes)).await;
    }
    if text.is_empty() && images.is_empty() {
        return None;
    }
    let mut text = if text.is_empty() {
        crate::images::NO_TEXT.to_string()
    } else {
        text
    };
    // The model should not answer about a picture it never got.
    if !notes.is_empty() {
        text.push_str(&format!("\n\n{}", notes_text(&notes)));
    }
    Some((text, images))
}

/// Runs one input in the channel: waits for a free slot, shows typing,
/// posts the reply's text blocks as they finish and the tool calls as
/// short lines, and ends with the error if there is one.
#[allow(clippy::too_many_arguments)]
async fn turn<P: Platform>(
    shared: &Shared<P>,
    channel: &str,
    state: &ChannelState,
    cancel: &Cancel,
    conversation: &mut dyn Conversation,
    input: Input<P::Attachment>,
    out: &UnboundedSender<Progress>,
    progress: &mut UnboundedReceiver<Progress>,
) {
    cancel.reset();
    let Some((input, images)) = prepare(shared, channel, state, cancel, input).await else {
        return;
    };
    let place = shared.platform.place();
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
                        shared.parallel
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
        let run = conversation.turn_with_images(&input, images, out);
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
                        && let Err(err) = shared.platform.typing(channel).await
                    {
                        (shared.log)(&format!("{place} {channel}: typing: {err:#}"));
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
            (shared.log)(&format!("{place} {channel}: turn failed: {err:#}"));
            let new = shared.platform.command("new");
            shared.say(channel, &error_text(&err, &new)).await;
        }
    }
}

/// An error of a turn, with the hint the terminal would give; `new` is
/// how the new-conversation command is written.
fn error_text(err: &anyhow::Error, new: &str) -> String {
    let mut text = format!("error: {err:#}");
    if let Some(provider) = err.downcast_ref::<ProviderError>() {
        if provider.is_context_overflow() {
            text.push_str(&format!(
                "\n(the conversation no longer fits the context window; {new} starts a new one)"
            ));
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

async fn show<P: Platform>(
    shared: &Shared<P>,
    channel: &str,
    tools: &mut Vec<String>,
    item: Progress,
) {
    match item {
        Progress::Tool(line) => tools.push(line),
        Progress::Text(text) | Progress::Note(text) | Progress::Question(text) => {
            flush_tools(shared, channel, tools).await;
            shared.say(channel, &text).await;
        }
    }
}

/// Posts the tool calls since the last text as one message.
async fn flush_tools<P: Platform>(shared: &Shared<P>, channel: &str, tools: &mut Vec<String>) {
    if tools.is_empty() {
        return;
    }
    let lines = std::mem::take(tools).join("\n");
    shared
        .say(channel, &shared.platform.tool_lines(&lines))
        .await;
}
