//! The session journal: every change to the conversation is appended to a
//! JSONL file the moment it happens, so a conversation outlives the process
//! and can be resumed.
//!
//! A journal is `$JUNEES_HOME/sessions/<id>.jsonl`. The first line is a
//! [`Meta`] entry; every later line is one change. [`Transcript::apply`]
//! turns entries into history, and both the agent and [`Session::load`] go
//! through it, so a replayed journal rebuilds exactly what the agent had.

use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::message::{ContentBlock, Message, Role, Usage};

/// The journal format this version writes and the newest it can read.
/// Raise it when old binaries could misread new journals. Format 2 added
/// `clear_tool_results` and `compact`. Format 3 keeps the content blocks
/// Junees does not handle itself (thinking and the like) as the API sent
/// them, which older versions would read as placeholders the API rejects.
/// Format 1 and 2 journals still load; continuing one first writes an
/// [`Entry::Format`] line. Format 4 added `drop_server_calls`. Format 5
/// added conversations run by Claude Code: the meta line's `backend` and
/// `claude_turn` entries, which a format 4 Junees would replay as an empty
/// API conversation. Format 6 added `subagent_usage`. Format 7 added
/// images: `image` blocks and tool results with images, whose source names
/// a file of the session's image folder ([`crate::images::store`]), and
/// `append_content`; a format 6 Junees would send such sources to the API,
/// or refuse a tool result whose content is a list as corrupt. Format 8
/// added `clear_images`, which a format 7 Junees would refuse as a corrupt
/// line.
pub const FORMAT: u32 = 8;

/// [`Meta::backend`] of a conversation Claude Code runs (the `claude-code`
/// backend).
pub const CLAUDE_CODE: &str = "claude-code";

/// [`Meta::source`] of a conversation in a channel of the Discord gateway.
pub const DISCORD: &str = "discord";

/// [`Meta::source`] of a conversation in a chat of the Telegram gateway;
/// its [`Meta::channel`] is the chat id (`<chat>:<topic>` in a forum).
pub const TELEGRAM: &str = "telegram";

/// [`Meta::source`] of a conversation of the Slack gateway; its
/// [`Meta::channel`] is the DM channel id, or `<channel>:<thread_ts>` for
/// a thread in a channel.
pub const SLACK: &str = "slack";

/// [`Meta::source`] of a run of a scheduled job (`junees cron`); its
/// [`Meta::channel`] is the job id.
pub const CRON: &str = "cron";

/// [`Meta::source`] of a run started by a webhook request (`junees
/// webhook`); its [`Meta::channel`] is the route name.
pub const WEBHOOK: &str = "webhook";

/// Result recorded for a tool call the process died in the middle of.
pub const INTERRUPTED: &str = "interrupted: Junees stopped before this tool finished";

/// What a tool result says once its output was cleared to save context.
pub const CLEARED: &str = "[tool output cleared to save context]";

/// What takes the place of the images of a message the API rejected
/// ([`Entry::ClearImages`]).
pub const REJECTED_IMAGES: &str =
    "[image removed: the API rejected the request that carried it, so it is not sent again]";

/// Tool result messages whose output is never cleared: the most recent ones.
pub const KEEP_TOOL_RESULTS: usize = 3;

/// Starts the text block that carries a compacted conversation's summary.
pub const SUMMARY_HEADER: &str = "[Summary of the earlier conversation]\n";

/// User inputs a compaction keeps whole when it can.
const KEEP_INPUTS: usize = 2;

/// Tool rounds (a model reply and the results of its calls) a compaction
/// inside a single turn keeps whole when it can.
const KEEP_ROUNDS: usize = 2;

/// The first line of every journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Journal format version; files from before the field are format 1.
    #[serde(default = "first_format")]
    pub format: u32,
    /// Unix seconds.
    pub created_at: u64,
    pub cwd: String,
    pub model: String,
    pub version: String,
    /// What runs the conversation: [`CLAUDE_CODE`], or `None` (the field
    /// left out) for Junees's own agent on the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Where the conversation takes place when not at a terminal:
    /// [`DISCORD`] for a channel of the Discord gateway, which continues
    /// it there ([`TELEGRAM`] likewise), or [`CRON`] for a run of a
    /// scheduled job. `junees -c` and
    /// the `/sessions` list leave such sessions out. `None` (the field left
    /// out) for the terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The id of the channel a [`Meta::source`] session belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

fn first_format() -> u32 {
    1
}

/// The `type` tags of [`Entry`]; keep in step with its variants.
const ENTRY_TYPES: [&str; 13] = [
    "meta",
    "message",
    "append_text",
    "append_content",
    "usage",
    "clear_tool_results",
    "summary_usage",
    "compact",
    "format",
    "drop_server_calls",
    "claude_turn",
    "subagent_usage",
    "clear_images",
];

/// One line of a journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Meta(Meta),
    /// A message was added to the history.
    Message(Message),
    /// A text block was added to the trailing user message.
    AppendText {
        text: String,
    },
    /// Blocks were added to the trailing user message: an input with
    /// images ([`Transcript::user_input_entry`]).
    AppendContent {
        content: Vec<ContentBlock>,
    },
    /// Token usage of one model call.
    Usage(Usage),
    /// The output of every tool result in the messages before index `upto`
    /// was replaced with [`CLEARED`], and every thinking block was dropped
    /// (see [`Transcript::apply`]) unless `keep_thinking` is set.
    ClearToolResults {
        upto: usize,
        /// Set with manual extended thinking (see
        /// [`crate::provider::Provider::manual_thinking`]): the models that
        /// accept it do not check what came before a thinking block, and
        /// the turn in progress has to keep starting with one.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_thinking: bool,
    },
    /// Token usage of a call that summarized the conversation. It counts
    /// toward the session's usage but says nothing about the context size.
    SummaryUsage(Usage),
    /// The messages before index `keep_from` were replaced by `summary`.
    /// It leads the first kept message when that is a user input; when the
    /// tail starts with a model reply (a cut inside one turn), it goes into
    /// a new user message, followed by the text of the input that started
    /// the turn, word for word. Every kept thinking block was dropped
    /// unless `keep_thinking` is set (as for `clear_tool_results`).
    Compact {
        summary: String,
        keep_from: usize,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_thinking: bool,
    },
    /// The lines after it are in journal format `format`, newer than the
    /// meta line's: a later Junees continued the session. Versions that
    /// do not know the entry refuse the file with a clear error instead
    /// of misreading what follows.
    Format {
        format: u32,
    },
    /// The server tool calls (`server_tool_use`) with these ids were
    /// removed: they never got a result and the turn they were in ended,
    /// so the API would reject the history (see
    /// [`Transcript::open_server_calls`]). A model reply left with nothing
    /// but thinking goes with them.
    DropServerCalls {
        ids: Vec<String>,
    },
    /// Claude Code ran one turn of a [`CLAUDE_CODE`] conversation. Claude
    /// Code keeps the conversation itself; the journal keeps what is
    /// needed to continue it and a summary of the turn.
    ClaudeTurn(ClaudeTurn),
    /// Tokens the subagents of a `delegate` call used together
    /// ([`crate::subagent`]). They count toward the session's usage (and
    /// a task's budget) but say nothing about this conversation's context
    /// size; their own histories are in journals of their own.
    SubagentUsage(Usage),
    /// The images of the trailing user message were replaced by `note`:
    /// the API rejected the request that carried them (a file whose header
    /// looks right but whose data is broken, for one), so sending them
    /// again would fail the same way ([`Transcript::clear_images_entry`]).
    /// An input image becomes the note; a tool result loses its images and
    /// gets the note after its output.
    ClearImages {
        note: String,
    },
}

/// One turn of a conversation Claude Code runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeTurn {
    /// Claude Code's id of the conversation (`claude --resume <id>`), as
    /// the turn reported it; `None` when it ended before saying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session: Option<String>,
    /// What the user typed.
    pub input: String,
    /// Claude Code's final answer; empty when the turn ended without one.
    #[serde(default)]
    pub text: String,
    /// Tokens the turn used, as Claude Code reported them.
    pub usage: Usage,
    /// How the turn ended, in a few words (`answered`, `interrupted`,
    /// `stopped at the usage limit`, `failed`, ...).
    pub end: String,
}

/// An entry as a journal line stores it: stamped with when it was written.
#[derive(Serialize)]
struct LineOut<'a> {
    #[serde(flatten)]
    entry: &'a Entry,
    /// The model of the calls a usage entry counts ([`Journal::record_usage`]).
    /// An optional field, so older versions read such lines as before (they
    /// ignore it) and no format change is needed; entries without it count
    /// as the meta line's model.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    /// Unix seconds.
    ts: u64,
}

/// A journal line read back. Lines from before `ts` existed have none.
#[derive(Deserialize)]
struct LineIn {
    #[serde(flatten)]
    entry: Entry,
    #[serde(default)]
    #[allow(dead_code)] // Kept in the format for time budgets and status views.
    ts: Option<u64>,
}

/// Somewhere the agent records its history changes.
pub trait Journal: Send {
    fn record(&mut self, entry: &Entry) -> io::Result<()>;

    /// Records a usage entry (`usage`, `summary_usage`, `subagent_usage`)
    /// of calls made with `model`. Journals that keep no model record the
    /// entry alone.
    fn record_usage(&mut self, entry: &Entry, model: &str) -> io::Result<()> {
        let _ = model;
        self.record(entry)
    }

    /// The journal's file, if it is one: subagents keep theirs next to it
    /// ([`SessionWriter::create_sub`]).
    fn file(&self) -> Option<&Path> {
        None
    }
}

/// A conversation history and the tokens it has used so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub messages: Vec<Message>,
    pub usage: Usage,
    /// Size of the conversation as of the last model call that searched
    /// nothing and reported its usage (see [`Usage::context_tokens`]); 0
    /// until one is measured,
    /// and again after a compaction.
    pub context_tokens: u64,
    /// Estimated size of what the history gained since `context_tokens`
    /// was measured (see [`estimate_tokens`]): tool results, inputs, and
    /// the replies of calls that searched the web, which measure nothing.
    /// After a compaction, the estimate of the whole history.
    pub unmeasured_tokens: u64,
    /// The last call measured the size, which counts its reply already.
    pub(crate) reply_measured: bool,
    /// The Claude Code conversation a [`CLAUDE_CODE`] session continues:
    /// the latest id its turns reported.
    pub claude_session: Option<String>,
    /// Turns Claude Code ran in this session.
    pub claude_turns: usize,
}

