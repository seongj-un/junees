//! What the bot does with Discord's events: which messages are for it and
//! the `!` commands. The channels themselves (a worker per channel, the
//! queue, sessions, approvals) are the shared engine of
//! [`super::channels`]; [`Discord`] is its Discord side.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::ask::fence_safe;
use super::channels::{Channels, Shared};
use super::config::DiscordConfig;
use super::platform::Platform;
use super::rest::Rest;
use super::shard::Dispatch;
use super::split::MESSAGE_LIMIT;
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

/// Discord as the channel engine's platform: posts and typing through the
/// REST API, images from Discord's CDN.
pub struct Discord {
    pub rest: Arc<Rest>,
    /// Downloads the images attached to messages.
    pub fetcher: Fetcher,
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
            _ => {}
        }
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
        self.channels.input(channel, content, attachments, notes);
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
    fn strips_the_bot_mention() {
        assert_eq!(strip_mention("<@42> hi", "42"), "hi");
        assert_eq!(strip_mention("hi <@!42>", "42"), "hi");
        assert_eq!(strip_mention("<@43> hi", "42"), "<@43> hi");
    }
}
