//! Which Slack events are for the bot, and the `!` commands. The
//! conversations themselves (a worker each, the queue, sessions,
//! approvals) are the shared engine of [`crate::gateway::channels`].
//!
//! - A DM (`message` with `channel_type: im`) is one conversation per DM
//!   channel; every message there is for the bot.
//! - In a channel the bot answers a mention (`app_mention`, or a
//!   `message` whose text names it) and, after that, every message in the
//!   same thread (one conversation per thread, keyed by `thread_ts`; the
//!   bot replies in the thread). Thread messages without a mention reach
//!   the bot only when the app subscribes to `message.channels` /
//!   `message.groups`; without them, mention the bot each time.
//! - Messages with a `subtype` (edits, deletions, joins, other bots) are
//!   not the user's words and are skipped, except `file_share` (a message
//!   with files) and `thread_broadcast` (a thread reply also sent to the
//!   channel). The bot's own messages are skipped.
//! - Slack sends an event again when it did not see the acknowledgement
//!   in time, and a mention in a channel the app reads arrives twice
//!   (`message` and `app_mention`): each `event_id` and each message
//!   (channel and `ts`) is handled once.
//!
//! Presses on the buttons of approval questions arrive as `block_actions`
//! payloads ("block_actions payload": `user`, `channel`, `container`,
//! `message`, `actions[].action_id`/`value`). A press answers only when
//! the user is in `slack_allowed_users` and the button's value names the
//! question that waits in the conversation of the pressed message (its
//! channel, and its `thread_ts` in a channel) and the press comes from
//! that question's message (`ts`); see [`Channels::press`]. Every other
//! press changes nothing and its user gets a note only they see
//! (`chat.postEphemeral`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::api::Me;
use super::format::{escape, from_slack};
use super::platform::{Slack, Target, attachments};
use crate::gateway::ask::{Press, parse_button};
use crate::gateway::bot::parse_command;
use crate::gateway::channels::{Channels, Shared};
use crate::prompt_text::escape_hidden;

/// Event ids and messages remembered against repeats.
const RECENT: usize = 4096;

/// Channel threads remembered as the bot's (the most recently used).
const THREADS: usize = 4096;

/// Subtypes that still carry what a user wrote.
const USER_SUBTYPES: [&str; 2] = ["file_share", "thread_broadcast"];

/// What a press that answered nothing gets back.
pub const ALREADY_HANDLED: &str =
    "이미 답했거나 시간이 지났거나 멈춘 질문이라 이 버튼은 이제 아무것도 하지 않습니다.";
/// What a press of a user not in `slack_allowed_users` gets back.
pub const NOT_ALLOWED: &str = "slack_allowed_users에 있는 사용자만 Junees에 답할 수 있습니다.";
/// What a press of a button that is not Junees's gets back.
const NOT_OURS: &str = "Junees does not know this button.";