impl Transcript {
    /// Applies one journal entry. `Meta` and `Format` entries change
    /// nothing.
    pub fn apply(&mut self, entry: Entry) {
        match entry {
            Entry::Meta(_) | Entry::Format { .. } => {}
            Entry::Message(message) => {
                if !(self.reply_measured && message.role == Role::Assistant) {
                    self.unmeasured_tokens += estimate_tokens(&message.content);
                }
                self.reply_measured = false;
                self.messages.push(message);
            }
            Entry::AppendText { text } => {
                let block = ContentBlock::text(text);
                self.unmeasured_tokens += estimate_tokens(std::slice::from_ref(&block));
                self.reply_measured = false;
                match self.messages.last_mut() {
                    Some(last) if last.role == Role::User => last.content.push(block),
                    _ => self.messages.push(Message {
                        role: Role::User,
                        content: vec![block],
                    }),
                }
            }
            Entry::AppendContent { content } => {
                self.unmeasured_tokens += estimate_tokens(&content);
                self.reply_measured = false;
                match self.messages.last_mut() {
                    Some(last) if last.role == Role::User => last.content.extend(content),
                    _ => self.messages.push(Message {
                        role: Role::User,
                        content,
                    }),
                }
            }
            Entry::Usage(usage) => {
                self.usage.add(usage);
                // A call that searched the web reports the cache reads of
                // every step of the API's own search loop, far more than
                // the conversation holds (context editing docs, "Server-side
                // tools"), so it says nothing about the size. Its reply,
                // search results included, is estimated instead, on top of
                // the last size measured, until a call without a search
                // measures it again. A call that reported no usage at all
                // (an OpenAI-compatible server that leaves it out, or a
                // stream cut off before its usage chunk) measured nothing
                // either: a size of 0 would keep compaction from starting.
                if usage.web_searches() == 0 && usage.context_tokens() > 0 {
                    self.context_tokens = usage.context_tokens();
                    self.unmeasured_tokens = 0;
                    self.reply_measured = true;
                } else {
                    self.reply_measured = false;
                }
            }
            Entry::ClearToolResults {
                upto,
                keep_thinking,
            } => {
                for message in self.messages.iter_mut().take(upto) {
                    for block in &mut message.content {
                        // Images a tool gave back go with its output.
                        if let ContentBlock::ToolResult {
                            content, images, ..
                        } = block
                        {
                            *content = CLEARED.to_string();
                            images.clear();
                        }
                    }
                }
                if !keep_thinking {
                    self.drop_thinking();
                }
            }
            Entry::SummaryUsage(usage) | Entry::SubagentUsage(usage) => self.usage.add(usage),
            Entry::Compact {
                summary,
                keep_from,
                keep_thinking,
            } => {
                let dropped: Vec<Message> = self
                    .messages
                    .drain(..keep_from.min(self.messages.len()))
                    .collect();
                let block = ContentBlock::text(format!("{SUMMARY_HEADER}{summary}"));
                match self.messages.first_mut() {
                    Some(first) if first.role == Role::User => first.content.insert(0, block),
                    _ => {
                        // The cut was inside the turn of the first message's
                        // input. That input is the task the turn works on,
                        // so it stays as typed rather than only summarized,
                        // with the images it came with.
                        let mut content = vec![block];
                        if let Some(input) = dropped.first() {
                            content.extend(
                                input
                                    .content
                                    .iter()
                                    .filter(|block| match block {
                                        ContentBlock::Text { text, .. } => {
                                            !text.starts_with(SUMMARY_HEADER)
                                        }
                                        ContentBlock::Image(_) => true,
                                        _ => false,
                                    })
                                    .cloned(),
                            );
                        }
                        self.messages.insert(
                            0,
                            Message {
                                role: Role::User,
                                content,
                            },
                        );
                    }
                }
                if !keep_thinking {
                    self.drop_thinking();
                }
                // Unknown until the next model call measures it; estimated
                // until then.
                self.context_tokens = 0;
                self.unmeasured_tokens = self
                    .messages
                    .iter()
                    .map(|message| estimate_tokens(&message.content))
                    .sum();
                self.reply_measured = false;
            }
            // Claude Code manages its own context: nothing to measure.
            Entry::ClaudeTurn(turn) => {
                self.usage.add(turn.usage);
                self.claude_turns += 1;
                if turn.claude_session.is_some() {
                    self.claude_session = turn.claude_session;
                }
            }
            Entry::ClearImages { note } => {
                let Some(last) = self.messages.last_mut() else {
                    return;
                };
                if last.role != Role::User {
                    return;
                }
                let before = estimate_tokens(&last.content);
                let mut content = Vec::with_capacity(last.content.len());
                let mut noted = false;
                for mut block in std::mem::take(&mut last.content) {
                    match &mut block {
                        // All of the message's input images become one note.
                        ContentBlock::Image(_) => {
                            if !noted {
                                content.push(ContentBlock::text(note.clone()));
                                noted = true;
                            }
                            continue;
                        }
                        ContentBlock::ToolResult {
                            content: output,
                            images,
                            ..
                        } if !images.is_empty() => {
                            images.clear();
                            output.push_str(&format!("\n{note}"));
                        }
                        _ => {}
                    }
                    content.push(block);
                }
                last.content = content;
                let after = estimate_tokens(&last.content);
                self.unmeasured_tokens = (self.unmeasured_tokens + after).saturating_sub(before);
                self.reply_measured = false;
            }
            Entry::DropServerCalls { ids } => {
                let dropped = |block: &ContentBlock| {
                    matches!(block.other_type(), Some(kind) if SERVER_CALL_TYPES.contains(&kind))
                        && matches!(block, ContentBlock::Other(value)
                            if value["id"].as_str().is_some_and(|id| ids.iter().any(|d| d == id)))
                };
                let mut i = 0;
                while i < self.messages.len() {
                    let message = &mut self.messages[i];
                    if !message.content.iter().any(dropped) {
                        i += 1;
                        continue;
                    }
                    message.content.retain(|block| !dropped(block));
                    if message.content.iter().any(|block| !block.is_thinking()) {
                        i += 1;
                        continue;
                    }
                    // Nothing left worth sending: the reply goes, and the
                    // user messages around it become one.
                    self.messages.remove(i);
                    if i > 0
                        && i < self.messages.len()
                        && self.messages[i - 1].role == Role::User
                        && self.messages[i].role == Role::User
                    {
                        let next = self.messages.remove(i);
                        self.messages[i - 1].content.extend(next.content);
                    }
                }
            }
        }
    }

    /// The ids of the server tool calls that have no result anywhere in
    /// the history. A reply paused by the API (`pause_turn`) can end with
    /// one, and so can a reply that also called client tools: the API runs
    /// the search on the next request, when it carries nothing but those
    /// tools' results. Once a turn ends there instead (the pause limit, a
    /// cancel, an error), a new user input after it makes the API reject
    /// the request ("`web_search` tool use with id ... was found without a
    /// corresponding `web_search_tool_result` block"; handling stop
    /// reasons docs, "tool_use"), so these calls are dropped first.
    pub fn open_server_calls(&self) -> Vec<String> {
        let blocks = || self.messages.iter().flat_map(|message| &message.content);
        let answered: Vec<&str> = blocks()
            .filter_map(|block| match block {
                ContentBlock::Other(value) => value["tool_use_id"].as_str(),
                _ => None,
            })
            .collect();
        blocks()
            .filter_map(|block| match block {
                ContentBlock::Other(value)
                    if block
                        .other_type()
                        .is_some_and(|kind| SERVER_CALL_TYPES.contains(&kind)) =>
                {
                    value["id"].as_str()
                }
                _ => None,
            })
            .filter(|id| !answered.contains(id))
            .map(str::to_string)
            .collect()
    }

    /// The entries that drop the server tool calls left without a result
    /// (see [`Transcript::open_server_calls`]); empty when there are none.
    ///
    /// Such a call is normally in the last model reply, after its thinking
    /// blocks, so dropping it changes nothing a thinking block was made
    /// after. One in an earlier message (a history that went on past it,
    /// as an older Junees could leave) is followed by later replies, whose
    /// thinking blocks Claude 5.1 and later models would then reject: they
    /// check that nothing before a thinking block changed. Then a second
    /// entry, `clear_tool_results` with `upto: 0`, drops every thinking
    /// block and clears no tool output: an entry Junees reads since format
    /// 2, so the journal format stays. With `manual_thinking` the models do
    /// not check this, so it is left out.
    pub fn drop_server_calls_entries(&self, manual_thinking: bool) -> Vec<Entry> {
        let ids = self.open_server_calls();
        if ids.is_empty() {
            return Vec::new();
        }
        let last_reply = self
            .messages
            .iter()
            .rposition(|message| message.role == Role::Assistant);
        let earlier = self.messages.iter().enumerate().any(|(i, message)| {
            Some(i) != last_reply
                && message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Other(value)
                        if value["id"].as_str().is_some_and(|id| ids.iter().any(|d| d == id)))
                })
        });
        let thinking = self
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(ContentBlock::is_thinking);
        let mut entries = vec![Entry::DropServerCalls { ids }];
        if earlier && thinking && !manual_thinking {
            entries.push(Entry::ClearToolResults {
                upto: 0,
                keep_thinking: false,
            });
        }
        entries
    }

    /// Estimated size of the conversation: the last measured size plus the
    /// estimate of what came after it.
    pub fn context_size(&self) -> u64 {
        self.context_tokens + self.unmeasured_tokens
    }

    /// Drops every `thinking` and `redacted_thinking` block, after a change
    /// to earlier messages. Claude 5.1 and later models check that nothing
    /// before a thinking block sent back (system prompt, tools, earlier
    /// messages) changed since the block was made, and reject it, and every
    /// later one, if it did. Without thinking blocks there is nothing to
    /// check; the ones made from here on see the changed history. Replies
    /// keep their text and tool calls, so none becomes empty: a reply of
    /// nothing but thinking is never stored.
    fn drop_thinking(&mut self) {
        for message in &mut self.messages {
            message.content.retain(|block| !block.is_thinking());
        }
    }

    /// Where a compaction should cut the history: the index of the first
    /// kept message, or `None` when there is nowhere to cut.
    ///
    /// Cuts before a user input come first: the tail starts at a user
    /// message that carries typed input and no tool results, right after a
    /// message without tool calls, so no tool call is cut off from its
    /// result. It keeps the last two inputs and everything after them when
    /// it can, else the last one.
    ///
    /// When the first message is the only input (one long turn, as in a
    /// one-shot task), the cut goes inside that turn instead: at a model
    /// reply that directly follows a message holding nothing but tool
    /// results, so every earlier tool call has its result. It keeps the
    /// last two such rounds when it can, else the last one.
    ///
    /// A server tool called next to client tools (web search in the same
    /// reply as bash) runs on the request after their results, so its
    /// result block arrives in the next reply: no cut falls between the two.
    ///
    /// With `manual_thinking` the API wants the turn in progress to start
    /// with a thinking block, and only the turn's first reply has one, so a
    /// cut inside the turn keeps none of it: where the cut above would go,
    /// everything is summarized, and the turn starts again from the summary
    /// and its input.
    pub fn compact_point(&self, manual_thinking: bool) -> Option<usize> {
        let inputs: Vec<usize> = (1..self.messages.len())
            .filter(|&i| {
                let message = &self.messages[i];
                message.role == Role::User
                    && message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::Text { .. }))
                    && !has_tool_result(message)
                    && !self.messages[i - 1]
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
            })
            .collect();
        if !inputs.is_empty() {
            return inputs
                .iter()
                .rev()
                .nth(KEEP_INPUTS - 1)
                .or(inputs.last())
                .copied();
        }
        let rounds: Vec<usize> = (2..self.messages.len())
            .filter(|&i| {
                self.messages[i].role == Role::Assistant
                    && only_tool_results(&self.messages[i - 1])
                    && !orphans_server_tool_result(&self.messages[i..])
            })
            .collect();
        if manual_thinking {
            return rounds.last().map(|_| self.messages.len());
        }
        rounds
            .iter()
            .rev()
            .nth(KEEP_ROUNDS - 1)
            .or(rounds.last())
            .copied()
    }

    /// The entry that clears the output of every tool result except those
    /// in the last [`KEEP_TOOL_RESULTS`] messages that carry tool results,
    /// or `None` when there is nothing left to clear. The blocks stay, with
    /// their `tool_use_id` and `is_error`, so every tool call keeps its
    /// result. With `manual_thinking` the entry keeps thinking blocks.
    pub fn clear_entry(&self, manual_thinking: bool) -> Option<Entry> {
        self.clear_entry_keeping(KEEP_TOOL_RESULTS, manual_thinking)
    }

    /// Like [`Transcript::clear_entry`], but keeps the output of the last
    /// `keep` messages with tool results; 0 clears every tool result.
    pub fn clear_entry_keeping(&self, keep: usize, manual_thinking: bool) -> Option<Entry> {
        let upto = match keep.checked_sub(1) {
            None => self.messages.len(),
            Some(skip) => self
                .messages
                .iter()
                .enumerate()
                .filter(|(_, message)| has_tool_result(message))
                .map(|(i, _)| i)
                .rev()
                .nth(skip)?,
        };
        let uncleared = self.messages[..upto].iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, ContentBlock::ToolResult { content, .. } if content != CLEARED),
            )
        });
        uncleared.then_some(Entry::ClearToolResults {
            upto,
            keep_thinking: manual_thinking,
        })
    }

    /// The entry that adds user input: it joins a trailing user message (left
    /// behind by a failed turn) or starts a new one.
    pub fn user_text_entry(&self, text: &str) -> Entry {
        match self.messages.last() {
            Some(last) if last.role == Role::User => Entry::AppendText {
                text: text.to_string(),
            },
            _ => Entry::Message(Message::user_text(text)),
        }
    }

    /// The entry that adds user input with `images` before its text, as the
    /// vision docs advise ("Claude works best when images come before
    /// text"); without images, [`Transcript::user_text_entry`].
    pub fn user_input_entry(&self, text: &str, images: Vec<crate::images::Image>) -> Entry {
        if images.is_empty() {
            return self.user_text_entry(text);
        }
        let mut content: Vec<ContentBlock> = images.into_iter().map(ContentBlock::Image).collect();
        content.push(ContentBlock::text(text));
        match self.messages.last() {
            Some(last) if last.role == Role::User => Entry::AppendContent { content },
            _ => Entry::Message(Message {
                role: Role::User,
                content,
            }),
        }
    }

    /// The entry that takes the images out of the trailing user message,
    /// or `None` when the history does not end with a user message holding
    /// images. Called after the API rejected a request because of an image
    /// ([`crate::provider::ProviderError::is_image_rejected`]): a failed
    /// turn leaves its input as the last message and the next input joins
    /// it, so without this the rejected image would go with every later
    /// request, and compaction keeps the latest inputs whole. Only the
    /// trailing message changes, so no thinking block sees an edited
    /// prefix; images the API took in earlier turns stay.
    pub fn clear_images_entry(&self) -> Option<Entry> {
        let last = self.messages.last()?;
        (last.role == Role::User && last.content.iter().any(|block| !block.images().is_empty()))
            .then(|| Entry::ClearImages {
                note: REJECTED_IMAGES.to_string(),
            })
    }

    /// If the history ends with tool calls that never got a result (the
    /// process stopped while they ran), the user message that answers each
    /// of them with an error.
    pub fn interrupted_results(&self) -> Option<Message> {
        let last = self.messages.last()?;
        if last.role != Role::Assistant {
            return None;
        }
        let results: Vec<ContentBlock> = last
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => {
                    Some(ContentBlock::tool_result(id.clone(), INTERRUPTED, true))
                }
                _ => None,
            })
            .collect();
        (!results.is_empty()).then_some(Message {
            role: Role::User,
            content: results,
        })
    }
}

/// Block types of calls to tools the API runs itself.
const SERVER_CALL_TYPES: [&str; 2] = ["server_tool_use", "mcp_tool_use"];

/// A rough token count of content blocks: [`estimate_text_tokens`] of
/// their JSON, but images by their size in pixels
/// ([`crate::images::Image::estimate_tokens`]), not by their base64.
pub fn estimate_tokens(content: &[ContentBlock]) -> u64 {
    if content.iter().all(|block| block.images().is_empty()) {
        return json_tokens(content);
    }
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Image(image) => image.estimate_tokens(),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
            } => {
                json_tokens(&ContentBlock::tool_result(
                    tool_use_id.as_str(),
                    content.as_str(),
                    *is_error,
                )) + images
                    .iter()
                    .map(|image| image.estimate_tokens())
                    .sum::<u64>()
            }
            block => json_tokens(block),
        })
        .sum()
}

