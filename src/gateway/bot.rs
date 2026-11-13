//! What the bot does with Discord's events: which messages are for it,
//! the `!` commands, and presses on the buttons of approval questions.
//! The channels themselves (a worker per channel, the queue, sessions,
//! approvals) are the shared engine of [`super::channels`]; [`Discord`]
//! is its Discord side.
//!
//! Approval buttons are message components (developer docs, components
//! reference: an Action Row, type 1, of Buttons, type 2, each with a
//! `custom_id` of 1 to 100 characters). A press arrives as the Gateway
//! event `INTERACTION_CREATE` (no intent needed) holding an interaction of
//! type 3, `MESSAGE_COMPONENT` (interactions, "Receiving and
//! Responding"), which has to be answered over HTTP "within 3 seconds of
//! receiving the event": the press goes to the channel engine at once, and
//! the answer is type 6, `DEFERRED_UPDATE_MESSAGE` ("ACK an interaction and
//! edit the original message later; the user does not see a loading
//! state"), when it answered the question. The engine then edits the
//! message: buttons off, result added. Any other press gets type 4 with
//! the `EPHEMERAL` flag (1 << 6), a note only the presser sees.
//!
//! A press counts as it arrives, before Discord has the answer: when the
//! answer comes too late (a request retried past the 3 seconds, an event
//! sent again after a RESUME), Discord shows "This interaction failed"
//! although the press was applied; the edited question message shows the
//! result. Presses older than [`STALE_PRESS`] (by the time in their
//! interaction id) are dropped without an answer: their interaction token
//! has expired (it is valid for 15 minutes), so Discord shows them as
//! failed whatever Junees does, and only an event sent again comes that
//! late. A tighter limit, such as the 3 seconds, would hang on this
//! machine's clock agreeing with Discord's: a clock a few seconds off would
//! drop every press.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::ask::{
    Choice, Press, Question, button_data, fence_safe, human, parse_button, without_buttons,
};
use super::channels::{Channels, Shared};
use super::config::DiscordConfig;
use super::platform::Platform;
use super::rest::Rest;
use super::shard::Dispatch;
use super::split::{MESSAGE_LIMIT, length, split_message};
use crate::approval::Request;
use crate::images::Image;
use crate::images::discord::{self as attached, Attachment, Fetcher};

pub use super::channels::Command;

/// Message types that carry what a user wrote: DEFAULT and REPLY.
const USER_MESSAGE_TYPES: [u64; 2] = [0, 19];

/// What the commands do, for `!help`.
pub const HELP: &str = "\
`!new` start a new conversation in this channel
`!stop` stop the turn that runs here (and drop the queued messages)
`!status` what runs here: session, queue, tokens
`!memory` what the persistent memory holds (in a server channel only how much; the contents in a DM)
`!skills` the skills Junees knows
`!cron` the scheduled jobs (in a server channel without their prompts and folders)
`!webhooks` the webhook routes (in a server channel without their prompts and folders)
`!help` this list
In a server channel, mention the bot (`@Junees !stop`) unless the channel is one of discord_channels.";

/// Interaction callback types (interactions, "Receiving and Responding",
/// Interaction Callback Type).
const CHANNEL_MESSAGE_WITH_SOURCE: u64 = 4;
const DEFERRED_UPDATE_MESSAGE: u64 = 6;
/// The message flag of a reply only the user who pressed sees.
const EPHEMERAL: u64 = 1 << 6;
/// Interaction type of a message component.
const MESSAGE_COMPONENT: u64 = 3;

/// Discord's epoch, 2015-01-01, in Unix milliseconds: a snowflake id
/// holds the time it was made as milliseconds since then, above bit 22
/// (developer docs, "API Reference", Snowflakes).
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Presses made longer ago are dropped without an answer (module docs).
pub const STALE_PRESS: Duration = Duration::from_secs(15 * 60);

