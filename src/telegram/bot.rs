//! Which Telegram messages are for the bot, and the commands. The chats
//! themselves (a worker per chat, the queue, sessions, approvals) are the
//! shared engine of [`crate::gateway::channels`].
//!
//! Commands are Telegram's `/word` (also `/word@bot` or `@bot /word` in
//! a group, and Discord's `!word`). Junees's own terminal slash commands
//! (`/model`, `/sessions`, ...) do not exist here: a `/` word the gateway
//! does not know gets "unknown command" in a private chat or when it
//! names the bot, and is left to the other bots in a group otherwise.
//! An unknown word with more text after it that is not one of those
//! terminal commands (`/tmp is full`) is an ordinary message.
//! `/start`, which Telegram sends when a user opens the bot, shows the
//! help. A caption under a file and a forwarded message are never
//! commands.
//!
//! A press on an approval question's button arrives as a `callback_query`
//! update. It answers the question only when an allowed user pressed it
//! on the question that waits in that chat (or forum topic), from the
//! message holding it ([`Channels::press`]); every press gets
//! `answerCallbackQuery`, with a note when it answered nothing.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::api::{Me, Target};
use super::platform::{MAX_ATTACHMENTS, Telegram, TelegramFile, attachments};
use crate::gateway::ask::{Press, parse_button};
use crate::gateway::bot::command_word;
use crate::gateway::channels::{Channels, Command, Shared};
use crate::prompt_text::escape_hidden;

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
/// no commands, and neither is a word the gateway does not know that
/// more text follows (`/tmp is full`), unless it names the bot or is one
/// of Junees's terminal commands (`/model opus` still gets "unknown
/// command", which says the gateway has no such thing).
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
    if let Command::Unknown(word) = &command
        && bot.is_none()
        && !rest[first.len()..].trim().is_empty()
        && !crate::slash::is_command(&word.to_ascii_lowercase())
    {
        return Parsed::None;
    }
    match bot {
        Some(bot) if !bot.eq_ignore_ascii_case(me) => Parsed::OtherBot,
        named => Parsed::Command {
            command,
            named: named.is_some(),
        },
    }
}

/// Whether `text` starts like a command, `/word`, known or not.
fn command_form(text: &str) -> bool {
    text.trim_start()
        .strip_prefix('/')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_alphabetic())
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

/// The longest sender name a forwarded message's note repeats.
const MAX_FORWARDED_NAME: usize = 64;

/// Who wrote a forwarded message (`forward_origin`; the older
/// `forward_from`, `forward_sender_name` and `forward_from_chat` too),
/// hidden characters escaped; `None` when `message` is not forwarded.
pub fn forwarded_from(message: &Value) -> Option<String> {
    let origin = message.get("forward_origin");
    if origin.is_none() && message.get("forward_date").is_none() {
        return None;
    }
    let text = |value: Option<&Value>, key: &str| {
        value
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let person = |user: Option<&Value>| {
        let name = [text(user, "first_name"), text(user, "last_name")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        match text(user, "username") {
            Some(username) if name.is_empty() => Some(format!("@{username}")),
            Some(username) => Some(format!("{name} (@{username})")),
            None => Some(name),
        }
    };
    let field = |key: &str| origin.and_then(|origin| origin.get(key));
    let name = match text(origin, "type").as_deref() {
        Some("user") => person(field("sender_user")),
        Some("hidden_user") => text(origin, "sender_user_name"),
        Some("chat") => text(field("sender_chat"), "title"),
        Some("channel") => text(field("chat"), "title"),
        _ => message
            .get("forward_from")
            .and_then(|user| person(Some(user)))
            .or_else(|| text(Some(message), "forward_sender_name"))
            .or_else(|| text(message.get("forward_from_chat"), "title")),
    };
    let name = name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "someone".to_string());
    let mut name = escape_hidden(&name);
    if let Some((cut, _)) = name.char_indices().nth(MAX_FORWARDED_NAME) {
        name.truncate(cut);
        name.push_str("...");
    }
    Some(name)
}

/// A message with the rest of its album (messages of one
/// `media_group_id`, which Telegram sends one by one).
struct Incoming<'a> {
    message: &'a Value,
    album: Vec<&'a Value>,
}