/// [`estimate_text_tokens`] of the JSON of `value`.
fn json_tokens<T: Serialize + ?Sized>(value: &T) -> u64 {
    serde_json::to_string(value).map_or(0, |json| estimate_text_tokens(&json))
}

/// A rough token count of `text`, counted per character: a quarter token
/// for each ASCII character (4 characters a token), a whole token for each
/// Hangul, CJK ideograph or kana character (see [`is_wide`]), and for any
/// other character a quarter token per byte of its UTF-8 (2 to 4 bytes:
/// accented Latin, Greek and Cyrillic come to 2 characters a token).
///
/// A token per 4 bytes undercounts Korean: a Hangul syllable is 3 bytes
/// but takes about a token or more. English and other ASCII text estimate
/// exactly as by bytes, and no text estimates lower than by bytes. Keys and
/// quotes make the JSON of blocks err high, and so do the base64
/// `encrypted_content` of web search results. Erring high only compacts a
/// little early.
pub fn estimate_text_tokens(text: &str) -> u64 {
    let quarters: usize = text
        .chars()
        .map(|c| if is_wide(c) { 4 } else { c.len_utf8() })
        .sum();
    quarters.div_ceil(4) as u64
}

/// Hangul (syllables and jamo), CJK ideographs, radicals, symbols and
/// punctuation, kana, bopomofo, and full-width forms: scripts whose
/// characters take about a token each.
fn is_wide(c: char) -> bool {
    matches!(c,
        '\u{1100}'..='\u{11FF}'
        | '\u{2E80}'..='\u{9FFF}'
        | '\u{A960}'..='\u{A97F}'
        | '\u{AC00}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}'
        | '\u{20000}'..='\u{3FFFF}')
}

fn has_tool_result(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Whether `kept` holds a server tool result (`web_search_tool_result` and
/// the like) whose `server_tool_use` call is not in `kept` too.
fn orphans_server_tool_result(kept: &[Message]) -> bool {
    let blocks = || kept.iter().flat_map(|message| &message.content);
    let calls: Vec<&str> = blocks()
        .filter_map(|block| match block {
            ContentBlock::Other(value) if value["type"] == "server_tool_use" => {
                value["id"].as_str()
            }
            _ => None,
        })
        .collect();
    blocks().any(|block| match block {
        ContentBlock::Other(value) => {
            value["type"]
                .as_str()
                .is_some_and(|kind| kind.ends_with("_tool_result"))
                && value["tool_use_id"]
                    .as_str()
                    .is_some_and(|id| !calls.contains(&id))
        }
        _ => false,
    })
}

/// A user message that answers tool calls and says nothing else.
fn only_tool_results(message: &Message) -> bool {
    message.role == Role::User
        && !message.content.is_empty()
        && message
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// Appends entries to a journal file, one JSON line per entry, each written
/// with a single `write_all` on an unbuffered file.
///
/// A new session's file is only created with its first entry, so starting
/// Junees and quitting right away leaves no empty session behind for
/// `--continue` to pick.
///
/// While a writer has its file open it holds an exclusive `flock` on it, so
/// a second Junees cannot resume (and repair) a session that is still live.
pub struct SessionWriter {
    path: PathBuf,
    meta: Meta,
    file: Option<File>,
}

impl SessionWriter {
    /// Picks an id for a new session under `home/sessions`. Nothing is
    /// written until the first entry.
    pub fn create(home: &Path, cwd: &Path, model: &str) -> Result<(Meta, Self)> {
        Self::create_for(home, cwd, model, None)
    }

    /// [`SessionWriter::create`] for a conversation run by `backend`
    /// ([`Meta::backend`]).
    pub fn create_for(
        home: &Path,
        cwd: &Path,
        model: &str,
        backend: Option<&str>,
    ) -> Result<(Meta, Self)> {
        Self::create_with(home, cwd, model, backend, None)
    }

    /// [`SessionWriter::create_for`] for a conversation in `channel` of
    /// `source` ([`Meta::source`]), such as a Discord channel.
    pub fn create_for_channel(
        home: &Path,
        cwd: &Path,
        model: &str,
        backend: Option<&str>,
        source: &str,
        channel: &str,
    ) -> Result<(Meta, Self)> {
        Self::create_with(home, cwd, model, backend, Some((source, channel)))
    }

    fn create_with(
        home: &Path,
        cwd: &Path,
        model: &str,
        backend: Option<&str>,
        channel: Option<(&str, &str)>,
    ) -> Result<(Meta, Self)> {
        let dir = sessions_dir(home);
        let now = unix_now();
        for _ in 0..16 {
            let id = new_id(now);
            let path = dir.join(format!("{id}.jsonl"));
            if path.exists() {
                continue;
            }
            let meta = Meta {
                id,
                format: FORMAT,
                created_at: now,
                cwd: cwd.to_string_lossy().into_owned(),
                model: model.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                backend: backend.map(str::to_string),
                source: channel.map(|(source, _)| source.to_string()),
                channel: channel.map(|(_, channel)| channel.to_string()),
            };
            let writer = Self {
                path,
                meta: meta.clone(),
                file: None,
            };
            return Ok((meta, writer));
        }
        bail!("could not pick an unused session id in {}", dir.display())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The journal of a subagent of the session recorded at `parent`:
    /// `<parent id>/<name>.jsonl` next to it, with the parent's folder and
    /// model in its meta line. Like any session it is only created with its
    /// first entry, in a 0700 folder as a 0600 file.
    pub fn create_sub(parent: &Path, name: &str) -> Result<Self> {
        let meta = read_meta(parent)
            .with_context(|| format!("cannot read the meta line of {}", parent.display()))?;
        let path = parent.with_extension("").join(format!("{name}.jsonl"));
        if path.exists() {
            bail!("{} already exists", path.display());
        }
        let meta = Meta {
            id: format!("{}/{name}", meta.id),
            format: FORMAT,
            created_at: unix_now(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            backend: None,
            ..meta
        };
        Ok(Self {
            path,
            meta,
            file: None,
        })
    }

    /// Creates the file, locks it and writes the meta line. Journals hold
    /// every tool output (files read, command output), so the directories
    /// it creates are 0700 and the file is 0600 whatever the umask.
    fn start(&self) -> io::Result<File> {
        if let Some(dir) = self.path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let mut file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.path)?;
        lock(&file, &self.path)?;
        write_line(&mut file, &Entry::Meta(self.meta.clone()), None)?;
        Ok(file)
    }
}

impl SessionWriter {
    fn write(&mut self, entry: &Entry, model: Option<&str>) -> io::Result<()> {
        let mut file = match self.file.take() {
            Some(file) => file,
            None => self.start()?,
        };
        let mut result = Ok(());
        // A journal from an older version is continued in this one's
        // format; say so before the first line that may use it.
        if self.meta.format < FORMAT {
            result = write_line(&mut file, &Entry::Format { format: FORMAT }, None);
            if result.is_ok() {
                self.meta.format = FORMAT;
            }
        }
        // Images go to files of the session's folder; the line names them.
        let result = result
            .and_then(|()| crate::images::store::stored_entry(entry, &self.images_dir()))
            .and_then(|stored| write_line(&mut file, stored.as_ref().unwrap_or(entry), model));
        self.file = Some(file);
        result
    }

    /// Where the images of this journal's entries are kept.
    fn images_dir(&self) -> PathBuf {
        crate::images::store::dir_of(&self.path)
    }
}

impl Journal for SessionWriter {
    fn record(&mut self, entry: &Entry) -> io::Result<()> {
        self.write(entry, None)
    }

    fn record_usage(&mut self, entry: &Entry, model: &str) -> io::Result<()> {
        self.write(entry, Some(model))
    }

    fn file(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

/// Takes an exclusive `flock` on a journal without blocking. The kernel
/// drops it when the file is closed or the process dies, so a lock that is
/// held means the session's writer is still running. A lock that looks
/// taken is tried again for a moment, because a child another thread forks
/// shares a just-released lock until it execs ([`crate::flock`]).
fn lock(file: &File, path: &Path) -> io::Result<()> {
    if crate::flock::try_exclusive(file)? {
        return Ok(());
    }
    let id = path.file_stem().unwrap_or_default().to_string_lossy();
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("session {id} is in use by another Junees"),
    ))
}

fn write_line(file: &mut File, entry: &Entry, model: Option<&str>) -> io::Result<()> {
    let mut line = serde_json::to_vec(&LineOut {
        entry,
        model,
        ts: unix_now(),
    })?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.flush()
}

/// A journal read back from disk.
#[derive(Debug)]
pub struct Session {
    pub path: PathBuf,
    pub meta: Meta,
    pub transcript: Transcript,
    /// Set when a cut-off last line was dropped.
    pub warning: Option<String>,
    /// Bytes up to the end of the last good line.
    valid_len: u64,
    /// The last good line has no trailing newline.
    unterminated: bool,
}

impl Session {
    /// Reads and replays a journal. A last line that lacks its newline and
    /// does not parse is dropped with a warning: the process died while
    /// writing it. Any other line that does not parse is an error, because
    /// [`write_line`] writes a line and its newline at once, so a complete
    /// line is corrupt or from a format this version does not know.
    pub fn load(path: &Path) -> Result<Self> {
        let data = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut entries = Vec::new();
        let mut warning = None;
        let mut valid_len = 0;
        let mut pos = 0;
        let mut line_no = 0;
        while pos < data.len() {
            line_no += 1;
            let (line, next, terminated) = match data[pos..].iter().position(|&b| b == b'\n') {
                Some(n) => (&data[pos..pos + n], pos + n + 1, true),
                None => (&data[pos..], data.len(), false),
            };
            if line.trim_ascii().is_empty() {
                pos = next;
                continue;
            }
            match serde_json::from_slice::<LineIn>(line).map(|line| line.entry) {
                Ok(entry) => {
                    let format = match &entry {
                        Entry::Meta(meta) => meta.format,
                        Entry::Format { format } => *format,
                        _ => 0,
                    };
                    if format > FORMAT {
                        bail!(
                            "{} was written by a newer Junees (session format {format}, \
                             this version reads up to {FORMAT}); update Junees to open it",
                            path.display(),
                        );
                    }
                    entries.push(entry);
                    valid_len = next;
                }
                Err(err) if !terminated => {
                    warning = Some(format!(
                        "{}: dropped an unreadable last line ({err})",
                        path.display()
                    ));
                    break;
                }
                Err(err) => match unknown_type(line) {
                    // A later Junees may have continued an older session
                    // without changing its meta line.
                    Some(kind) => bail!(
                        "{}: line {line_no} is a `{kind}` entry, which this version \
                         does not know: the session was probably continued by a newer \
                         Junees; update Junees to open it",
                        path.display()
                    ),
                    None => bail!("{}: line {line_no} is corrupt: {err}", path.display()),
                },
            }
            pos = next;
        }

        let mut entries = entries.into_iter();
        let Some(Entry::Meta(mut meta)) = entries.next() else {
            bail!("{} is not a Junees session (no meta line)", path.display());
        };
        let mut transcript = Transcript::default();
        let images = crate::images::store::dir_of(path);
        for entry in entries {
            match entry {
                Entry::Meta(_) => bail!("{}: unexpected second meta line", path.display()),
                // The format the journal is in from here on.
                Entry::Format { format } => meta.format = meta.format.max(format),
                _ => {}
            }
            transcript.apply(crate::images::store::resolve_entry(entry, &images));
        }
        Ok(Self {
            path: path.to_path_buf(),
            meta,
            transcript,
            warning,
            valid_len: valid_len as u64,
            unterminated: valid_len > 0 && data[valid_len - 1] != b'\n',
        })
    }

    /// Loads a journal to continue it: drops a cut-off last line from the
    /// file, answers tool calls that never finished (and records that), and
    /// returns a writer that appends to the file.
    ///
    /// The file is locked before it is read or changed, so a session another
    /// Junees still has open is refused instead of repaired under it.
    pub fn resume(path: &Path) -> Result<(Self, SessionWriter)> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        lock(&file, path)?;
        let mut session = Self::load(path)?;
        file.set_len(session.valid_len)?;
        if session.unterminated {
            file.write_all(b"\n")?;
        }
        let mut writer = SessionWriter {
            path: path.to_path_buf(),
            meta: session.meta.clone(),
            file: Some(file),
        };
        if let Some(results) = session.transcript.interrupted_results() {
            let entry = Entry::Message(results);
            writer
                .record(&entry)
                .with_context(|| format!("cannot write {}", path.display()))?;
            session.transcript.apply(entry);
        }
        Ok((session, writer))
    }
}

/// The `type` of a JSON object line, if it is not one this version knows.
fn unknown_type(line: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let kind = value.get("type")?.as_str()?;
    (!ENTRY_TYPES.contains(&kind)).then(|| kind.to_string())
}

pub fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// What `/sessions` shows about one session.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub id: String,
    /// Last write, in Unix seconds.
    pub updated: u64,
    pub cwd: String,
    /// Text of the first user message.
    pub first_input: String,
    /// [`Meta::backend`].
    pub backend: Option<String>,
    /// [`Meta::source`].
    pub source: Option<String>,
    /// [`Meta::channel`].
    pub channel: Option<String>,
}

struct SessionFile {
    path: PathBuf,
    id: String,
    modified: SystemTime,
}

/// Journal files under `home/sessions`, most recently written first.
fn session_files(home: &Path) -> Result<Vec<SessionFile>> {
    let dir = sessions_dir(home);
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in read {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let id = id.to_string();
        let Ok(modified) = fs::metadata(&path).and_then(|meta| meta.modified()) else {
            continue;
        };
        files.push(SessionFile { path, id, modified });
    }
    files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
    Ok(files)
}

/// Reads one line of a journal as an entry.
fn read_entry(reader: &mut impl io::BufRead) -> Option<Entry> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    Some(serde_json::from_str::<LineIn>(&line).ok()?.entry)
}

/// The meta line of a journal, if its first line is one, and a reader
/// positioned after it.
fn read_head(path: &Path) -> Option<(Meta, io::BufReader<File>)> {
    let mut reader = io::BufReader::new(File::open(path).ok()?);
    match read_entry(&mut reader)? {
        Entry::Meta(meta) => Some((meta, reader)),
        _ => None,
    }
}

/// The meta line of a journal, if its first line is one.
pub fn read_meta(path: &Path) -> Option<Meta> {
    read_head(path).map(|(meta, _)| meta)
}

/// The most recently written session started at the terminal in `cwd`
/// and run by `backend` ([`Meta::backend`]). Sessions of a channel
/// ([`Meta::source`]) are left to the gateway that continues them.
pub fn find_latest(home: &Path, cwd: &Path, backend: Option<&str>) -> Result<Option<PathBuf>> {
    let cwd = cwd.to_string_lossy();
    Ok(session_files(home)?
        .into_iter()
        .find(|file| {
            read_meta(&file.path).is_some_and(|meta| {
                meta.cwd == cwd && meta.backend.as_deref() == backend && meta.source.is_none()
            })
        })
        .map(|file| file.path))
}

/// The session whose id is `prefix`, or the only one whose id starts with it.
pub fn find_by_prefix(home: &Path, prefix: &str) -> Result<PathBuf> {
    if prefix.is_empty() {
        bail!("the session id is empty");
    }
    let mut matches: Vec<SessionFile> = session_files(home)?
        .into_iter()
        .filter(|file| file.id.starts_with(prefix))
        .collect();
    if let Some(exact) = matches.iter().position(|file| file.id == prefix) {
        return Ok(matches.swap_remove(exact).path);
    }
    match matches.len() {
        0 => bail!(
            "no session id starts with `{prefix}` in {}",
            sessions_dir(home).display()
        ),
        1 => Ok(matches.remove(0).path),
        n => {
            let mut ids: Vec<&str> = matches.iter().map(|file| file.id.as_str()).collect();
            ids.sort_unstable();
            bail!("`{prefix}` matches {n} sessions: {}", ids.join(", "))
        }
    }
}

/// Up to `limit` sessions started at the terminal with a readable meta
/// line, most recently written first. Only the meta line and the first
/// entry (the first user input) of each file are read, so long sessions do
/// not slow the list down.
pub fn list_recent(home: &Path, limit: usize) -> Result<Vec<Summary>> {
    list(home, limit, Some(false))
}

/// [`list_recent`] for the sessions of channels ([`Meta::source`]).
pub fn list_channel_sessions(home: &Path, limit: usize) -> Result<Vec<Summary>> {
    list(home, limit, Some(true))
}

/// [`list_recent`] for every session: of the terminal and of channels.
pub fn list_all(home: &Path, limit: usize) -> Result<Vec<Summary>> {
    list(home, limit, None)
}

/// `channels`: `Some(true)` for the sessions of channels only,
/// `Some(false)` for those of the terminal only, `None` for both.
fn list(home: &Path, limit: usize, channels: Option<bool>) -> Result<Vec<Summary>> {
    Ok(session_files(home)?
        .into_iter()
        .filter_map(|file| {
            let (meta, mut rest) = read_head(&file.path)?;
            if channels.is_some_and(|channels| meta.source.is_some() != channels) {
                return None;
            }
            let first_input = match read_entry(&mut rest) {
                Some(Entry::Message(message)) if message.role == Role::User => message.text(),
                Some(Entry::ClaudeTurn(turn)) => turn.input,
                _ => String::new(),
            };
            Some(Summary {
                id: meta.id,
                updated: file
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                cwd: meta.cwd,
                first_input,
                backend: meta.backend,
                source: meta.source,
                channel: meta.channel,
            })
        })
        .take(limit)
        .collect())
}

/// The path of the journal of session `id` in the state folder `home`.
pub fn journal_path(home: &Path, id: &str) -> PathBuf {
    sessions_dir(home).join(format!("{id}.jsonl"))
}

/// How many messages the journal at `path` records: its user and
/// assistant messages, two for a Claude Code turn (the input and the
/// reply). Lines that do not read are skipped; the summary of a
/// compaction is not counted.
pub fn count_messages(path: &Path) -> Result<usize> {
    let file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut reader = io::BufReader::new(file);
    let mut count = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        match io::BufRead::read_line(&mut reader, &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        }
        match serde_json::from_str::<LineIn>(&line).map(|line| line.entry) {
            Ok(Entry::Message(_)) => count += 1,
            Ok(Entry::ClaudeTurn(_)) => count += 2,
            _ => {}
        }
    }
    Ok(count)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A session id that sorts by creation time: `YYYYMMDD-HHMMSS-xxxx` (UTC,
/// then 16 random bits).
fn new_id(unix_secs: u64) -> String {
    let (date, time) = utc_parts(unix_secs);
    // RandomState is seeded randomly per process and differs per instance.
    let random = RandomState::new().hash_one((unix_secs, std::process::id())) as u16;
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}-{random:04x}",
        date.0, date.1, date.2, time.0, time.1, time.2
    )
}

