//! Which Telegram messages are for the bot, and the commands. The chats
//! themselves (a worker per chat, the queue, sessions, approvals) are the
//! shared engine of [`crate::gateway::channels`].
//!
//! Commands are Telegram's `/word` (also `/word@bot` in a group, as
//! Telegram writes them, and Discord's `!word`). Junees's own terminal
//! slash commands (`/model`, `/sessions`, ...) do not exist here: a `/`
//! word the gateway does not know gets "unknown command" in a private
//! chat, and is left to the other bots in a group. `/start`, which
//! Telegram sends when a user opens the bot, shows the help.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::api::{Me, Target};
use super::platform::{MAX_ATTACHMENTS, Telegram, TelegramFile, attachments};
use crate::gateway::bot::command_word;
use crate::gateway::channels::{Channels, Command, Shared};

/// What the start of a message says about commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// No command.
    None,
    /// A command; `named` when it names this bot (`/stop@bot`).
    Command { command: Command, named: bool },
    /// A command for another bot (`/start@other_bot`).
    OtherBot,
}

/// Reads a command at the start of `text`: `/word`, `/word@bot` or
/// `!word`, followed by the end or whitespace. `/etc/hosts` and `/ x` are
/// no commands.
pub fn parse_command(text: &str, me: &str) -> Parsed {
    let text = text.trim_start();
    let Some(rest) = text.strip_prefix('/').or_else(|| text.strip_prefix('!')) else {
        return Parsed::None;
    };
    let first = rest.split_whitespace().next().unwrap_or_default();
    if !rest.starts_with(first) || first.is_empty() {
        return Parsed::None;
    }
    let (word, bot) = match first.split_once('@') {
        Some((word, bot)) if text.starts_with('/') => (word, Some(bot)),
        _ => (first, None),
    };
    let Some(command) = command_word(word) else {
        return Parsed::None;
    };
    let command = match command {
        Command::Unknown(word) if word.eq_ignore_ascii_case("start") => Command::Help,
        command => command,
    };
    match bot {
        Some(bot) if !bot.eq_ignore_ascii_case(me) => Parsed::OtherBot,
        named => Parsed::Command {
            command,
            named: named.is_some(),
        },
    }
}