/// How long before `now` snowflake `id` was made; `None` for an id that
/// is no number or lies after `now` (a clock behind Discord's).
pub fn snowflake_age(id: &str, now: SystemTime) -> Option<Duration> {
    let made = (id.parse::<u64>().ok()? >> 22).checked_add(DISCORD_EPOCH_MS)?;
    let now = u64::try_from(now.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()?;
    now.checked_sub(made).map(Duration::from_millis)
}

/// What a press that answered nothing gets back.
pub const ALREADY_HANDLED: &str =
    "이미 답했거나 시간이 지났거나 멈춘 질문이라 이 버튼은 이제 아무것도 하지 않습니다.";
/// What a press of a user not in `discord_allowed_users` gets back.
pub const NOT_ALLOWED: &str = "discord_allowed_users에 있는 사용자만 Junees에게 답할 수 있습니다.";
/// What a press of a button that is not Junees's gets back.
const NOT_OURS: &str = "Junees does not know this button.";

/// Discord as the channel engine's platform: posts and typing through the
/// REST API, images from Discord's CDN.
pub struct Discord {
    pub rest: Arc<Rest>,
    /// Downloads the images attached to messages.
    pub fetcher: Fetcher,
    /// The questions posted with buttons, by question id, until they end.
    questions: Mutex<HashMap<String, Posted>>,
}

/// A question message with buttons.
struct Posted {
    channel: String,
    message: String,
    /// Its text, which the edit keeps.
    content: String,
}

impl Discord {
    pub fn new(rest: Arc<Rest>, fetcher: Fetcher) -> Self {
        Self {
            rest,
            fetcher,
            questions: Mutex::new(HashMap::new()),
        }
    }

    fn questions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Posted>> {
        self.questions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// After posting a question with buttons failed with `err`: posts
    /// `parts` and then `last`, the parts not shown yet, without buttons;
    /// the error to report.
    async fn post_without_buttons(
        &self,
        channel: &str,
        parts: &[String],
        last: &str,
        err: anyhow::Error,
    ) -> anyhow::Error {
        let last = without_buttons(last);
        for part in parts.iter().map(String::as_str).chain([last.as_str()]) {
            if let Err(again) = self.rest.create_message(channel, part).await {
                return anyhow!("{err:#}; posting it without buttons failed too: {again:#}");
            }
        }
        anyhow!("{err:#}; posted it without buttons")
    }
}

/// The buttons of `question`: one action row.
fn components(question: &Question) -> Value {
    let buttons: Vec<Value> = Choice::offered(question)
        .into_iter()
        .map(|choice| {
            // Button styles: Primary 1, Success 3, Danger 4.
            let style = match choice {
                Choice::Allow => 3,
                Choice::Always => 1,
                Choice::Decline => 4,
            };
            json!({
                "type": 2,
                "style": style,
                "label": choice.label(),
                "custom_id": button_data(&question.id, choice),
            })
        })
        .collect();
    json!([{"type": 1, "components": buttons}])
}

#[async_trait]
impl Platform for Discord {
    type Attachment = Attachment;

    fn place(&self) -> &'static str {
        "channel"
    }

    fn message_limit(&self) -> usize {
        MESSAGE_LIMIT
    }

    async fn post(&self, channel: &str, text: &str) -> Result<()> {
        self.rest.create_message(channel, text).await
    }

    async fn typing(&self, channel: &str) -> Result<()> {
        self.rest.typing(channel).await
    }

    async fn fetch_all(&self, attachments: &[Attachment]) -> (Vec<Image>, Vec<String>) {
        attached::fetch_all(&self.fetcher, attachments).await
    }

    async fn fetch_audio(
        &self,
        attachment: &Attachment,
        limit: u64,
    ) -> Result<Vec<u8>, Result<u64, String>> {
        self.fetcher.bytes(attachment, limit).await
    }

    fn command(&self, word: &str) -> String {
        format!("`!{word}`")
    }

    fn help(&self) -> String {
        HELP.to_string()
    }

    fn task_note(&self) -> String {
        "`!task` is not available in the gateway yet; run `junees task \"<goal>\"` on the \
         machine (task progress can go to a Discord webhook, notify_webhook)."
            .to_string()
    }

    fn tool_lines(&self, lines: &str) -> String {
        format!("```\n{}\n```", fence_safe(lines))
    }

    fn private_place(&self) -> &'static str {
        "a direct message"
    }

    fn question(request: &Request, timeout: Duration) -> String {
        super::ask::question(request, timeout)
    }

    /// The question's last message (it is cut like any post) carries the
    /// buttons.
    async fn post_question(&self, channel: &str, question: &Question) -> Result<Option<String>> {
        let mut parts = split_message(&question.text, MESSAGE_LIMIT);
        let last = parts.pop().unwrap_or_default();
        for (at, part) in parts.iter().enumerate() {
            if let Err(err) = self.rest.create_message(channel, part).await {
                return Err(self
                    .post_without_buttons(channel, &parts[at..], &last, err)
                    .await);
            }
        }
        let message = match self
            .rest
            .create_message_with(channel, &last, components(question))
            .await
        {
            Ok(message) => message,
            Err(err) => return Err(self.post_without_buttons(channel, &[], &last, err).await),
        };
        self.questions().insert(
            question.id.clone(),
            Posted {
                channel: channel.to_string(),
                message: message.clone(),
                content: last,
            },
        );
        Ok(Some(message))
    }

    /// The result goes under the question, in bold; when that would not
    /// fit the message, the buttons go and the result is a post of its
    /// own.
    async fn close_question(&self, _channel: &str, id: &str, outcome: &str) -> Result<()> {
        let Some(posted) = self.questions().remove(id) else {
            return Ok(());
        };
        let closed = format!("{}\n\n**{outcome}**", posted.content);
        if length(&closed) <= MESSAGE_LIMIT {
            return self
                .rest
                .edit_message(&posted.channel, &posted.message, &closed)
                .await;
        }
        self.rest
            .edit_message(&posted.channel, &posted.message, &posted.content)
            .await?;
        self.rest
            .create_message(&posted.channel, &format!("**{outcome}**"))
            .await
    }
}

