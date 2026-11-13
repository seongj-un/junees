//! What a chat platform gives the channel engine ([`super::channels`]):
//! posting a message, the typing indicator, downloading attached images,
//! and how its replies spell commands, tool lines and approval questions.
//! Discord ([`super::bot::Discord`]) and Telegram
//! ([`crate::telegram::platform::Telegram`]) implement it; the engine
//! (channels, queues, sessions, approvals, `stop`/`status`/`new`) is the
//! same for both.
//!
//! Approval questions may come with buttons ([`Platform::post_question`]):
//! the platform's router hands a press to
//! [`super::channels::Channels::press`] and answers the platform's own
//! callback; the engine takes the buttons off when the question ends
//! ([`Platform::close_question`]). A platform without buttons keeps the
//! defaults, and its questions are posted as text.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use super::ask::Question;
use crate::approval::Request;
use crate::images::Image;

/// What an attachment the gateway reads holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A picture, for the model.
    Image,
    /// A voice message or audio file, transcribed ([`crate::transcribe`]);
    /// `mime` as the platform gave it.
    Audio { mime: String },
}

/// What the channel engine reads of an attachment.
pub trait Attached {
    /// How it is named in notes, hidden characters escaped.
    fn name(&self) -> &str;
    /// Bytes, as the platform reports them (0 when it does not).
    fn size(&self) -> u64;
    fn kind(&self) -> &Kind;
}

#[async_trait]
pub trait Platform: Send + Sync + 'static {
    /// An image or audio file attached to a message, as the platform names
    /// it; it is downloaded when its turn starts, and audio only when
    /// transcription is on.
    type Attachment: Attached + Send + Sync + 'static;

    /// What a conversation's place is called in replies and log lines
    /// (`channel`, `chat`).
    fn place(&self) -> &'static str;

    /// The longest message [`Platform::post`] takes, in UTF-16 units
    /// ([`super::split::length`]).
    fn message_limit(&self) -> usize;

    /// Whether posts render Markdown code blocks, so a block cut between
    /// two messages is closed and opened again; plain text shows fences
    /// as typed.
    fn code_blocks(&self) -> bool {
        true
    }

    /// Posts one message of at most [`Platform::message_limit`] units.
    /// Errors never hold the bot token.
    async fn post(&self, channel: &str, text: &str) -> Result<()>;

    /// Shows that the bot is typing in `channel`, for a few seconds.
    async fn typing(&self, channel: &str) -> Result<()>;

    /// Downloads `attachments` one after the other: the images that came,
    /// and a note for each that did not.
    async fn fetch_all(&self, attachments: &[Self::Attachment]) -> (Vec<Image>, Vec<String>);

    /// Downloads an audio attachment ([`Kind::Audio`]) from the platform's
    /// own hosts: its bytes, `Err(Ok(len))` when it holds more than
    /// `limit` (`len` as far as known), or `Err(Err(why))` in words that
    /// name the file but never its URL.
    async fn fetch_audio(
        &self,
        attachment: &Self::Attachment,
        limit: u64,
    ) -> Result<Vec<u8>, Result<u64, String>>;

    /// How a gateway command is written in a reply (`` `!new` ``, `/new`).
    fn command(&self, word: &str) -> String;

    /// What the help command shows.
    fn help(&self) -> String;

    /// The answer to the task command, which the gateway does not run.
    fn task_note(&self) -> String;

    /// The tool calls since the last text (one per line), as one message.
    fn tool_lines(&self, lines: &str) -> String;

    /// Where the memory's contents are shown (`a direct message`).
    fn private_place(&self) -> &'static str;

    /// An approval question as posted.
    fn question(request: &Request, timeout: Duration) -> String
    where
        Self: Sized;

    /// Posts `question` with its buttons ([`super::ask::Choice::offered`],
    /// each carrying [`super::ask::button_data`]); the id of the message
    /// that holds them, which a press must come from. `None` when the
    /// platform has no buttons: the engine posts the text instead, and
    /// text answers work either way. When posting with buttons fails, the
    /// platform posts the part of the question not shown yet without them
    /// (and without the words about them, [`super::ask::without_buttons`])
    /// before it returns the error, so the question can still be answered
    /// in text.
    async fn post_question(&self, _channel: &str, _question: &Question) -> Result<Option<String>> {
        Ok(None)
    }

    /// Edits the message of question `id` once it ended: buttons off,
    /// `outcome` shown. Does nothing for a question it did not post.
    async fn close_question(&self, _channel: &str, _id: &str, _outcome: &str) -> Result<()> {
        Ok(())
    }
}