/// `text` without `@username` (any case), trimmed.
pub fn strip_mention(text: &str, username: &str) -> String {
    let mention = format!("@{username}");
    let lower = text.to_ascii_lowercase();
    let wanted = mention.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    // ASCII lowercasing keeps every byte where it was, and `@` is ASCII,
    // so the matches sit on character boundaries of `text` too.
    while let Some(found) = lower[at..].find(&wanted) {
        let start = at + found;
        let end = start + wanted.len();
        let word_ends = text[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        out.push_str(&text[at..start]);
        if !word_ends {
            out.push_str(&text[start..end]);
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out.trim().to_string()
}

/// The text an entity covers: `offset` and `length` count UTF-16 code
/// units (Bot API, MessageEntity).
fn entity_text(text: &str, entity: &Value) -> Option<String> {
    let offset = usize::try_from(entity.get("offset")?.as_u64()?).ok()?;
    let length = usize::try_from(entity.get("length")?.as_u64()?).ok()?;
    let units: Vec<u16> = text.encode_utf16().collect();
    let slice = units.get(offset..offset.checked_add(length)?)?;
    String::from_utf16(slice).ok()
}

/// A message with the rest of its album (messages of one
/// `media_group_id`, which Telegram sends one by one).
struct Incoming<'a> {
    message: &'a Value,
    album: Vec<&'a Value>,
}

/// The router: hands the bot's messages to the chats.
pub struct Router {
    channels: Channels<Telegram>,
    allowed: HashSet<i64>,
    me: Me,
    /// Users already reported as not allowed (reported once).
    strangers: HashSet<i64>,
}

impl Router {
    pub fn new(shared: Arc<Shared<Telegram>>, allowed: HashSet<i64>, me: Me) -> Self {
        Self {
            channels: Channels::new(shared),
            allowed,
            me,
            strangers: HashSet::new(),
        }
    }

    /// Handles a batch of updates from `getUpdates`, in order. The
    /// messages of one album in a row become one input.
    pub fn handle(&mut self, updates: &[Value]) {
        let messages: Vec<&Value> = updates
            .iter()
            .filter_map(|update| update.get("message"))
            .collect();
        let mut incoming: Vec<Incoming> = Vec::new();
        for message in messages {
            let group = message.get("media_group_id").and_then(Value::as_str);
            if let (Some(group), Some(last)) = (group, incoming.last_mut())
                && last.message.get("media_group_id").and_then(Value::as_str) == Some(group)
                && last.message.pointer("/chat/id") == message.pointer("/chat/id")
                && last.message.pointer("/from/id") == message.pointer("/from/id")
            {
                last.album.push(message);
                continue;
            }
            incoming.push(Incoming {
                message,
                album: Vec::new(),
            });
        }
        for item in incoming {
            self.message(item);
        }
    }

    fn message(&mut self, incoming: Incoming) {
        let message = incoming.message;
        if message.pointer("/from/is_bot").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let (Some(user), Some(chat)) = (
            message.pointer("/from/id").and_then(Value::as_i64),
            message.pointer("/chat/id").and_then(Value::as_i64),
        ) else {
            return;
        };
        let kind = message
            .pointer("/chat/type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(kind, "private" | "group" | "supergroup") {
            return;
        }
        if !self.allowed.contains(&user) {
            if self.strangers.insert(user) {
                (self.channels.shared().log)(&format!(
                    "ignoring messages of user {user}: not in telegram_allowed_users"
                ));
            }
            return;
        }
        let direct = kind == "private";
        // A forum topic is a conversation of its own; reply threads in an
        // ordinary group are not.
        let thread = if message.get("is_topic_message").and_then(Value::as_bool) == Some(true) {
            message.get("message_thread_id").and_then(Value::as_i64)
        } else {
            None
        };
        let channel = Target { chat, thread }.key();
        let text_of = |message: &Value| {
            message
                .get("text")
                .or_else(|| message.get("caption"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let text = text_of(message);
        match parse_command(&text, &self.me.username) {
            Parsed::OtherBot => return,
            Parsed::Command { command, named } => {
                // In a group, a `/word` the gateway does not know is
                // likely another bot's.
                if direct || named || !matches!(command, Command::Unknown(_)) {
                    self.channels.command(&channel, command, direct);
                }
                return;
            }
            Parsed::None => {}
        }
        let mentioned = self.mentioned(message, &text);
        let replied = message
            .pointer("/reply_to_message/from/id")
            .and_then(Value::as_i64)
            == Some(self.me.id);
        let answering = self.channels.answering(&channel);
        if !(direct || mentioned || replied || answering) {
            return;
        }
        let mut content = strip_mention(&text, &self.me.username);
        let (mut files, mut notes) = attachments(message);
        for part in &incoming.album {
            let caption = text_of(part);
            if !caption.trim().is_empty() {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&strip_mention(&caption, &self.me.username));
            }
            let (more, more_notes) = attachments(part);
            files.extend(more);
            notes.extend(more_notes);
        }
        let files = limit(files, &mut notes);
        if content.is_empty() && files.is_empty() && notes.is_empty() {
            return;
        }
        self.channels.input(&channel, content, files, notes);
    }

    /// Whether `message` mentions the bot: `@username` as a mention
    /// entity, or a text mention of its user.
    fn mentioned(&self, message: &Value, text: &str) -> bool {
        let entities = message
            .get("entities")
            .or_else(|| message.get("caption_entities"))
            .and_then(Value::as_array);
        entities.into_iter().flatten().any(|entity| {
            match entity.get("type").and_then(Value::as_str) {
                Some("mention") => entity_text(text, entity).is_some_and(|mention| {
                    mention
                        .strip_prefix('@')
                        .is_some_and(|name| name.eq_ignore_ascii_case(&self.me.username))
                }),
                Some("text_mention") => {
                    entity.pointer("/user/id").and_then(Value::as_i64) == Some(self.me.id)
                }
                _ => false,
            }
        })
    }

    /// Stops every chat: running turns are cancelled (and recorded as
    /// such), queued messages dropped, and the workers waited for.
    pub async fn shutdown(self, grace: Duration) {
        self.channels.shutdown(grace).await;
    }
}

/// At most [`MAX_ATTACHMENTS`] files; a note for each one past it.
fn limit(mut files: Vec<TelegramFile>, notes: &mut Vec<String>) -> Vec<TelegramFile> {
    if files.len() > MAX_ATTACHMENTS {
        for file in files.drain(MAX_ATTACHMENTS..) {
            notes.push(format!(
                "\"{}\" left out: at most {MAX_ATTACHMENTS} images per message",
                file.name
            ));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn command(command: Command, named: bool) -> Parsed {
        Parsed::Command { command, named }
    }

    #[test]
    fn reads_telegram_commands() {
        let me = "Junees_bot";
        assert_eq!(parse_command("/stop", me), command(Command::Stop, false));
        assert_eq!(
            parse_command("  /NEW now", me),
            command(Command::New, false)
        );
        assert_eq!(
            parse_command("/status@junees_BOT", me),
            command(Command::Status, true)
        );
        assert_eq!(parse_command("/start", me), command(Command::Help, false));
        assert_eq!(
            parse_command("!memory", me),
            command(Command::Memory, false)
        );
        assert_eq!(
            parse_command("/model opus", me),
            command(Command::Unknown("model".into()), false)
        );
        assert_eq!(parse_command("/stop@other_bot", me), Parsed::OtherBot);
        for text in [
            "stop",
            "/",
            "/ stop",
            "/etc/hosts",
            "/123",
            "hi /stop",
            "!stop@x y",
        ] {
            assert!(
                !matches!(parse_command(text, me), Parsed::Command { named: true, .. }),
                "{text}"
            );
        }
        for text in ["stop", "/", "/ stop", "/etc/hosts", "/123", "hi /stop"] {
            assert_eq!(parse_command(text, me), Parsed::None, "{text}");
        }
    }

    #[test]
    fn strips_the_mention_in_any_case() {
        assert_eq!(strip_mention("@Junees_bot hi", "junees_bot"), "hi");
        assert_eq!(
            strip_mention("안녕 @JUNEES_BOT 하세요", "junees_bot"),
            "안녕  하세요"
        );
        assert_eq!(
            strip_mention("@junees_bot2 hi", "junees_bot"),
            "@junees_bot2 hi"
        );
    }

    #[test]
    fn entities_count_utf16_units() {
        // "😀" is two UTF-16 units, so the mention starts at 3.
        let text = "😀 @junees_bot hi";
        let entity = json!({"type": "mention", "offset": 3, "length": 11});
        assert_eq!(entity_text(text, &entity).as_deref(), Some("@junees_bot"));
        let outside = json!({"type": "mention", "offset": 30, "length": 11});
        assert_eq!(entity_text(text, &outside), None);
    }
}