/// The command `text` is, if it starts with `!` and a word.
pub fn parse_command(text: &str) -> Option<Command> {
    let rest = text.trim().strip_prefix('!')?;
    let word = rest
        .split_whitespace()
        .next()
        .filter(|_| rest.starts_with(|c: char| !c.is_whitespace()))?;
    command_word(word)
}

/// The command a word names, if it is one word of ASCII letters (a known
/// command or [`Command::Unknown`]).
pub fn command_word(word: &str) -> Option<Command> {
    if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(match word.to_ascii_lowercase().as_str() {
        "new" | "reset" => Command::New,
        "stop" => Command::Stop,
        "status" => Command::Status,
        "memory" => Command::Memory,
        "skills" => Command::Skills,
        "cron" => Command::Cron,
        "webhooks" => Command::Webhooks,
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
    channels: Channels<Discord>,
    config: DiscordConfig,
    /// The bot's own user id, from Ready.
    me: Option<String>,
    /// Threads the bot answers in without a mention: its own, and those
    /// under a channel of `discord_channels`.
    threads: HashSet<String>,
    /// Users already reported as not allowed (reported once).
    strangers: HashSet<String>,
}

impl Bot {
    pub fn new(shared: Arc<Shared<Discord>>, config: DiscordConfig) -> Self {
        Self {
            channels: Channels::new(shared),
            config,
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
            "INTERACTION_CREATE" => self.interaction(data),
            _ => {}
        }
    }

    /// A press on a button: answers the question it belongs to if an
    /// allowed user pressed it on the question that waits in that channel
    /// ([`Channels::press`]), and answers the interaction in the
    /// background (module docs).
    fn interaction(&mut self, data: &Value) {
        let text = |pointer: &str| data.pointer(pointer).and_then(Value::as_str);
        // The bot registers no commands: only its buttons send these.
        if data.get("type").and_then(Value::as_u64) != Some(MESSAGE_COMPONENT) {
            return;
        }
        let (Some(id), Some(token)) = (text("/id"), text("/token")) else {
            return;
        };
        if let Some(age) = snowflake_age(id, SystemTime::now())
            && age > STALE_PRESS
        {
            (self.channels.shared().log)(&format!(
                "ignoring a button press made {} ago: too late to answer",
                human(age)
            ));
            return;
        }
        // `member` in a server, `user` in a DM.
        let user = text("/member/user/id").or_else(|| text("/user/id"));
        let channel = text("/channel_id").or_else(|| text("/channel/id"));
        let refusal = match (user, channel) {
            (Some(user), Some(channel)) if self.config.allowed_users.contains(user) => {
                match text("/data/custom_id").and_then(parse_button) {
                    Some((question, choice)) => {
                        let press = Press {
                            id: question,
                            choice,
                            message: text("/message/id").map(str::to_string),
                            user: user.to_string(),
                            by: format!("<@{user}>"),
                        };
                        if self.channels.press(channel, press) {
                            None
                        } else {
                            Some(ALREADY_HANDLED)
                        }
                    }
                    None => Some(NOT_OURS),
                }
            }
            (user, _) => {
                if let Some(user) = user
                    && self.strangers.insert(user.to_string())
                {
                    (self.channels.shared().log)(&format!(
                        "ignoring button presses of user {user}: not in discord_allowed_users"
                    ));
                }
                Some(NOT_ALLOWED)
            }
        };
        let response = match refusal {
            None => json!({"type": DEFERRED_UPDATE_MESSAGE}),
            Some(note) => json!({
                "type": CHANNEL_MESSAGE_WITH_SOURCE,
                "data": {
                    "content": note,
                    "flags": EPHEMERAL,
                    "allowed_mentions": {"parse": []},
                },
            }),
        };
        let shared = self.channels.shared();
        let rest = Arc::clone(&shared.platform.rest);
        let log = Arc::clone(&shared.log);
        let (id, token) = (id.to_string(), token.to_string());
        tokio::spawn(async move {
            if let Err(err) = rest.respond(&id, &token, response).await {
                log(&format!("cannot answer a button press: {err:#}"));
            }
        });
    }

    fn thread(&mut self, thread: &Value) {
        let id = thread.get("id").and_then(Value::as_str);
        let owner = thread.get("owner_id").and_then(Value::as_str);
        let parent = thread.get("parent_id").and_then(Value::as_str);
        let ours = owner.is_some() && owner == self.me.as_deref();
        let listed = parent.is_some_and(|parent| self.config.channels.contains(parent));
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
        if !self.config.allowed_users.contains(user) {
            if self.strangers.insert(user.to_string()) {
                (self.channels.shared().log)(&format!(
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
        let listed = self.config.channels.contains(channel) || self.threads.contains(channel);
        let answering = self.channels.answering(channel);
        if !(direct || mentioned || listed || answering) {
            return;
        }
        let content = text("/content").unwrap_or_default();
        let content = match &self.me {
            Some(me) => strip_mention(content, me),
            None => content.trim().to_string(),
        };
        let (attachments, notes) = attached::attachments(data);
        if content.is_empty() && attachments.is_empty() && notes.is_empty() {
            return;
        }
        if let Some(command) = parse_command(&content) {
            self.channels.command(channel, command, direct);
            return;
        }
        self.channels
            .input(channel, user, content, attachments, notes);
    }

    /// Stops every channel: running turns are cancelled (and recorded as
    /// such), queued messages dropped, and the workers waited for.
    pub async fn shutdown(self, grace: Duration) {
        self.channels.shutdown(grace).await;
    }
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
        assert_eq!(parse_command("!cron"), Some(Command::Cron));
        assert_eq!(parse_command("!webhooks"), Some(Command::Webhooks));
        assert_eq!(parse_command("!task build it"), Some(Command::Task));
        assert_eq!(parse_command("!foo"), Some(Command::Unknown("foo".into())));
        for text in ["new", "!", "! new", "!!!", "!123", "hello !new"] {
            assert_eq!(parse_command(text), None, "{text}");
        }
    }

    #[test]
    fn reads_the_age_of_an_interaction() {
        // The docs' example: 175928847299117063 was made at 1462015105796.
        let made = UNIX_EPOCH + Duration::from_millis(1_462_015_105_796);
        let id = "175928847299117063";
        assert_eq!(snowflake_age(id, made), Some(Duration::ZERO));
        assert_eq!(
            snowflake_age(id, made + Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );
        assert_eq!(snowflake_age(id, made - Duration::from_secs(1)), None);
        assert_eq!(snowflake_age("not-a-number", made), None);
    }

    #[test]
    fn strips_the_bot_mention() {
        assert_eq!(strip_mention("<@42> hi", "42"), "hi");
        assert_eq!(strip_mention("hi <@!42>", "42"), "hi");
        assert_eq!(strip_mention("<@43> hi", "42"), "<@43> hi");
    }
}