/// The parts of an album received so far, held back until no part came
/// for a while.
struct Held {
    key: AlbumKey,
    messages: Vec<Value>,
    /// When to hand them on if no other part comes.
    due: Instant,
}

/// The album `message` belongs to: chat, sender, `media_group_id`.
fn album_key(message: &Value) -> Option<AlbumKey> {
    Some((
        message.pointer("/chat/id")?.as_i64()?,
        message
            .pointer("/from/id")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        message.get("media_group_id")?.as_str()?.to_string(),
    ))
}

/// What a press that answered nothing shows.
pub const ALREADY_HANDLED: &str = "이미 답했거나 시간이 지났거나 멈춘 질문입니다.";
/// What a press of a user not in `telegram_allowed_users` shows.
pub const NOT_ALLOWED: &str = "telegram_allowed_users에 있는 사용자만 Junees에 답할 수 있습니다.";
/// What a press of a button that is not Junees's shows.
const NOT_OURS: &str = "Junees does not know this button.";

/// Who pressed a button, as the result line names them: first and last
/// name, hidden characters escaped, cut.
fn presser(user: &Value) -> String {
    let name = ["first_name", "last_name"]
        .iter()
        .filter_map(|key| user.get(*key).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let name = name.trim();
    let mut name = escape_hidden(if name.is_empty() { "someone" } else { name });
    if let Some((cut, _)) = name.char_indices().nth(MAX_FORWARDED_NAME) {
        name.truncate(cut);
        name.push_str("...");
    }
    name
}

/// Albums remembered as taken in a group.
const ALBUMS: usize = 64;

/// An album in a chat: chat id, sender, `media_group_id`.
type AlbumKey = (i64, i64, String);

/// The router: hands the bot's messages to the chats.
pub struct Router {
    channels: Channels<Telegram>,
    allowed: HashSet<i64>,
    me: Me,
    /// Users already reported as not allowed (reported once).
    strangers: HashSet<i64>,
    /// Albums whose first part was for the bot in a group, newest last
    /// ([`ALBUMS`] at most): the parts that come after the album was
    /// handed on have no caption (no mention), and are taken too.
    albums: VecDeque<AlbumKey>,
    /// Albums being received ([`Self::handle`]).
    held: Vec<Held>,
    /// How long an album waits for its next part.
    album_wait: Duration,
}

impl Router {
    /// `album_wait`: how long an album's parts are held back for the
    /// next one ([`Self::handle`]).
    pub fn new(
        shared: Arc<Shared<Telegram>>,
        allowed: HashSet<i64>,
        me: Me,
        album_wait: Duration,
    ) -> Self {
        Self {
            channels: Channels::new(shared),
            allowed,
            me,
            strangers: HashSet::new(),
            albums: VecDeque::new(),
            held: Vec::new(),
            album_wait,
        }
    }

    /// Handles a batch of updates from `getUpdates`, in order. Telegram
    /// sends the messages of an album one by one, and a long poll answers
    /// as soon as the first is there, so the rest may come in the next
    /// answers: an album's parts are held until none came for
    /// `album_wait` ([`Self::next_due`], [`Self::flush_due`]) and then
    /// become one input. Another message in the same chat hands the held
    /// albums of that chat on first, so the chat keeps its order.
    pub fn handle(&mut self, updates: &[Value]) {
        let now = Instant::now();
        for update in updates {
            if let Some(query) = update.get("callback_query") {
                self.press(query);
                continue;
            }
            let Some(message) = update.get("message") else {
                continue;
            };
            if let Some(key) = album_key(message) {
                let due = now + self.album_wait;
                match self.held.iter_mut().find(|held| held.key == key) {
                    Some(held) => {
                        held.messages.push(message.clone());
                        held.due = due;
                    }
                    None => self.held.push(Held {
                        key,
                        messages: vec![message.clone()],
                        due,
                    }),
                }
                continue;
            }
            if let Some(chat) = message.pointer("/chat/id").and_then(Value::as_i64) {
                self.release(|held| held.key.0 == chat);
            }
            self.message(Incoming {
                message,
                album: Vec::new(),
            });
        }
        self.flush_due();
    }

    /// A press on an inline keyboard button (module docs).
    fn press(&mut self, query: &Value) {
        let Some(id) = query.get("id").and_then(Value::as_str) else {
            return;
        };
        let user = query.pointer("/from/id").and_then(Value::as_i64);
        let note = match user {
            Some(user) if self.allowed.contains(&user) => {
                let message = query.get("message");
                let chat = message
                    .and_then(|message| message.pointer("/chat/id"))
                    .and_then(Value::as_i64);
                let data = query.get("data").and_then(Value::as_str);
                match (chat, data.and_then(parse_button)) {
                    (Some(chat), Some((question, choice))) => {
                        let message = message.unwrap_or(&Value::Null);
                        let topic =
                            message.get("is_topic_message").and_then(Value::as_bool) == Some(true);
                        let thread = message
                            .get("message_thread_id")
                            .and_then(Value::as_i64)
                            .filter(|_| topic);
                        let press = Press {
                            id: question,
                            choice,
                            message: message
                                .get("message_id")
                                .and_then(Value::as_i64)
                                .map(|id| id.to_string()),
                            user: user.to_string(),
                            by: presser(query.get("from").unwrap_or(&Value::Null)),
                        };
                        if self.channels.press(&Target { chat, thread }.key(), press) {
                            None
                        } else {
                            Some(ALREADY_HANDLED)
                        }
                    }
                    _ => Some(NOT_OURS),
                }
            }
            _ => {
                if let Some(user) = user
                    && self.strangers.insert(user)
                {
                    (self.channels.shared().log)(&format!(
                        "ignoring button presses of user {user}: not in telegram_allowed_users"
                    ));
                }
                Some(NOT_ALLOWED)
            }
        };
        let shared = self.channels.shared();
        let api = Arc::clone(&shared.platform.api);
        let log = Arc::clone(&shared.log);
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(err) = api.answer_callback(&id, note).await {
                log(&format!("cannot answer a button press: {err}"));
            }
        });
    }

    /// When the next held album is due, if one is held.
    pub fn next_due(&self) -> Option<Instant> {
        self.held.iter().map(|held| held.due).min()
    }

    /// Hands on the held albums that are due.
    pub fn flush_due(&mut self) {
        let now = Instant::now();
        self.release(|held| held.due <= now);
    }

    /// Hands on the held albums `pick` chooses, oldest first.
    fn release(&mut self, pick: impl Fn(&Held) -> bool) {
        let (ready, kept): (Vec<Held>, Vec<Held>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|held| pick(held));
        self.held = kept;
        for held in ready {
            if let Some((message, rest)) = held.messages.split_first() {
                self.message(Incoming {
                    message,
                    album: rest.iter().collect(),
                });
            }
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
        // Someone else's words: never a command, and marked as theirs, so
        // the model does not take them as the user's.
        let forwarded = forwarded_from(message);
        // A caption is about its file: "/new" under a photo is no command
        // that drops the photo.
        let (mut files, mut notes) = attachments(message);
        let attached = !(files.is_empty() && notes.is_empty() && incoming.album.is_empty());
        let commands = forwarded.is_none() && !attached;
        match parse_command(&text, &self.me.username) {
            _ if !commands => {}
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
        // A waiting question takes any message of the chat as its answer,
        // but a forwarded one is no answer of the user's, and in a group
        // neither is a `/word` with more text (`/roll 2d6`): likely another
        // bot's command (the gateway's own commands were taken above).
        let other_command = !direct && commands && command_form(&text);
        let answering = forwarded.is_none() && !other_command && self.channels.answering(&channel);
        let album = message
            .get("media_group_id")
            .and_then(Value::as_str)
            .map(|id| (chat, user, id.to_string()));
        let rest_of_album = album.as_ref().is_some_and(|key| self.albums.contains(key));
        if !(direct || mentioned || replied || answering || rest_of_album) {
            return;
        }
        if let Some(key) = album
            && !direct
            && !rest_of_album
        {
            if self.albums.len() == ALBUMS {
                self.albums.pop_front();
            }
            self.albums.push_back(key);
        }
        let mut content = strip_mention(&text, &self.me.username);
        // `@bot /stop`: in a group the mention often comes first. A command
        // after it names this bot, as `/stop@bot` does.
        if commands && content != text.trim() {
            match parse_command(&content, &self.me.username) {
                Parsed::OtherBot => return,
                Parsed::Command { command, .. } => {
                    self.channels.command(&channel, command, direct);
                    return;
                }
                Parsed::None => {}
            }
        }
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
        match forwarded {
            Some(who) => {
                let content = format!(
                    "(forwarded message from {who}; they wrote it, not the user)\n{content}"
                )
                .trim_end()
                .to_string();
                self.channels.queue(&channel, content, files, notes);
            }
            None => self
                .channels
                .input(&channel, &user.to_string(), content, files, notes),
        }
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
        // An unknown word followed by more text reads as a message, unless
        // it names the bot or is a terminal command the gateway lacks.
        for text in ["/tmp is full", "!important read this", "/Opt has 3 GB left"] {
            assert_eq!(parse_command(text, me), Parsed::None, "{text}");
        }
        assert_eq!(
            parse_command("/tmp@junees_bot is full", me),
            command(Command::Unknown("tmp".into()), true)
        );
        assert_eq!(
            parse_command("/tmp", me),
            command(Command::Unknown("tmp".into()), false)
        );
        assert_eq!(
            parse_command("/search build failed", me),
            command(Command::Unknown("search".into()), false)
        );
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
    fn names_who_wrote_a_forwarded_message() {
        let origin = |origin: Value| json!({"text": "hi", "forward_origin": origin});
        let cases = [
            (
                origin(json!({"type": "user", "date": 0, "sender_user":
                    {"id": 5, "is_bot": false, "first_name": "Mallory", "last_name": "M",
                     "username": "mal"}})),
                "Mallory M (@mal)",
            ),
            (
                origin(json!({"type": "hidden_user", "date": 0,
                    "sender_user_name": "Eve\u{202e}"})),
                "Eve\\u{202e}",
            ),
            (
                origin(json!({"type": "chat", "date": 0, "sender_chat":
                    {"id": -1, "type": "group", "title": "Other group"}})),
                "Other group",
            ),
            (
                origin(
                    json!({"type": "channel", "date": 0, "message_id": 3, "chat":
                    {"id": -2, "type": "channel", "title": "News"}}),
                ),
                "News",
            ),
            (
                json!({"text": "hi", "forward_date": 0, "forward_sender_name": "Old"}),
                "Old",
            ),
            (origin(json!({"type": "user", "date": 0})), "someone"),
        ];
        for (message, want) in cases {
            assert_eq!(forwarded_from(&message).as_deref(), Some(want), "{message}");
        }
        let long = origin(json!({"type": "hidden_user", "sender_user_name": "x".repeat(500)}));
        assert_eq!(
            forwarded_from(&long).unwrap(),
            format!("{}...", "x".repeat(MAX_FORWARDED_NAME))
        );
        assert_eq!(forwarded_from(&json!({"text": "hi"})), None);
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