/// `YYYY-MM-DD HH:MM` in UTC.
pub fn format_utc(unix_secs: u64) -> String {
    let ((y, mo, d), (h, mi, _)) = utc_parts(unix_secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339 in UTC).
pub fn format_utc_seconds(unix_secs: u64) -> String {
    let ((y, mo, d), (h, mi, s)) = utc_parts(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

type Ymd = (i64, u32, u32);
type Hms = (u32, u32, u32);

fn utc_parts(unix_secs: u64) -> (Ymd, Hms) {
    let days = (unix_secs / 86_400) as i64;
    let secs = (unix_secs % 86_400) as u32;
    (
        civil_from_days(days),
        (secs / 3600, secs / 60 % 60, secs % 60),
    )
}

/// Gregorian date of a day count since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> Ymd {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("running"),
                ContentBlock::ToolUse {
                    id: id.into(),
                    name: "bash".into(),
                    input: json!({"command": "sleep 100"}),
                },
            ],
        }
    }

    fn write_lines(path: &Path, lines: &[String], tail: &str) {
        write_with_tail(path, lines, tail.as_bytes());
    }

    fn write_with_tail(path: &Path, lines: &[String], tail: &[u8]) {
        let mut data = lines.join("\n").into_bytes();
        data.push(b'\n');
        data.extend_from_slice(tail);
        fs::write(path, data).unwrap();
    }

    fn line(entry: &Entry) -> String {
        serde_json::to_string(entry).unwrap()
    }

    fn meta() -> Entry {
        Entry::Meta(Meta {
            id: "20261109-061704-3fa2".into(),
            format: FORMAT,
            created_at: 1_794_205_024,
            cwd: "/work".into(),
            model: "m".into(),
            version: "0.1.0".into(),
            backend: None,
            source: None,
            channel: None,
        })
    }

    #[test]
    fn entries_have_a_type_tag() {
        assert_eq!(
            serde_json::to_value(Entry::AppendText { text: "x".into() }).unwrap(),
            json!({"type": "append_text", "text": "x"})
        );
        assert_eq!(
            serde_json::to_value(Entry::Message(Message::user_text("hi"))).unwrap(),
            json!({"type": "message", "role": "user", "content": [{"type": "text", "text": "hi"}]})
        );
        assert_eq!(
            serde_json::to_value(Entry::Usage(Usage {
                input_tokens: 3,
                output_tokens: 1,
                ..Default::default()
            }))
            .unwrap(),
            json!({"type": "usage", "input_tokens": 3, "output_tokens": 1})
        );
        assert_eq!(serde_json::to_value(meta()).unwrap()["type"], "meta");
    }

    #[test]
    fn user_text_joins_a_trailing_user_message() {
        let mut transcript = Transcript::default();
        let entry = transcript.user_text_entry("first");
        assert!(matches!(entry, Entry::Message(_)));
        transcript.apply(entry);
        let entry = transcript.user_text_entry("second");
        assert!(matches!(entry, Entry::AppendText { .. }));
        transcript.apply(entry);
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text(), "first\nsecond");
    }

    fn picture(width: u32) -> crate::images::Image {
        crate::images::Image::from_bytes(&crate::images::testing::png(width, 10))
            .unwrap()
            .0
    }

    #[test]
    fn inputs_with_images_put_them_first() {
        let mut transcript = Transcript::default();
        let entry = transcript.user_input_entry("what is this?", vec![picture(1), picture(2)]);
        let Entry::Message(message) = &entry else {
            panic!("{entry:?}");
        };
        assert_eq!(
            message.content,
            [
                ContentBlock::Image(picture(1)),
                ContentBlock::Image(picture(2)),
                ContentBlock::text("what is this?")
            ]
        );
        transcript.apply(entry);
        // After a failed turn the input joins the trailing user message.
        let entry = transcript.user_input_entry("and this?", vec![picture(3)]);
        assert_eq!(
            entry,
            Entry::AppendContent {
                content: vec![
                    ContentBlock::Image(picture(3)),
                    ContentBlock::text("and this?")
                ]
            }
        );
        transcript.apply(entry);
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].content.len(), 5);
        assert_eq!(transcript.messages[0].text(), "what is this?\nand this?");
        // Without images, as before.
        assert!(matches!(
            transcript.user_input_entry("plain", Vec::new()),
            Entry::AppendText { .. }
        ));
        // A content entry with no user message to join starts one.
        let mut empty = Transcript::default();
        empty.apply(Entry::AppendContent {
            content: vec![ContentBlock::text("x")],
        });
        assert_eq!(empty.messages, [Message::user_text("x")]);
    }

    #[test]
    fn images_are_kept_as_files_and_come_back_on_resume() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let (meta, mut writer) =
            SessionWriter::create(home.path(), Path::new("/work"), "m").unwrap();
        let mut expected = Transcript::default();
        let input = expected.user_input_entry("look", vec![picture(5)]);
        let call = Entry::Message(tool_call("toolu_1"));
        let result = Entry::Message(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "Image a.png".into(),
                is_error: false,
                images: vec![picture(6), picture(5)],
            }],
        });
        for entry in [input, call, result] {
            writer.record(&entry).unwrap();
            expected.apply(entry);
        }
        drop(writer);

        let path = sessions_dir(home.path()).join(format!("{}.jsonl", meta.id));
        let journal = fs::read_to_string(&path).unwrap();
        assert!(!journal.contains("\"base64\""), "{journal}");
        assert_eq!(journal.matches("\"junees_file\"").count(), 3);
        assert!(
            journal.contains(&format!("\"format\":{FORMAT}")),
            "{journal}"
        );
        let images = sessions_dir(home.path()).join(&meta.id).join("images");
        let mut files: Vec<String> = fs::read_dir(&images)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files.len(), 2, "{files:?}");
        for file in &files {
            assert!(file.ends_with(".png") && file.len() == 64 + 4, "{file}");
            let mode = fs::metadata(images.join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let (session, writer) = Session::resume(&path).unwrap();
        assert_eq!(session.transcript, expected);
        assert_eq!(
            session.transcript.messages[0].content[0],
            ContentBlock::Image(picture(5))
        );
        drop(writer);

        // A deleted image file leaves a note in its place.
        for file in &files {
            fs::remove_file(images.join(file)).unwrap();
        }
        let session = Session::load(&path).unwrap();
        let first = &session.transcript.messages[0].content[0];
        assert!(
            matches!(first, ContentBlock::Text { text, .. } if text.contains("could not be read")),
            "{first:?}"
        );
        assert!(
            session.transcript.messages[2].content[0]
                .images()
                .is_empty()
        );
    }

    #[test]
    fn clearing_drops_the_images_of_cleared_results() {
        let mut transcript = tool_rounds(4);
        if let ContentBlock::ToolResult { images, .. } = &mut transcript.messages[2].content[0] {
            images.push(picture(3));
        }
        let entry = transcript.clear_entry(false).unwrap();
        transcript.apply(entry);
        assert_eq!(
            transcript.messages[2].content[0],
            ContentBlock::tool_result("toolu_0", CLEARED, true)
        );
    }

    #[test]
    fn compacting_inside_a_turn_keeps_the_input_images() {
        let mut transcript = tool_rounds(4);
        transcript.messages[0]
            .content
            .insert(0, ContentBlock::Image(picture(8)));
        let keep_from = transcript.compact_point(false).unwrap();
        transcript.apply(Entry::Compact {
            summary: "did 0 to 1".into(),
            keep_from,
            keep_thinking: false,
        });
        assert_eq!(
            transcript.messages[0].content,
            [
                ContentBlock::text(format!("{SUMMARY_HEADER}did 0 to 1")),
                ContentBlock::Image(picture(8)),
                ContentBlock::text("go"),
            ]
        );
    }

    #[test]
    fn images_are_estimated_by_their_size_not_their_base64() {
        let mut bytes = crate::images::testing::png(100, 100);
        // Megabytes of data a small picture can still have.
        bytes.resize(3 * 1024 * 1024, 0);
        let image = crate::images::Image::from_bytes(&bytes).unwrap().0;
        let content = [ContentBlock::Image(image.clone()), ContentBlock::text("hi")];
        let estimate = estimate_tokens(&content);
        // 4x4 patches, plus the JSON of the text block.
        assert!((16..40).contains(&estimate), "{estimate}");
        let result = [ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "Image".into(),
            is_error: false,
            images: vec![image],
        }];
        let estimate = estimate_tokens(&result);
        assert!((16..60).contains(&estimate), "{estimate}");
        // Text alone as before.
        let text = [ContentBlock::text("hello")];
        assert_eq!(
            estimate_tokens(&text),
            estimate_text_tokens(&serde_json::to_string(&text).unwrap())
        );
    }

    #[test]
    fn writes_and_loads_a_session() {
        let home = tempfile::tempdir().unwrap();
        let (meta, mut writer) =
            SessionWriter::create(home.path(), Path::new("/work"), "m").unwrap();
        let entries = [
            Entry::Message(Message::user_text("안녕")),
            Entry::Usage(Usage {
                input_tokens: 5,
                output_tokens: 2,
                ..Default::default()
            }),
            Entry::Message(tool_call("toolu_1")),
        ];
        let mut expected = Transcript::default();
        for entry in entries {
            writer.record(&entry).unwrap();
            expected.apply(entry);
        }

        let path = sessions_dir(home.path()).join(format!("{}.jsonl", meta.id));
        assert_eq!(writer.path(), path);
        let session = Session::load(&path).unwrap();
        assert_eq!(session.meta, meta);
        assert_eq!(session.meta.cwd, "/work");
        assert_eq!(session.transcript, expected);
        assert!(session.warning.is_none());
    }

    #[test]
    fn written_lines_carry_a_timestamp() {
        // The other tests write their fixtures without `ts`, as journals
        // from before the field were, and those still load.
        let home = tempfile::tempdir().unwrap();
        let (meta, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        writer
            .record(&Entry::Message(Message::user_text("hi")))
            .unwrap();
        let data = fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert!(line["ts"].as_u64().unwrap() >= meta.created_at, "{line}");
        }
        assert_eq!(lines[1]["type"], "message");
        let session = Session::load(writer.path()).unwrap();
        assert_eq!(session.transcript.messages, [Message::user_text("hi")]);
    }

    #[test]
    fn usage_lines_name_their_model_without_a_format_change() {
        let home = tempfile::tempdir().unwrap();
        let (_, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        let usage = Usage {
            input_tokens: 7,
            output_tokens: 2,
            ..Usage::default()
        };
        writer
            .record(&Entry::Message(Message::user_text("hi")))
            .unwrap();
        writer
            .record_usage(&Entry::Usage(usage), "claude-other")
            .unwrap();
        writer
            .record_usage(&Entry::SummaryUsage(usage), "claude-other")
            .unwrap();
        let data = fs::read_to_string(writer.path()).unwrap();
        let lines: Vec<serde_json::Value> = data
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["format"], FORMAT);
        assert_eq!(lines[1].get("model"), None, "{}", lines[1]);
        assert_eq!(lines[2]["type"], "usage");
        assert_eq!(lines[2]["model"], "claude-other");
        assert_eq!(lines[3]["model"], "claude-other");
        // Readers that do not know the field (this version's loader reads
        // lines like every earlier one) still replay the session.
        let session = Session::load(writer.path()).unwrap();
        assert!(session.warning.is_none());
        assert_eq!(session.transcript.usage.input_tokens, 14);
        assert_eq!(session.transcript.context_tokens, 9);
    }

    #[test]
    fn a_new_session_file_appears_with_its_first_entry() {
        let home = tempfile::tempdir().unwrap();
        let (_, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        assert!(!sessions_dir(home.path()).exists());
        writer
            .record(&Entry::Message(Message::user_text("hi")))
            .unwrap();
        let session = Session::load(writer.path()).unwrap();
        assert_eq!(session.transcript.messages, [Message::user_text("hi")]);
    }

    #[test]
    fn session_files_and_their_folders_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("junees");
        let (_, mut writer) = SessionWriter::create(&home, Path::new("/w"), "m").unwrap();
        writer
            .record(&Entry::Message(Message::user_text("cat .env")))
            .unwrap();
        for path in [home.as_path(), &sessions_dir(&home), writer.path()] {
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{} has mode {mode:o}", path.display());
        }
    }

    #[test]
    fn drops_a_cut_off_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        // Cut in the middle of a Hangul character.
        let full = line(&Entry::Message(Message::user_text("한국어 문장")));
        let cut = &full.as_bytes()[..full.len() - 5];
        assert!(std::str::from_utf8(cut).is_err());
        write_with_tail(&path, &[line(&meta()), line(&user)], cut);

        let session = Session::load(&path).unwrap();
        assert_eq!(session.transcript.messages, [Message::user_text("hi")]);
        assert!(session.warning.unwrap().contains("unreadable last line"));
    }

    #[test]
    fn a_broken_middle_line_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = line(&Entry::Message(Message::user_text("hi")));
        write_lines(&path, &[line(&meta()), "{\"type\":".into(), user], "");
        let err = Session::load(&path).unwrap_err();
        assert!(err.to_string().contains("line 2 is corrupt"), "{err}");
    }

    #[test]
    fn a_complete_last_line_that_does_not_parse_is_kept_and_refused() {
        // Newline included, so no crash cut it short: it may be an entry
        // from a newer Junees, and resume must not delete it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = line(&Entry::Message(Message::user_text("hi")));
        let future = r#"{"type":"from_the_future","x":1}"#;
        write_lines(&path, &[line(&meta()), user, future.into()], "");
        let before = fs::read(&path).unwrap();

        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("line 3 is a `from_the_future` entry"), "{err}");
        assert!(err.contains("continued by a newer Junees"), "{err}");
        assert!(Session::resume(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);

        // A known entry type that does not parse is plain corrupt.
        let broken = r#"{"type":"usage","input_tokens":"many"}"#;
        write_lines(&path, &[line(&meta()), broken.into()], "");
        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("line 2 is corrupt"), "{err}");
    }

    #[test]
    fn every_entry_type_is_known() {
        let samples = [
            meta(),
            Entry::Message(Message::user_text("hi")),
            Entry::AppendText { text: "x".into() },
            Entry::AppendContent {
                content: vec![ContentBlock::text("x")],
            },
            Entry::Usage(Usage::default()),
            Entry::ClearToolResults {
                upto: 1,
                keep_thinking: false,
            },
            Entry::SummaryUsage(Usage::default()),
            Entry::Compact {
                summary: "s".into(),
                keep_from: 1,
                keep_thinking: false,
            },
            Entry::Format { format: FORMAT },
            Entry::DropServerCalls { ids: vec![] },
            Entry::ClaudeTurn(claude_turn(Some("c-1"), "hi")),
            Entry::SubagentUsage(Usage::default()),
            Entry::ClearImages { note: "n".into() },
        ];
        let kinds: Vec<String> = samples
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap()["type"].to_string())
            .collect();
        let known: Vec<String> = ENTRY_TYPES
            .iter()
            .map(|kind| format!("\"{kind}\""))
            .collect();
        assert_eq!(kinds, known);
    }

    #[test]
    fn meta_lines_carry_the_format_version() {
        let value = serde_json::to_value(meta()).unwrap();
        assert_eq!(value["format"], 8);
        // An API conversation has no backend field.
        assert!(value.get("backend").is_none(), "{value}");
        // Journals written before the field existed are format 1.
        let old =
            r#"{"type":"meta","id":"a","created_at":1,"cwd":"/w","model":"m","version":"0.1.0"}"#;
        let Entry::Meta(old) = serde_json::from_str(old).unwrap() else {
            panic!("not a meta line");
        };
        assert_eq!(old.format, 1);
    }

    #[test]
    fn refuses_a_session_from_a_newer_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut newer = serde_json::to_value(meta()).unwrap();
        newer["format"] = json!(FORMAT + 1);
        let future = r#"{"type":"from_the_future","x":1}"#;
        write_lines(&path, &[newer.to_string(), future.into()], "");
        let before = fs::read(&path).unwrap();

        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("written by a newer Junees"), "{err}");
        assert!(
            err.contains(&format!("session format {}", FORMAT + 1)),
            "{err}"
        );
        assert!(Session::resume(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn loads_a_format_1_session() {
        // As L1 wrote them: format 1, no cache counts, no `ts`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"meta","id":"a","format":1,"created_at":1,"cwd":"/w","model":"m","version":"0.1.0"}"#,
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"hi"}]}"#,
            r#"{"type":"usage","input_tokens":30,"output_tokens":4}"#,
            r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"yo"}]}"#,
        ]
        .map(String::from);
        write_lines(&path, &lines, "");
        let (session, mut writer) = Session::resume(&path).unwrap();
        assert_eq!(session.meta.format, 1);
        assert_eq!(session.transcript.messages.len(), 2);
        assert_eq!(session.transcript.context_tokens, 34);
        // New entries are appended after a line that says they are in this
        // version's format, so an older Junees refuses the file instead of
        // misreading them.
        let clear = Entry::ClearToolResults {
            upto: 0,
            keep_thinking: false,
        };
        writer.record(&clear).unwrap();
        writer.record(&clear).unwrap();
        drop(writer);
        let written = fs::read_to_string(&path).unwrap();
        let kinds: Vec<String> = written
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["type"].to_string())
            .collect();
        assert_eq!(
            kinds[4..],
            [
                "\"format\"",
                "\"clear_tool_results\"",
                "\"clear_tool_results\""
            ]
        );
        let format_line: Value = serde_json::from_str(written.lines().nth(4).unwrap()).unwrap();
        assert_eq!(format_line["format"], json!(FORMAT));
        let reloaded = Session::load(&path).unwrap();
        assert_eq!(reloaded.transcript, session.transcript);
        assert_eq!(reloaded.meta.format, FORMAT);

        // Continued again: already in this format, so no second marker.
        let (_, mut writer) = Session::resume(&path).unwrap();
        writer.record(&clear).unwrap();
        drop(writer);
        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(written.lines().count(), 8);
        assert_eq!(written.matches(r#""type":"format""#).count(), 1);
    }

    #[test]
    fn refuses_a_session_continued_in_a_newer_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let newer = json!({"type": "format", "format": FORMAT + 1});
        write_lines(&path, &[line(&meta()), newer.to_string()], "");
        let err = Session::load(&path).unwrap_err().to_string();
        assert!(err.contains("written by a newer Junees"), "{err}");
        assert!(
            err.contains(&format!("session format {}", FORMAT + 1)),
            "{err}"
        );
    }

    fn tool_results(id: &str, output: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: output.into(),
                is_error,
                images: Vec::new(),
            }],
        }
    }

    /// `n` rounds of a tool call and its result after one user input.
    fn tool_rounds(n: usize) -> Transcript {
        let mut transcript = Transcript::default();
        transcript.apply(Entry::Message(Message::user_text("go")));
        for i in 0..n {
            let id = format!("toolu_{i}");
            transcript.apply(Entry::Message(tool_call(&id)));
            let output = format!("output {i}");
            transcript.apply(Entry::Message(tool_results(&id, &output, i == 0)));
        }
        transcript
    }

    #[test]
    fn clearing_keeps_the_last_three_tool_results() {
        assert_eq!(tool_rounds(3).clear_entry(false), None);
        let mut transcript = tool_rounds(5);
        let before = transcript.clone();
        let entry = transcript.clear_entry(false).unwrap();
        // Results sit at 2, 4, 6, 8, 10; the last three start at 6.
        assert_eq!(
            entry,
            Entry::ClearToolResults {
                upto: 6,
                keep_thinking: false
            }
        );
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"type": "clear_tool_results", "upto": 6})
        );
        transcript.apply(entry);

        assert_eq!(transcript.messages.len(), before.messages.len());
        assert_eq!(
            transcript.messages[2],
            tool_results("toolu_0", CLEARED, true)
        );
        assert_eq!(
            transcript.messages[4],
            tool_results("toolu_1", CLEARED, false)
        );
        assert_eq!(transcript.messages[6..], before.messages[6..]);
        // Everything but tool output is untouched.
        for (now, then) in transcript.messages.iter().zip(&before.messages).take(6) {
            if !has_tool_result(then) {
                assert_eq!(now, then);
            }
        }
        // Already cleared: nothing to do until another result comes in.
        assert_eq!(transcript.clear_entry(false), None);
        transcript.apply(Entry::Message(tool_call("toolu_5")));
        transcript.apply(Entry::Message(tool_results("toolu_5", "more", false)));
        assert_eq!(
            transcript.clear_entry(false),
            Some(Entry::ClearToolResults {
                upto: 8,
                keep_thinking: false
            })
        );
    }

    #[test]
    fn clearing_can_take_the_latest_tool_results_too() {
        let mut transcript = tool_rounds(2);
        let entry = transcript.clear_entry_keeping(0, false).unwrap();
        assert_eq!(
            entry,
            Entry::ClearToolResults {
                upto: 5,
                keep_thinking: false
            }
        );
        transcript.apply(entry);
        assert_eq!(
            transcript.messages[4],
            tool_results("toolu_1", CLEARED, false)
        );
        assert_eq!(transcript.clear_entry_keeping(0, false), None);
        assert_eq!(Transcript::default().clear_entry_keeping(0, false), None);
    }

    /// A tool call after a signed thinking block and a redacted one.
    fn thinking_call(id: &str) -> Message {
        let mut message = tool_call(id);
        message.content.splice(
            0..0,
            [
                ContentBlock::Other(json!({"type": "thinking", "thinking": format!("why {id}"),
                                           "signature": format!("sig-{id}==")})),
                ContentBlock::Other(json!({"type": "redacted_thinking", "data": "opaque+/="})),
            ],
        );
        message
    }

    fn has_thinking(transcript: &Transcript) -> bool {
        transcript
            .messages
            .iter()
            .any(|message| message.content.iter().any(ContentBlock::is_thinking))
    }

    /// One input and `n` tool rounds whose replies all start with thinking,
    /// as Claude 5 models (thinking on by default) send them.
    fn thinking_rounds(n: usize) -> Vec<Message> {
        let mut messages = vec![Message::user_text("go")];
        for i in 0..n {
            let id = format!("toolu_{i}");
            messages.push(thinking_call(&id));
            messages.push(tool_results(&id, "out", false));
        }
        messages
    }

    #[test]
    fn clearing_drops_every_thinking_block() {
        let mut transcript = transcript_of(thinking_rounds(6));
        // The rules cut where they would without thinking blocks.
        let entry = transcript.clear_entry(false).unwrap();
        assert_eq!(Some(&entry), tool_rounds(6).clear_entry(false).as_ref());
        transcript.apply(entry);
        // Thinking after a cleared result would no longer match what came
        // before it, and dropping only that would change what came before
        // the rest: none stays, before the cut or after it.
        assert!(!has_thinking(&transcript), "{:#?}", transcript.messages);
        let mut plain = tool_rounds(6);
        let entry = plain.clear_entry(false).unwrap();
        plain.apply(entry);
        for (now, then) in transcript.messages.iter().zip(&plain.messages) {
            assert_eq!(now.role, then.role);
            assert_eq!(now.content.len(), then.content.len());
        }
        assert_eq!(transcript.messages.len(), 13);
        assert_eq!(transcript.messages[11], tool_call("toolu_5"));

        // Thinking that comes after the clear saw the cleared history and
        // stays until the next change.
        transcript.apply(Entry::Message(thinking_call("toolu_6")));
        assert_eq!(transcript.messages[13], thinking_call("toolu_6"));
    }

    #[test]
    fn compaction_drops_every_thinking_block_and_replays_the_same() {
        let messages = thinking_rounds(5);
        let home = tempfile::tempdir().unwrap();
        let (_, mut writer) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        let mut transcript = Transcript::default();
        for message in messages.clone() {
            let entry = Entry::Message(message);
            writer.record(&entry).unwrap();
            transcript.apply(entry);
        }
        let keep_from = transcript.compact_point(false).unwrap();
        assert_eq!(Some(keep_from), tool_rounds(5).compact_point(false));
        let entry = Entry::Compact {
            summary: "s".into(),
            keep_from,
            keep_thinking: false,
        };
        writer.record(&entry).unwrap();
        transcript.apply(entry);
        drop(writer);

        // The kept replies keep their tool calls, without their thinking:
        // it was made after the messages the summary replaced.
        assert!(!has_thinking(&transcript), "{:#?}", transcript.messages);
        assert_eq!(transcript.messages[1], tool_call("toolu_3"));
        assert_eq!(transcript.messages[3], tool_call("toolu_4"));
        assert_eq!(transcript.interrupted_results(), None);
        let path = sessions_dir(home.path());
        let path = fs::read_dir(path).unwrap().next().unwrap().unwrap().path();
        let (session, _) = Session::resume(&path).unwrap();
        assert_eq!(session.meta.format, FORMAT);
        assert_eq!(session.transcript, transcript);
    }

    #[test]
    fn manual_thinking_compacts_a_turn_whole_and_keeps_thinking() {
        // Manual extended thinking without interleaving: only the turn's
        // first reply thinks.
        let mut messages = vec![Message::user_text("go"), thinking_call("toolu_0")];
        messages.push(tool_results("toolu_0", "out", false));
        for i in 1..6 {
            let id = format!("toolu_{i}");
            messages.push(tool_call(&id));
            messages.push(tool_results(&id, "out", false));
        }
        let transcript = transcript_of(messages);
        // A cut inside the turn would leave it starting with a reply that
        // has no thinking block.
        assert_eq!(transcript.compact_point(false), Some(9));
        assert_eq!(transcript.compact_point(true), Some(13));
        assert_eq!(tool_rounds(1).compact_point(true), None);
        assert_eq!(tool_rounds(2).compact_point(true), Some(5));

        let mut compacted = transcript.clone();
        compacted.apply(Entry::Compact {
            summary: "did 0 to 5".into(),
            keep_from: 13,
            keep_thinking: true,
        });
        assert_eq!(
            compacted.messages,
            [Message {
                role: Role::User,
                content: vec![
                    ContentBlock::text(format!("{SUMMARY_HEADER}did 0 to 5")),
                    ContentBlock::text("go"),
                ],
            }]
        );

        // Cuts before a user input stay where they were, and the kept
        // turns keep their thinking.
        let mut transcript = transcript;
        transcript.apply(Entry::Message(assistant("done")));
        transcript.apply(Entry::Message(Message::user_text("next")));
        transcript.apply(Entry::Message(thinking_call("toolu_9")));
        transcript.apply(Entry::Message(tool_results("toolu_9", "x", false)));
        assert_eq!(transcript.compact_point(true), Some(14));
        let entry = Entry::Compact {
            summary: "s".into(),
            keep_from: 14,
            keep_thinking: true,
        };
        transcript.apply(entry);
        assert_eq!(transcript.messages[1], thinking_call("toolu_9"));
    }

    #[test]
    fn manual_thinking_clears_tool_output_and_keeps_thinking() {
        let mut transcript = transcript_of(thinking_rounds(5));
        let before = transcript.clone();
        let entry = transcript.clear_entry(true).unwrap();
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"type": "clear_tool_results", "upto": 6, "keep_thinking": true})
        );
        transcript.apply(entry);
        assert_eq!(
            transcript.messages[2],
            tool_results("toolu_0", CLEARED, false)
        );
        for i in [1, 3, 5, 7, 9] {
            assert_eq!(transcript.messages[i], before.messages[i]);
        }
        // Entries without the field (format 2, other modes) drop thinking.
        let entry: Entry =
            serde_json::from_value(json!({"type": "clear_tool_results", "upto": 6})).unwrap();
        assert_eq!(
            entry,
            Entry::ClearToolResults {
                upto: 6,
                keep_thinking: false
            }
        );
    }

    #[test]
    fn rejected_images_leave_only_the_trailing_message() {
        let input = Message {
            role: Role::User,
            content: vec![ContentBlock::Image(picture(1)), ContentBlock::text("look")],
        };
        let call = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_1".into(),
                name: "read_file".into(),
                input: json!({}),
            }],
        };
        let result = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: "read shot.png".into(),
                is_error: false,
                images: vec![picture(2000)],
            }],
        };
        // Nothing to clear when the history ends with a reply or holds no
        // image at its end.
        assert!(
            transcript_of(vec![input.clone()])
                .clear_images_entry()
                .is_some()
        );
        assert!(
            transcript_of(vec![input.clone(), assistant("ok")])
                .clear_images_entry()
                .is_none()
        );
        assert!(
            transcript_of(vec![
                input.clone(),
                assistant("ok"),
                Message::user_text("more")
            ])
            .clear_images_entry()
            .is_none()
        );

        let mut transcript = transcript_of(vec![input.clone(), call.clone(), result]);
        let entry = transcript.clear_images_entry().unwrap();
        assert_eq!(
            entry,
            Entry::ClearImages {
                note: REJECTED_IMAGES.to_string()
            }
        );
        let before = transcript.context_size();
        transcript.apply(entry);
        // The images the API took earlier stay; the result loses its own.
        assert_eq!(transcript.messages[..2], [input, call]);
        assert_eq!(
            transcript.messages[2].content,
            [ContentBlock::tool_result(
                "toolu_1",
                format!("read shot.png\n{REJECTED_IMAGES}"),
                false
            )]
        );
        assert!(transcript.context_size() < before);

        // Several input images become one note before the text.
        let mut transcript = Transcript::default();
        transcript.apply(transcript.user_input_entry("both", vec![picture(1), picture(2)]));
        transcript.apply(transcript.user_input_entry("and this", vec![picture(3)]));
        transcript.apply(transcript.clear_images_entry().unwrap());
        assert_eq!(
            transcript.messages[0].content,
            [
                ContentBlock::text(REJECTED_IMAGES),
                ContentBlock::text("both"),
                ContentBlock::text("and this"),
            ]
        );
        assert!(transcript.clear_images_entry().is_none());
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }

    fn transcript_of(messages: Vec<Message>) -> Transcript {
        let mut transcript = Transcript::default();
        for message in messages {
            transcript.apply(Entry::Message(message));
        }
        transcript
    }

    #[test]
    fn a_single_turn_is_cut_after_a_closed_tool_round() {
        assert_eq!(Transcript::default().compact_point(false), None);
        // The first reply answers the input itself: nothing to cut yet.
        assert_eq!(tool_rounds(1).compact_point(false), None);
        // Replies sit at 1, 3, 5, ...; each after the first follows a
        // message of tool results only. The last two rounds stay.
        assert_eq!(tool_rounds(2).compact_point(false), Some(3));
        assert_eq!(tool_rounds(6).compact_point(false), Some(9));

        // Results that carry typed input too (a failed turn) are no round
        // boundary.
        let mut with_input = tool_results("toolu_1", "out", false);
        with_input.content.push(ContentBlock::text("and now?"));
        let transcript = transcript_of(vec![
            Message::user_text("first"),
            tool_call("toolu_1"),
            with_input,
            tool_call("toolu_2"),
            tool_results("toolu_2", "out", false),
            tool_call("toolu_3"),
            tool_results("toolu_3", "out", false),
        ]);
        assert_eq!(transcript.compact_point(false), Some(5));
    }

    #[test]
    fn a_cut_never_keeps_a_search_result_without_its_call() {
        // Web search called next to bash: the search runs on the request
        // after the bash result, so its result comes in the next reply.
        let mut transcript = tool_rounds(4);
        let mut mixed = tool_call("toolu_4");
        mixed.content.insert(
            1,
            ContentBlock::Other(json!({"type": "server_tool_use", "id": "srvtoolu_1",
                "name": "web_search", "input": {"query": "q"}})),
        );
        let mut answered = tool_call("toolu_5");
        answered.content.insert(
            0,
            ContentBlock::Other(json!({"type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_1", "content": []})),
        );
        for message in [
            mixed,
            tool_results("toolu_4", "out", false),
            answered,
            tool_results("toolu_5", "out", false),
            tool_call("toolu_6"),
            tool_results("toolu_6", "out", false),
        ] {
            transcript.apply(Entry::Message(message));
        }
        // Rounds start at 3, 5, 7, 9, 11, 13; 11 holds the result of the
        // call in 9, so the cut goes back to 9.
        let keep_from = transcript.compact_point(false).unwrap();
        assert_eq!(keep_from, 9);
        transcript.apply(Entry::Compact {
            summary: "s".into(),
            keep_from,
            keep_thinking: false,
        });
        assert!(!orphans_server_tool_result(&transcript.messages));
        let kinds: Vec<_> = transcript
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(ContentBlock::other_type)
            .collect();
        assert_eq!(kinds, ["server_tool_use", "web_search_tool_result"]);
    }

    #[test]
    fn compacting_inside_a_turn_keeps_its_input_word_for_word() {
        let mut transcript = tool_rounds(4);
        let before = transcript.clone();
        let keep_from = transcript.compact_point(false).unwrap();
        assert_eq!(keep_from, 5);
        transcript.apply(Entry::Compact {
            summary: "did 0 to 1".into(),
            keep_from,
            keep_thinking: false,
        });

        let head = Message {
            role: Role::User,
            content: vec![
                ContentBlock::text(format!("{SUMMARY_HEADER}did 0 to 1")),
                ContentBlock::text("go"),
            ],
        };
        assert_eq!(transcript.messages[0], head);
        assert_eq!(transcript.messages[1..], before.messages[5..]);
        for (i, message) in transcript.messages.iter().enumerate() {
            let expected = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            assert_eq!(message.role, expected, "{:#?}", transcript.messages);
        }

        // A later cut in the same turn replaces the old summary and keeps
        // the input once.
        for i in 4..6 {
            let id = format!("toolu_{i}");
            transcript.apply(Entry::Message(tool_call(&id)));
            transcript.apply(Entry::Message(tool_results(&id, "more", false)));
        }
        let keep_from = transcript.compact_point(false).unwrap();
        assert_eq!(keep_from, 5);
        transcript.apply(Entry::Compact {
            summary: "did 0 to 3".into(),
            keep_from,
            keep_thinking: false,
        });
        assert_eq!(
            transcript.messages[0].content,
            [
                ContentBlock::text(format!("{SUMMARY_HEADER}did 0 to 3")),
                ContentBlock::text("go"),
            ]
        );
        assert_eq!(transcript.messages[1], tool_call("toolu_4"));
        assert_eq!(transcript.messages.len(), 5);
    }

    #[test]
    fn compaction_keeps_the_last_two_inputs_when_it_can() {
        let mut transcript = tool_rounds(2); // input at 0, results at 2 and 4
        transcript.apply(Entry::Message(assistant("done")));
        transcript.apply(Entry::Message(Message::user_text("second")));
        assert_eq!(transcript.compact_point(false), Some(6));
        transcript.apply(Entry::Message(assistant("ok")));
        transcript.apply(Entry::Message(Message::user_text("third")));
        assert_eq!(transcript.compact_point(false), Some(6));
        transcript.apply(Entry::Message(tool_call("toolu_8")));
        transcript.apply(Entry::Message(tool_results("toolu_8", "x", false)));
        transcript.apply(Entry::Message(assistant("ok")));
        transcript.apply(Entry::Message(Message::user_text("fourth")));
        assert_eq!(transcript.compact_point(false), Some(8));
    }

    #[test]
    fn compaction_never_starts_the_tail_at_a_tool_result() {
        // Input typed after a failed turn joins the tool results; that
        // message cannot lead the tail, since its calls are before it.
        let mut with_results = tool_results("toolu_1", "out", false);
        with_results.content.push(ContentBlock::text("and now?"));
        let transcript = transcript_of(vec![
            Message::user_text("first"),
            tool_call("toolu_1"),
            with_results,
            assistant("hm"),
            Message::user_text("second"),
            tool_call("toolu_2"),
            tool_results("toolu_2", "out", false),
        ]);
        assert_eq!(transcript.compact_point(false), Some(4));

        // Nor right after a message with tool calls, should one show up.
        let odd = transcript_of(vec![
            Message::user_text("first"),
            tool_call("toolu_1"),
            Message::user_text("no result"),
        ]);
        assert_eq!(odd.compact_point(false), None);
        // A message with tool results never leads, even after one without
        // calls.
        let mut orphan = tool_results("toolu_7", "out", false);
        orphan.content.push(ContentBlock::text("hi"));
        let odd = transcript_of(vec![Message::user_text("first"), assistant("hm"), orphan]);
        assert_eq!(odd.compact_point(false), None);
    }

    #[test]
    fn compact_puts_the_summary_in_front_of_the_kept_input() {
        let mut transcript = tool_rounds(1);
        transcript.apply(Entry::Message(assistant("done")));
        transcript.apply(Entry::Message(Message::user_text("second")));
        transcript.apply(Entry::Message(tool_call("toolu_9")));
        transcript.apply(Entry::Message(tool_results("toolu_9", "x", false)));
        transcript.apply(Entry::Usage(Usage {
            input_tokens: 900,
            ..Default::default()
        }));
        let keep_from = transcript.compact_point(false).unwrap();
        assert_eq!(keep_from, 4);
        let tail = transcript.messages[4..].to_vec();
        let entry = Entry::Compact {
            summary: "did things".into(),
            keep_from,
            keep_thinking: false,
        };
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!({"type": "compact", "summary": "did things", "keep_from": 4})
        );
        transcript.apply(entry);

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].role, Role::User);
        assert_eq!(
            transcript.messages[0].content,
            [
                ContentBlock::text("[Summary of the earlier conversation]\ndid things"),
                ContentBlock::text("second"),
            ]
        );
        assert_eq!(transcript.messages[1..], tail[1..]);
        assert_eq!(transcript.context_tokens, 0);
        assert_eq!(transcript.usage.input_tokens, 900);
    }

    #[test]
    fn summary_usage_counts_but_does_not_size_the_context() {
        let mut transcript = Transcript::default();
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Default::default()
        };
        transcript.apply(Entry::Usage(usage));
        transcript.apply(Entry::SummaryUsage(Usage {
            input_tokens: 500,
            ..Default::default()
        }));
        assert_eq!(transcript.usage.input_tokens, 510);
        assert_eq!(transcript.context_tokens, 12);
        assert_eq!(
            serde_json::to_value(Entry::SummaryUsage(usage)).unwrap()["type"],
            "summary_usage"
        );
    }

    #[test]
    fn a_call_that_searched_the_web_counts_but_does_not_size_the_context() {
        let mut transcript = Transcript::default();
        let plain = |input_tokens| Usage {
            input_tokens,
            ..Default::default()
        };
        transcript.apply(Entry::Usage(plain(100)));
        let searched = Usage {
            input_tokens: 40,
            // Read again by every step of the API's search loop.
            cache_read_input_tokens: 270_000,
            server_tool_use: crate::message::ServerToolUsage {
                web_search_requests: 3,
            },
            ..Default::default()
        };
        transcript.apply(Entry::Usage(searched));
        assert_eq!(transcript.context_tokens, 100);
        assert_eq!(transcript.usage.cache_read_input_tokens, 270_000);
        assert_eq!(transcript.usage.web_searches(), 3);
        // Journaled and read back, the count stays.
        let line = serde_json::to_value(Entry::Usage(searched)).unwrap();
        assert_eq!(line["server_tool_use"], json!({"web_search_requests": 3}));
        assert_eq!(
            serde_json::from_value::<Entry>(line).unwrap(),
            Entry::Usage(searched)
        );
        transcript.apply(Entry::Usage(plain(300)));
        assert_eq!(transcript.context_tokens, 300);
    }

    #[test]
    fn a_call_without_usage_keeps_the_last_size_and_estimates_its_reply() {
        let mut transcript = Transcript::default();
        let reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text("a reply of some length")],
        };
        transcript.apply(Entry::Usage(Usage {
            input_tokens: 100,
            ..Default::default()
        }));
        // Measured with its call.
        transcript.apply(Entry::Message(reply.clone()));
        assert_eq!(transcript.context_size(), 100);
        // The server sent no usage for the next call.
        transcript.apply(Entry::Usage(Usage::default()));
        transcript.apply(Entry::Message(reply.clone()));
        let estimate = estimate_tokens(&reply.content);
        assert!(estimate > 0);
        assert_eq!(transcript.context_tokens, 100);
        assert_eq!(transcript.context_size(), 100 + estimate);
    }

    #[test]
    fn estimates_the_size_no_call_measured() {
        let mut transcript = Transcript::default();
        let plain = Usage {
            input_tokens: 100,
            ..Default::default()
        };
        let searched = Usage {
            cache_read_input_tokens: 9_000,
            server_tool_use: crate::message::ServerToolUsage {
                web_search_requests: 2,
            },
            ..Default::default()
        };
        let reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text("a".repeat(400))],
        };
        let results = tool_results("toolu_1", &"b".repeat(400), false);
        transcript.apply(Entry::Message(Message::user_text("hi")));
        transcript.apply(Entry::Usage(plain));
        // The reply of a measured call is in its size already.
        transcript.apply(Entry::Message(reply.clone()));
        assert_eq!(transcript.context_size(), 100);
        transcript.apply(Entry::Message(results.clone()));
        let results_size = estimate_tokens(&results.content);
        assert!(results_size > 100, "{results_size}");
        assert_eq!(transcript.context_size(), 100 + results_size);
        // A searching call measures nothing; its reply is estimated.
        transcript.apply(Entry::Usage(searched));
        transcript.apply(Entry::Message(reply.clone()));
        let reply_size = estimate_tokens(&reply.content);
        assert_eq!(transcript.context_size(), 100 + results_size + reply_size);
        transcript.apply(Entry::AppendText { text: "x".into() });
        assert!(transcript.context_size() > 100 + results_size + reply_size);
        // A compaction estimates what it leaves.
        let mut compacted = transcript.clone();
        compacted.apply(Entry::Compact {
            summary: "s".into(),
            keep_from: 3,
            keep_thinking: false,
        });
        assert_eq!(compacted.context_tokens, 0);
        assert_eq!(
            compacted.context_size(),
            compacted
                .messages
                .iter()
                .map(|message| estimate_tokens(&message.content))
                .sum::<u64>()
        );
        // The next measurement replaces the estimate.
        transcript.apply(Entry::Usage(plain));
        assert_eq!(transcript.context_size(), 100);
    }

    #[test]
    fn estimates_korean_at_a_token_a_character_and_english_by_bytes() {
        let korean = "한국어 테스트 문장입니다. ".repeat(100);
        let hangul = korean
            .chars()
            .filter(|c| ('\u{AC00}'..='\u{D7A3}').contains(c))
            .count() as u64;
        assert_eq!(hangul, 1100);
        let block = [ContentBlock::text(korean.clone())];
        let json = serde_json::to_string(&block).unwrap();
        // A token per 4 bytes came to less than a token per syllable.
        assert!((json.len() as u64).div_ceil(4) < hangul);
        assert!(
            estimate_tokens(&block) >= hangul,
            "{}",
            estimate_tokens(&block)
        );
        // Hangul a token each, the spaces and dots and the JSON a quarter.
        let ascii = json.chars().filter(char::is_ascii).count() as u64;
        assert_eq!(estimate_tokens(&block), hangul + ascii.div_ceil(4));

        // ASCII text estimates by bytes, as before.
        let english = "The quick brown fox jumps over the lazy dog. ".repeat(100);
        let block = [ContentBlock::text(english)];
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(estimate_tokens(&block), (json.len() as u64).div_ceil(4));

        // Japanese and Chinese count like Korean; nothing counts below
        // a token per 4 bytes.
        assert_eq!(estimate_text_tokens("日本語のテキスト中文"), 10);
        for text in [
            "Привет, мир",
            "café ñandú",
            "emoji 😀🚀",
            "สวัสดี",
            "한글 and ASCII",
        ] {
            assert!(
                estimate_text_tokens(text) >= (text.len() as u64).div_ceil(4),
                "{text}"
            );
        }
        assert_eq!(estimate_text_tokens("Привет"), 3);
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn replay_estimates_korean_input_per_character() {
        // Sizes are computed while applying entries, never stored, so a
        // journal written before this estimate replays with it.
        let mut transcript = Transcript::default();
        transcript.apply(Entry::Message(Message::user_text("hi")));
        transcript.apply(Entry::Usage(Usage {
            input_tokens: 100,
            ..Default::default()
        }));
        transcript.apply(Entry::Message(assistant("ok")));
        transcript.apply(Entry::Message(Message::user_text("가".repeat(1000))));
        assert!(
            transcript.context_size() >= 1100,
            "{}",
            transcript.context_size()
        );
    }

    fn server_call(id: &str) -> ContentBlock {
        ContentBlock::Other(json!({"type": "server_tool_use", "id": id,
            "name": "web_search", "input": {"query": "q"}}))
    }

    #[test]
    fn drops_the_server_calls_left_without_a_result() {
        let thinking = ContentBlock::Other(json!({"type": "thinking", "thinking": "t",
            "signature": "s"}));
        let answered = Message {
            role: Role::Assistant,
            content: vec![server_call("srvtoolu_a"), ContentBlock::text("paused")],
        };
        let result = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Other(json!({"type": "web_search_tool_result",
                    "tool_use_id": "srvtoolu_a", "content": []})),
                ContentBlock::text("more"),
                server_call("srvtoolu_b"),
            ],
        };
        // A reply of nothing but thinking and a search, which a later input
        // was already appended after (an older Junees).
        let bare = Message {
            role: Role::Assistant,
            content: vec![thinking, server_call("srvtoolu_c")],
        };
        let mut transcript = transcript_of(vec![
            Message::user_text("q"),
            answered.clone(),
            result,
            Message::user_text("next"),
            bare,
            Message::user_text("again"),
        ]);
        assert_eq!(transcript.open_server_calls(), ["srvtoolu_b", "srvtoolu_c"]);

        let entry = Entry::DropServerCalls {
            ids: transcript.open_server_calls(),
        };
        let line = serde_json::to_value(&entry).unwrap();
        assert_eq!(line["type"], "drop_server_calls");
        assert_eq!(serde_json::from_value::<Entry>(line).unwrap(), entry);
        transcript.apply(entry);
        assert!(transcript.open_server_calls().is_empty());
        assert_eq!(transcript.messages.len(), 4);
        assert_eq!(transcript.messages[1], answered);
        assert_eq!(transcript.messages[2].content.len(), 2);
        assert_eq!(transcript.messages[2].text(), "more");
        // The emptied reply went, and the inputs around it became one.
        assert_eq!(transcript.messages[3].role, Role::User);
        assert_eq!(transcript.messages[3].text(), "next\nagain");
    }

    #[test]
    fn a_call_before_the_last_reply_drops_the_thinking_after_it() {
        let thinking = |signature: &str| {
            ContentBlock::Other(json!({"type": "thinking", "thinking": "t",
                "signature": signature}))
        };
        // A search left open in an earlier reply, with a thinking reply
        // after it.
        let messages = vec![
            Message::user_text("q"),
            Message {
                role: Role::Assistant,
                content: vec![
                    thinking("sig-1"),
                    ContentBlock::text("searching"),
                    server_call("srvtoolu_a"),
                ],
            },
            Message::user_text("next"),
            Message {
                role: Role::Assistant,
                content: vec![thinking("sig-2"), ContentBlock::text("answer")],
            },
        ];
        let transcript = transcript_of(messages.clone());
        let entries = transcript.drop_server_calls_entries(false);
        assert_eq!(
            entries,
            [
                Entry::DropServerCalls {
                    ids: vec!["srvtoolu_a".into()]
                },
                Entry::ClearToolResults {
                    upto: 0,
                    keep_thinking: false
                },
            ]
        );
        let mut dropped = transcript.clone();
        for entry in entries {
            dropped.apply(entry);
        }
        assert!(dropped.open_server_calls().is_empty());
        assert!(
            dropped
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .all(|block| !block.is_thinking())
        );
        assert_eq!(dropped.messages[1].text(), "searching");
        assert_eq!(dropped.messages[3].text(), "answer");
        // Manual thinking: those models do not check what came before.
        assert_eq!(transcript.drop_server_calls_entries(true).len(), 1);

        // Open in the last reply, after its thinking: thinking stays.
        let last = transcript_of(messages[..2].to_vec());
        let entries = last.drop_server_calls_entries(false);
        assert_eq!(entries.len(), 1);
        let mut dropped = last.clone();
        dropped.apply(entries[0].clone());
        assert_eq!(dropped.messages[1].content.len(), 2);
        assert!(dropped.messages[1].content[0].is_thinking());
        // The same reply with its call answered by tool results only: the
        // call is still in the last reply.
        let mut mixed = messages[..2].to_vec();
        mixed[1].content.push(ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "bash".into(),
            input: json!({}),
        });
        mixed.push(tool_results("toolu_1", "out", false));
        assert_eq!(
            transcript_of(mixed).drop_server_calls_entries(false).len(),
            1
        );
        // Nothing open, nothing to record.
        assert!(
            transcript_of(vec![Message::user_text("q")])
                .drop_server_calls_entries(false)
                .is_empty()
        );
    }

    #[test]
    fn needs_a_meta_line_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        write_lines(
            &path,
            &[line(&Entry::Message(Message::user_text("hi")))],
            "",
        );
        assert!(Session::load(&path).is_err());
        fs::write(&path, "").unwrap();
        assert!(Session::load(&path).is_err());
    }

    #[test]
    fn resume_cuts_the_broken_tail_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        write_lines(&path, &[line(&meta()), line(&user)], "{\"type\":\"mess");

        let (session, mut writer) = Session::resume(&path).unwrap();
        assert!(session.warning.is_some());
        writer
            .record(&Entry::AppendText {
                text: "more".into(),
            })
            .unwrap();

        let reloaded = Session::load(&path).unwrap();
        assert!(reloaded.warning.is_none());
        assert_eq!(reloaded.transcript.messages[0].text(), "hi\nmore");
    }

    #[test]
    fn resume_adds_a_newline_after_an_unterminated_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        fs::write(&path, format!("{}\n{}", line(&meta()), line(&user))).unwrap();

        let (_, mut writer) = Session::resume(&path).unwrap();
        writer
            .record(&Entry::AppendText {
                text: "more".into(),
            })
            .unwrap();
        let reloaded = Session::load(&path).unwrap();
        assert_eq!(reloaded.transcript.messages[0].text(), "hi\nmore");
    }

    #[test]
    fn resume_answers_unfinished_tool_calls_and_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            line(&meta()),
            line(&Entry::Message(Message::user_text("sleep"))),
            line(&Entry::Message(tool_call("toolu_9"))),
        ];
        write_lines(&path, &lines, "");

        let (session, writer) = Session::resume(&path).unwrap();
        drop(writer);
        let expected = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_9".into(),
                content: INTERRUPTED.into(),
                is_error: true,
                images: Vec::new(),
            }],
        };
        assert_eq!(session.transcript.messages.len(), 3);
        assert_eq!(session.transcript.messages[2], expected);

        // The fix is in the file too, so it is not added twice.
        let (again, _writer) = Session::resume(&path).unwrap();
        assert_eq!(again.transcript, session.transcript);
    }

    #[test]
    fn resume_refuses_a_session_another_writer_holds() {
        let home = tempfile::tempdir().unwrap();
        let (meta, mut live) = SessionWriter::create(home.path(), Path::new("/w"), "m").unwrap();
        live.record(&Entry::Message(Message::user_text("build it")))
            .unwrap();
        live.record(&Entry::Message(tool_call("toolu_1"))).unwrap();
        let path = live.path().to_path_buf();
        let before = fs::read(&path).unwrap();

        let err = Session::resume(&path).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            format!("session {} is in use by another Junees", meta.id)
        );
        assert_eq!(fs::read(&path).unwrap(), before);

        // The lock goes with the writer; then the tool call counts as cut off.
        drop(live);
        let (session, _writer) = Session::resume(&path).unwrap();
        assert_eq!(session.transcript.messages.len(), 3);
    }

    #[test]
    fn a_resumed_session_is_locked_until_its_writer_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let user = Entry::Message(Message::user_text("hi"));
        write_lines(&path, &[line(&meta()), line(&user)], "");

        let (_, first) = Session::resume(&path).unwrap();
        let err = Session::resume(&path).err().unwrap();
        assert!(
            format!("{err:#}").contains("in use by another Junees"),
            "{err:#}"
        );
        drop(first);
        assert!(Session::resume(&path).is_ok());
    }

    #[test]
    fn no_fix_needed_after_answered_or_plain_turns() {
        let mut transcript = Transcript::default();
        assert!(transcript.interrupted_results().is_none());
        transcript.apply(Entry::Message(Message::user_text("hi")));
        assert!(transcript.interrupted_results().is_none());
        transcript.apply(Entry::Message(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text("yo")],
        }));
        assert!(transcript.interrupted_results().is_none());
    }

    #[test]
    fn ids_sort_by_time() {
        let id = new_id(1_794_205_024);
        assert!(id.starts_with("20261109-061704-"), "{id}");
        assert_eq!(id.len(), "20261109-061704-3fa2".len());
        assert!(new_id(1_709_251_199) < new_id(1_794_205_024));
    }

    #[test]
    fn formats_utc_dates() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(1_709_251_199), "2024-02-29 23:59");
        assert_eq!(format_utc(951_868_800), "2000-03-01 00:00");
        assert_eq!(format_utc(1_794_205_024), "2026-11-09 06:17");
        assert_eq!(format_utc_seconds(1_794_205_024), "2026-11-09T06:17:04Z");
    }

    fn set_mtime(path: &Path, unix_secs: u64) {
        let time = UNIX_EPOCH + std::time::Duration::from_secs(unix_secs);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn new_session(home: &Path, cwd: &str, inputs: &[&str], mtime: u64) -> PathBuf {
        let (meta, mut writer) = SessionWriter::create(home, Path::new(cwd), "m").unwrap();
        for input in inputs {
            writer
                .record(&Entry::Message(Message::user_text(*input)))
                .unwrap();
        }
        let path = sessions_dir(home).join(format!("{}.jsonl", meta.id));
        set_mtime(&path, mtime);
        path
    }

    #[test]
    fn continue_picks_the_latest_session_of_the_folder() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!(find_latest(home, Path::new("/x"), None).unwrap(), None);

        let _old = new_session(home, "/x", &["old"], 1_000);
        let newer = new_session(home, "/x", &["newer"], 2_000);
        let _other = new_session(home, "/y", &["other"], 3_000);
        let junk = sessions_dir(home).join("zzzz.jsonl");
        fs::write(&junk, "not json\n").unwrap();
        set_mtime(&junk, 4_000);
        fs::write(sessions_dir(home).join("notes.txt"), "").unwrap();

        assert_eq!(
            find_latest(home, Path::new("/x"), None).unwrap(),
            Some(newer.clone())
        );
        assert_eq!(find_latest(home, Path::new("/z"), None).unwrap(), None);

        // Each backend continues its own conversations.
        let claude = claude_session(home, "/x", &["asked claude"], 5_000);
        assert_eq!(
            find_latest(home, Path::new("/x"), Some(CLAUDE_CODE)).unwrap(),
            Some(claude)
        );
        assert_eq!(
            find_latest(home, Path::new("/x"), None).unwrap(),
            Some(newer)
        );
        assert_eq!(
            find_latest(home, Path::new("/y"), Some(CLAUDE_CODE)).unwrap(),
            None
        );
    }

    fn claude_turn(session: Option<&str>, input: &str) -> ClaudeTurn {
        ClaudeTurn {
            claude_session: session.map(str::to_string),
            input: input.into(),
            text: format!("answer to {input}"),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..Usage::default()
            },
            end: "answered".into(),
        }
    }

    /// A Claude Code conversation with one turn per input.
    fn claude_session(home: &Path, cwd: &str, inputs: &[&str], mtime: u64) -> PathBuf {
        let (meta, mut writer) =
            SessionWriter::create_for(home, Path::new(cwd), "", Some(CLAUDE_CODE)).unwrap();
        for input in inputs {
            writer
                .record(&Entry::ClaudeTurn(claude_turn(Some("c-1"), input)))
                .unwrap();
        }
        let path = sessions_dir(home).join(format!("{}.jsonl", meta.id));
        set_mtime(&path, mtime);
        path
    }

    #[test]
    fn a_claude_code_session_keeps_its_conversation_id_and_turns() {
        let home = tempfile::tempdir().unwrap();
        let path = claude_session(home.path(), "/x", &["first"], 1_000);
        let (meta, mut writer) = {
            let (session, writer) = Session::resume(&path).unwrap();
            (session.meta, writer)
        };
        assert_eq!(meta.backend.as_deref(), Some(CLAUDE_CODE));
        // A turn cut off before Claude Code named its conversation keeps
        // the earlier id; a later id replaces it.
        let mut cut = claude_turn(None, "second");
        cut.end = "interrupted".into();
        cut.text.clear();
        writer.record(&Entry::ClaudeTurn(cut)).unwrap();
        drop(writer);
        let session = Session::load(&path).unwrap();
        assert_eq!(session.transcript.claude_session.as_deref(), Some("c-1"));
        assert_eq!(session.transcript.claude_turns, 2);
        assert_eq!(session.transcript.usage.input_tokens, 20);
        assert!(session.transcript.messages.is_empty());
        let (_, mut writer) = Session::resume(&path).unwrap();
        writer
            .record(&Entry::ClaudeTurn(claude_turn(Some("c-2"), "third")))
            .unwrap();
        drop(writer);
        let session = Session::load(&path).unwrap();
        assert_eq!(session.transcript.claude_session.as_deref(), Some("c-2"));
        assert_eq!(session.transcript.claude_turns, 3);

        // The lines as written.
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["backend"], "claude-code");
        assert_eq!(lines[0]["format"], FORMAT);
        assert_eq!(lines[1]["type"], "claude_turn");
        assert_eq!(lines[1]["claude_session"], "c-1");
        assert_eq!(lines[1]["input"], "first");
        assert_eq!(lines[1]["text"], "answer to first");
        assert_eq!(lines[1]["end"], "answered");
        assert_eq!(lines[1]["usage"]["input_tokens"], 10);
        assert!(lines[2].get("claude_session").is_none(), "{}", lines[2]);

        let list = list_recent(home.path(), 10).unwrap();
        assert_eq!(list[0].first_input, "first");
        assert_eq!(list[0].backend.as_deref(), Some(CLAUDE_CODE));
    }

    #[test]
    fn resume_accepts_a_unique_id_prefix() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let dir = sessions_dir(home);
        fs::create_dir_all(&dir).unwrap();
        for id in [
            "20261109-061704-3fa2",
            "20261109-061704-9bcd",
            "20261110-080000-0001",
            "20261110-080000-00012",
        ] {
            fs::write(dir.join(format!("{id}.jsonl")), "").unwrap();
        }
        let found = |prefix: &str| {
            find_by_prefix(home, prefix)
                .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        };

        assert_eq!(found("20261109-061704-3").unwrap(), "20261109-061704-3fa2");
        // A full id wins even when a longer id starts with it.
        assert_eq!(
            found("20261110-080000-0001").unwrap(),
            "20261110-080000-0001"
        );
        let err = found("20261109").unwrap_err().to_string();
        assert!(err.contains("matches 2 sessions"), "{err}");
        assert!(found("2025").is_err());
        assert!(found("").is_err());
    }

    #[test]
    fn lists_recent_sessions() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert!(list_recent(home, 10).unwrap().is_empty());
        new_session(home, "/x", &["first question", "and more"], 1_000);
        new_session(home, "/y", &["두 번째"], 2_000);
        new_session(home, "/y", &["third"], 3_000);

        let list = list_recent(home, 2).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].first_input, "third");
        assert_eq!(list[1].first_input, "두 번째");
        assert_eq!(list[1].updated, 2_000);
        assert_eq!(list[1].cwd, "/y");
        assert_eq!(list[1].backend, None);
        let all = list_recent(home, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].first_input, "first question");
    }

    fn channel_session(home: &Path, cwd: &str, input: &str, mtime: u64) -> PathBuf {
        let (meta, mut writer) =
            SessionWriter::create_for_channel(home, Path::new(cwd), "m", None, DISCORD, "505")
                .unwrap();
        writer
            .record(&Entry::Message(Message::user_text(input)))
            .unwrap();
        let path = sessions_dir(home).join(format!("{}.jsonl", meta.id));
        set_mtime(&path, mtime);
        path
    }

    #[test]
    fn channel_sessions_are_left_to_their_channel() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let terminal = new_session(home, "/x", &["at the terminal"], 1_000);
        let channel = channel_session(home, "/x", "in discord", 2_000);

        // The meta line says where the conversation takes place.
        let meta = read_meta(&channel).unwrap();
        assert_eq!(meta.source.as_deref(), Some(DISCORD));
        assert_eq!(meta.channel.as_deref(), Some("505"));
        let first: serde_json::Value = serde_json::from_str(
            fs::read_to_string(&channel)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (&first["source"], &first["channel"]),
            (&json!("discord"), &json!("505"))
        );
        // A terminal session has neither field, and older journals read
        // without them.
        let first: serde_json::Value = serde_json::from_str(
            fs::read_to_string(&terminal)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert!(
            first.get("source").is_none() && first.get("channel").is_none(),
            "{first}"
        );
        assert_eq!(read_meta(&terminal).unwrap().source, None);

        // `-c` passes over the newer channel session.
        assert_eq!(
            find_latest(home, Path::new("/x"), None).unwrap(),
            Some(terminal)
        );
        let listed = list_recent(home, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].first_input, "at the terminal");
        let channels = list_channel_sessions(home, 10).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].first_input, "in discord");
        assert_eq!(channels[0].source.as_deref(), Some(DISCORD));
        assert_eq!(channels[0].channel.as_deref(), Some("505"));
        // `--resume <id>` still finds it by its id.
        let id = channel.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(find_by_prefix(home, &id).unwrap(), channel);
    }

    #[test]
    fn listing_reads_only_the_head_of_each_session() {
        let home = tempfile::tempdir().unwrap();
        let path = new_session(home.path(), "/x", &["first", "second"], 1_000);
        // A broken later line would stop a replay, but the list never
        // reads that far.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"not json\n").unwrap();
        drop(file);
        assert!(Session::load(&path).is_err());

        let list = list_recent(home.path(), 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].first_input, "first");
    }
}
