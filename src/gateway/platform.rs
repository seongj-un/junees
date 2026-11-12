//! What a chat platform gives the channel engine ([`super::channels`]):
//! posting a message, the typing indicator, downloading attached images,
//! and how its replies spell commands, tool lines and approval questions.
//! Discord ([`super::bot::Discord`]) and Telegram
//! ([`crate::telegram::platform::Telegram`]) implement it; the engine
//! (channels, queues, sessions, approvals, `stop`/`status`/`new`) is the
//! same for both.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::approval::Request;
use crate::images::Image;

#[async_trait]
pub trait Platform: Send + Sync + 'static {
    /// An image attached to a message, as the platform names it; it is
    /// downloaded when its turn starts.
    type Attachment: Send + 'static;

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
}