/// `text` without mentions of the bot (`<@U123>`, `<@U123|name>`),
/// trimmed.
pub fn strip_mention(text: &str, bot: &str) -> String {
    let mention = format!("<@{bot}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&mention) {
        let after = &rest[at + mention.len()..];
        let end = match after.chars().next() {
            Some('>') => Some(1),
            Some('|') => after.find('>').map(|close| close + 1),
            _ => None,
        };
        match end {
            Some(end) => {
                out.push_str(&rest[..at]);
                rest = &after[end..];
            }
            None => {
                out.push_str(&rest[..at + mention.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Whether `text` mentions the bot.
fn mentions(text: &str, bot: &str) -> bool {
    text.contains(&format!("<@{bot}>")) || text.contains(&format!("<@{bot}|"))
}

/// Remembers `id` in `recent` (at most [`RECENT`]); false when it was
/// there already.
fn remember(recent: &mut VecDeque<String>, id: String) -> bool {
    if recent.contains(&id) {
        return false;
    }
    if recent.len() == RECENT {
        recent.pop_front();
    }
    recent.push_back(id);
    true
}

/// The router: hands the bot's messages to the conversations.
pub struct Router {
    channels: Channels<Slack>,
    allowed: HashSet<String>,
    me: Me,
    /// Users already reported as not allowed (reported once).
    strangers: HashSet<String>,
    events: VecDeque<String>,
    messages: VecDeque<String>,
    /// Channel threads the bot was mentioned in: their replies need no
    /// mention. Threads with a saved session count too (after a
    /// restart, or once a thread left this list).
    threads: Threads,
}

/// The [`THREADS`] most recently used thread keys.
#[derive(Default)]
struct Threads {
    used: HashMap<String, u64>,
    clock: u64,
}

impl Threads {
    fn contains(&self, key: &str) -> bool {
        self.used.contains_key(key)
    }

    /// Remembers `key` as used now, forgetting the least recently used
    /// thread past the limit.
    fn insert(&mut self, key: String) {
        self.clock += 1;
        self.used.insert(key, self.clock);
        if self.used.len() > THREADS
            && let Some(oldest) = self
                .used
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| key.clone())
        {
            self.used.remove(&oldest);
        }
    }
}

impl Router {
    pub fn new(shared: Arc<Shared<Slack>>, allowed: HashSet<String>, me: Me) -> Self {
        Self {
            channels: Channels::new(shared),
            allowed,
            me,
            strangers: HashSet::new(),
            events: VecDeque::new(),
            messages: VecDeque::new(),
            threads: Threads::default(),
        }
    }

    /// Closes the conversations idle for `idle` (see
    /// [`Channels::close_idle`]).
    pub fn close_idle(&mut self, idle: Duration) {
        let closed = self.channels.close_idle(idle);
        if closed > 0 {
            (self.channels.shared().log)(&format!(
                "slack: closed {closed} idle conversation(s), {} open; a new message opens \
                 its saved session again",
                self.channels.open_count()
            ));
        }
    }

    /// Handles the payload of an `events_api` or `interactive` envelope.
    pub fn handle(&mut self, payload: &Value) {
        match payload.get("type").and_then(Value::as_str) {
            Some("event_callback") => {}
            Some("block_actions") => return self.press(payload),
            _ => return,
        }
        if let Some(id) = payload.get("event_id").and_then(Value::as_str)
            && !remember(&mut self.events, id.to_string())
        {
            return;
        }
        let Some(event) = payload.get("event") else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("message") => self.message(event, false),
            Some("app_mention") => self.message(event, true),
            _ => {}
        }
    }

    fn message(&mut self, event: &Value, mention_event: bool) {
        let text_of = |key: &str| event.get(key).and_then(Value::as_str);
        if let Some(subtype) = text_of("subtype")
            && !USER_SUBTYPES.contains(&subtype)
        {
            return;
        }
        if event.get("bot_id").is_some_and(|id| !id.is_null()) {
            return;
        }
        let (Some(user), Some(channel), Some(ts)) =
            (text_of("user"), text_of("channel"), text_of("ts"))
        else {
            return;
        };
        if user == self.me.user_id {
            return;
        }
        if !remember(&mut self.messages, format!("{channel}/{ts}")) {
            return;
        }
        if !self.allowed.contains(user) {
            if self.strangers.insert(user.to_string()) {
                (self.channels.shared().log)(&format!(
                    "ignoring messages of user {user}: not in slack_allowed_users"
                ));
            }
            return;
        }
        let direct = match text_of("channel_type") {
            Some(kind) => kind == "im",
            None => channel.starts_with('D'),
        };
        let text = text_of("text").unwrap_or_default();
        let key = if direct {
            channel.to_string()
        } else {
            Target {
                channel: channel.to_string(),
                thread_ts: Some(text_of("thread_ts").unwrap_or(ts).to_string()),
            }
            .key()
        };
        if !direct {
            let mentioned = mention_event || mentions(text, &self.me.user_id);
            let in_thread = text_of("thread_ts").is_some();
            let ours = in_thread && (self.threads.contains(&key) || self.saved(&key));
            if !(mentioned || ours || self.channels.answering(&key)) {
                return;
            }
            self.threads.insert(key.clone());
        }
        let content = from_slack(&strip_mention(text, &self.me.user_id))
            .trim()
            .to_string();
        let (files, notes) = attachments(event);
        // Text under files is about them: never a command.
        if files.is_empty()
            && notes.is_empty()
            && let Some(command) = parse_command(&content)
        {
            self.channels.command(&key, command, direct);
            return;
        }
        if content.is_empty() && files.is_empty() && notes.is_empty() {
            return;
        }
        self.channels.input(&key, user, content, files, notes);
    }

    /// A press on a button (module docs). The envelope was acknowledged
    /// already; a press that answers nothing gets a note in the
    /// background.
    fn press(&mut self, payload: &Value) {
        let text = |pointer: &str| payload.pointer(pointer).and_then(Value::as_str);
        let (Some(user), Some(channel)) = (
            text("/user/id"),
            text("/channel/id").or_else(|| text("/container/channel_id")),
        ) else {
            return;
        };
        let message = text("/message/ts").or_else(|| text("/container/message_ts"));
        let thread = text("/message/thread_ts").or_else(|| text("/container/thread_ts"));
        let refusal = if !self.allowed.contains(user) {
            if self.strangers.insert(user.to_string()) {
                (self.channels.shared().log)(&format!(
                    "ignoring button presses of user {}: not in slack_allowed_users",
                    escape_hidden(user)
                ));
            }
            Some(NOT_ALLOWED)
        } else {
            let button = payload
                .pointer("/actions/0/value")
                .and_then(Value::as_str)
                .and_then(parse_button);
            match (button, message) {
                (Some((id, choice)), Some(message)) => {
                    let key = Target {
                        channel: channel.to_string(),
                        thread_ts: thread.map(str::to_string),
                    }
                    .key();
                    let name = text("/user/username")
                        .or_else(|| text("/user/name"))
                        .unwrap_or(user);
                    let press = Press {
                        id,
                        choice,
                        message: Some(message.to_string()),
                        user: user.to_string(),
                        by: escape_hidden(name),
                    };
                    if self.channels.press(&key, press) {
                        None
                    } else {
                        Some(ALREADY_HANDLED)
                    }
                }
                _ => Some(NOT_OURS),
            }
        };
        let Some(note) = refusal else {
            return;
        };
        let shared = self.channels.shared();
        let api = Arc::clone(&shared.platform.api);
        let log = Arc::clone(&shared.log);
        let (channel, user, thread) = (
            channel.to_string(),
            user.to_string(),
            thread.map(str::to_string),
        );
        tokio::spawn(async move {
            if let Err(err) = api
                .post_ephemeral(&channel, &user, thread.as_deref(), &escape(note))
                .await
            {
                log(&format!("cannot answer a button press: {err}"));
            }
        });
    }

    /// Whether `key` has a saved session.
    fn saved(&self, key: &str) -> bool {
        self.channels
            .shared()
            .index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .is_some()
    }

    /// Stops every conversation: running turns are cancelled (and
    /// recorded as such), queued messages dropped, and the workers waited
    /// for.
    pub async fn shutdown(self, grace: Duration) {
        self.channels.shutdown(grace).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_the_bots_mention() {
        assert_eq!(strip_mention("<@UBOT> hi", "UBOT"), "hi");
        assert_eq!(
            strip_mention("hi <@UBOT|junees> there", "UBOT"),
            "hi  there"
        );
        assert_eq!(strip_mention("<@UBOT2> hi", "UBOT"), "<@UBOT2> hi");
        assert_eq!(strip_mention("<@UBOT", "UBOT"), "<@UBOT");
        assert_eq!(strip_mention("한글 <@UBOT> !stop", "UBOT"), "한글  !stop");
        assert!(mentions("x <@UBOT|j> y", "UBOT"));
        assert!(!mentions("x <@UBOT2> y", "UBOT"));
    }

    #[test]
    fn keeps_the_most_recently_used_threads() {
        let mut threads = Threads::default();
        for n in 0..THREADS {
            threads.insert(format!("C1:{n}"));
        }
        // Used again: now the newest.
        threads.insert("C1:0".into());
        threads.insert("C1:new".into());
        assert_eq!(threads.used.len(), THREADS);
        assert!(threads.contains("C1:0") && threads.contains("C1:new"));
        assert!(!threads.contains("C1:1"));
        assert!(threads.contains("C1:2"));
    }

    #[test]
    fn remembers_a_bounded_number_of_ids() {
        let mut recent = VecDeque::new();
        assert!(remember(&mut recent, "a".into()));
        assert!(!remember(&mut recent, "a".into()));
        for n in 0..RECENT {
            remember(&mut recent, n.to_string());
        }
        assert_eq!(recent.len(), RECENT);
        assert!(remember(&mut recent, "a".into()));
    }
}
