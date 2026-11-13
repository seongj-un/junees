//! Slack as the channel engine's platform ([`crate::gateway::platform`]):
//! posts through `chat.postMessage` in mrkdwn ([`super::format`]) and
//! images from `files.slack.com`. Bots have no typing indicator in the
//! Web API (it belonged to the retired RTM API), so `typing` does
//! nothing.
//!
//! Approval questions carry Block Kit buttons ([`Platform::post_question`]):
//! the question's (last) message is a `section` block of mrkdwn and an
//! `actions` block of `button` elements whose `action_id` and `value` are
//! [`button_data`] (`junees:<id>:<y|a|n>`, 41 characters; a button's
//! `action_id` takes up to 255, its `value` up to 2000, "Button element").
//! A section's text holds at most 3000 characters ("Section block"),
//! so a longer question is cut before its last part gets the buttons.
//! When the question ends, `chat.update` replaces the blocks with the
//! question and the result, without the buttons. Presses come to the
//! router ([`super::bot`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::api::Api;
use super::format::{escape, to_mrkdwn};
use crate::approval::{Request, render};
use crate::gateway::ask::{
    Choice, Question, WITH_BUTTONS, button_data, fence_safe, human, without_buttons,
};
use crate::gateway::platform::{Attached, Kind, Platform};
use crate::gateway::split::{length, split_message};
use crate::images::{Image, MAX_IMAGE_BYTES, too_large};
use crate::prompt_text::escape_hidden;

/// The longest message part the engine hands to `post`, in UTF-16 units.
/// `chat.postMessage`: "For best results, limit the `text` field to 4,000
/// characters", and Slack cuts a message only past 40,000, so the few
/// characters escaping and mrkdwn add stay well inside.
pub const MESSAGE_LIMIT: usize = 3800;

/// Images of one message that are read.
pub const MAX_ATTACHMENTS: usize = 10;

/// Time one download may take.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Image types Junees reads.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// An image or audio file attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackFile {
    /// The file name, hidden characters escaped.
    pub name: String,
    /// `url_private_download` (or `url_private`).
    pub url: String,
    /// Bytes, as Slack reports them (0 when it does not).
    pub size: u64,
    pub kind: Kind,
}

impl Attached for SlackFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn kind(&self) -> &Kind {
        &self.kind
    }
}

/// The images and audio files (`mimetype` `audio/*`, sizes checked when
/// the turn starts) in `event`'s `files`, and a note for what is left
/// out: a file that is neither, an image over [`MAX_IMAGE_BYTES`], a remote file
/// (its URL may point anywhere: "Never send Slack credentials to a host
/// outside Slack"), one Slack does not give out (`file_access`), and any
/// past [`MAX_ATTACHMENTS`].
pub fn attachments(event: &Value) -> (Vec<SlackFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut notes = Vec::new();
    for file in event
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |key: &str| file.get(key).and_then(Value::as_str);
        let name = escape_hidden(text("name").or(text("title")).unwrap_or("file"));
        let size = file.get("size").and_then(Value::as_u64).unwrap_or(0);
        let remote = text("mode") == Some("external")
            || file.get("is_external").and_then(Value::as_bool) == Some(true)
            || text("filetype") == Some("remote");
        let url = text("url_private_download").or(text("url_private"));
        let kind = match text("mimetype") {
            Some(mime) if IMAGE_TYPES.contains(&mime) => Some(Kind::Image),
            Some(mime) if mime.starts_with("audio/") => Some(Kind::Audio {
                mime: mime.to_string(),
            }),
            _ => None,
        };
        if remote {
            notes.push(format!(
                "\"{name}\" is stored outside Slack; not downloaded"
            ));
        } else if text("file_access").is_some_and(|access| access != "visible") {
            notes.push(format!("\"{name}\" is not available to the bot"));
        } else if kind.is_none() {
            notes.push(format!(
                "\"{name}\" is not an image or audio; only those are read"
            ));
        } else if kind == Some(Kind::Image) && size > MAX_IMAGE_BYTES {
            notes.push(format!("\"{name}\": {}", too_large(size)));
        } else if let Some(url) = url {
            if files.len() == MAX_ATTACHMENTS {
                notes.push(format!(
                    "\"{name}\" left out: at most {MAX_ATTACHMENTS} files per message"
                ));
            } else {
                files.push(SlackFile {
                    name,
                    url: url.to_string(),
                    size,
                    kind: kind.unwrap_or(Kind::Image),
                });
            }
        } else {
            notes.push(format!("\"{name}\" has no download address"));
        }
    }
    (files, notes)
}

/// Whether a download is an HTML page rather than a file.
fn looks_like_html(bytes: &[u8]) -> bool {
    let start: Vec<u8> = bytes
        .iter()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .take(15)
        .map(u8::to_ascii_lowercase)
        .collect();
    start.starts_with(b"<!doctype html") || start.starts_with(b"<html")
}

/// What the commands do, for `!help`.
pub const HELP: &str = "\
`!new` start a new conversation here
`!stop` stop the turn that runs here (and drop the queued messages)
`!status` what runs here: session, queue, tokens
`!memory` what the persistent memory holds (in a channel only how much; the contents in a DM)
`!skills` the skills Junees knows
`!cron` the scheduled jobs (in a channel without their prompts and folders)
`!webhooks` the webhook routes (in a channel without their prompts and folders)
`!help` this list
In a channel, mention the bot to start; each thread is its own conversation, and replies in \
the bot's thread need no mention when the app gets channel messages (message.channels).";

/// Where a conversation's messages go: a channel, and in a channel the
/// thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub channel: String,
    pub thread_ts: Option<String>,
}

impl Target {
    /// The gateway's channel key: `<channel>` (a DM) or
    /// `<channel>:<thread_ts>`.
    pub fn key(&self) -> String {
        match &self.thread_ts {
            Some(thread) => format!("{}:{thread}", self.channel),
            None => self.channel.clone(),
        }
    }

    /// The target a channel key names.
    pub fn parse(key: &str) -> Self {
        match key.split_once(':') {
            Some((channel, thread)) => Self {
                channel: channel.to_string(),
                thread_ts: Some(thread.to_string()),
            },
            None => Self {
                channel: key.to_string(),
                thread_ts: None,
            },
        }
    }
}

/// The longest text of a `section` block ("Section block": "maximum
/// length is 3000 characters"), counted here in UTF-16 units, which are
/// never fewer than characters.
pub const SECTION_LIMIT: usize = 3000;

/// Slack, for the channel engine.
pub struct Slack {
    pub api: Arc<Api>,
    /// The questions posted with buttons, by question id, until they end.
    questions: Mutex<HashMap<String, Posted>>,
}

/// A question message with buttons.
struct Posted {
    /// The Slack channel id.
    channel: String,
    ts: String,
    /// Its section's mrkdwn, which the edit keeps.
    text: String,
}

impl Slack {
    pub fn new(api: Arc<Api>) -> Self {
        Self {
            api,
            questions: Mutex::new(HashMap::new()),
        }
    }

    fn questions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Posted>> {
        self.questions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Posts `parts`, then `last` without the words about buttons, after
    /// posting the question with buttons failed with `err`.
    async fn post_without_buttons(
        &self,
        channel: &str,
        parts: &[String],
        last: &str,
        err: anyhow::Error,
    ) -> anyhow::Error {
        let last = without_buttons(last);
        for part in parts.iter().map(String::as_str).chain([last.as_str()]) {
            if let Err(again) = self.post(channel, part).await {
                return anyhow!("{err:#}; posting it without buttons failed too: {again:#}");
            }
        }
        anyhow!("{err:#}; posted it without buttons")
    }
    async fn fetch(&self, file: &SlackFile) -> Result<Image, String> {
        let name = format!("\"{}\"", file.name);
        let bytes = match self
            .api
            .download(&file.url, MAX_IMAGE_BYTES, DOWNLOAD_TIMEOUT)
            .await
        {
            Ok(bytes) => bytes,
            Err(Ok(len)) => return Err(format!("{name}: {}", too_large(len))),
            Err(Err(why)) => return Err(format!("{name} could not be downloaded: {why}")),
        };
        let (image, _) = Image::from_bytes(&bytes).map_err(|err| format!("{name}: {err}"))?;
        Ok(image)
    }
}

#[async_trait]
impl Platform for Slack {
    type Attachment = SlackFile;

    /// A DM, or a thread in a channel.
    fn place(&self) -> &'static str {
        "chat"
    }

    fn message_limit(&self) -> usize {
        MESSAGE_LIMIT
    }

    async fn post(&self, channel: &str, text: &str) -> Result<()> {
        let target = Target::parse(channel);
        self.api
            .post_message(
                &target.channel,
                target.thread_ts.as_deref(),
                &to_mrkdwn(text),
            )
            .await
            .map_err(Into::into)
    }

    async fn typing(&self, _channel: &str) -> Result<()> {
        Ok(())
    }

    async fn fetch_audio(
        &self,
        attachment: &SlackFile,
        limit: u64,
    ) -> Result<Vec<u8>, Result<u64, String>> {
        let name = &attachment.name;
        let bytes = self
            .api
            .download(&attachment.url, limit, DOWNLOAD_TIMEOUT)
            .await
            .map_err(|err| {
                err.map_err(|why| format!("\"{name}\" could not be downloaded: {why}"))
            })?;
        // Without files:read, Slack answers 200 with its sign-in page.
        if looks_like_html(&bytes) {
            return Err(Err(format!(
                "\"{name}\" could not be downloaded: Slack answered with a web page, not the \
                 file (does the app have the files:read scope?)"
            )));
        }
        Ok(bytes)
    }

    async fn fetch_all(&self, attachments: &[SlackFile]) -> (Vec<Image>, Vec<String>) {
        let mut images = Vec::new();
        let mut notes = Vec::new();
        for file in attachments {
            match self.fetch(file).await {
                Ok(image) => images.push(image),
                Err(note) => notes.push(note),
            }
        }
        (images, notes)
    }

    fn command(&self, word: &str) -> String {
        format!("`!{word}`")
    }

    fn help(&self) -> String {
        HELP.to_string()
    }

    fn task_note(&self) -> String {
        "`!task` is not available in the gateway yet; run `junees task \"<goal>\"` on the \
         machine."
            .to_string()
    }

    fn tool_lines(&self, lines: &str) -> String {
        format!("```\n{}\n```", fence_safe(lines))
    }

    fn private_place(&self) -> &'static str {
        "a direct message"
    }

    fn question(request: &Request, timeout: Duration) -> String {
        question(request, timeout)
    }

    /// The question's last message (it is cut like any post, and again
    /// to fit a section) carries the buttons.
    async fn post_question(&self, channel: &str, question: &Question) -> Result<Option<String>> {
        let (parts, last) = button_parts(&question.text);
        for (at, part) in parts.iter().enumerate() {
            if let Err(err) = self.post(channel, part).await {
                return Err(self
                    .post_without_buttons(channel, &parts[at..], &last, err)
                    .await);
            }
        }
        let target = Target::parse(channel);
        let text = to_mrkdwn(&last);
        let ts = match self
            .api
            .post_blocks(
                &target.channel,
                target.thread_ts.as_deref(),
                &text,
                question_blocks(&text, question),
            )
            .await
        {
            Ok(ts) => ts,
            Err(err) => {
                return Err(self
                    .post_without_buttons(channel, &[], &last, err.into())
                    .await);
            }
        };
        self.questions().insert(
            question.id.clone(),
            Posted {
                channel: target.channel,
                ts: ts.clone(),
                text,
            },
        );
        Ok(Some(ts))
    }

    /// The buttons go; the result shows in bold under the question, as a
    /// section of its own.
    async fn close_question(&self, _channel: &str, id: &str, outcome: &str) -> Result<()> {
        let Some(posted) = self.questions().remove(id) else {
            return Ok(());
        };
        let line = format!("*{}*", escape(outcome));
        let blocks = json!([section(&posted.text), section(&line)]);
        self.api
            .update_message(
                &posted.channel,
                &posted.ts,
                &format!("{}\n\n{line}", posted.text),
                blocks,
            )
            .await
            .map_err(Into::into)
    }
}

/// `text`, a question in Markdown, cut into the messages before the one
/// with the buttons and that one, whose mrkdwn fits a section
/// ([`SECTION_LIMIT`]; escaping makes `&` five characters long).
fn button_parts(text: &str) -> (Vec<String>, String) {
    let mut parts = split_message(text, MESSAGE_LIMIT);
    let mut last = parts.pop().unwrap_or_default();
    let mut limit = MESSAGE_LIMIT;
    // At 475 units even text of nothing but `&` fits.
    while length(&to_mrkdwn(&last)) > SECTION_LIMIT && limit > 500 {
        limit /= 2;
        let mut pieces = split_message(&last, limit);
        last = pieces.pop().unwrap_or_default();
        parts.extend(pieces);
    }
    (parts, last)
}

/// A `section` block of mrkdwn `text`, `verbatim` so Slack does not turn
/// plain words into links or mentions ("Text object": "When set to
/// `true`, Slack will continue to process all markdown formatting ... but
/// it won't modify any plain-text content").
fn section(text: &str) -> Value {
    json!({
        "type": "section",
        "text": {"type": "mrkdwn", "text": text, "verbatim": true},
    })
}

/// The blocks of a question's message: its mrkdwn `text`, then one
/// `actions` block with a button for each choice
/// ([`Choice::offered`]).
fn question_blocks(text: &str, question: &Question) -> Value {
    let buttons: Vec<Value> = Choice::offered(question)
        .into_iter()
        .map(|choice| {
            let data = button_data(&question.id, choice);
            let mut button = json!({
                "type": "button",
                "text": {"type": "plain_text", "text": choice.label()},
                "action_id": data,
                "value": data,
            });
            // "Button element": `primary` is green, `danger` red.
            match choice {
                Choice::Allow => button["style"] = json!("primary"),
                Choice::Decline => button["style"] = json!("danger"),
                Choice::Always => {}
            }
            button
        })
        .collect();
    json!([section(text), {"type": "actions", "elements": buttons}])
}

/// An approval question, in Markdown that [`to_mrkdwn`] turns into
/// Slack's: what the call does in a code block, then the choices.
pub fn question(request: &Request, timeout: Duration) -> String {
    let mut details = render(request);
    details.push_str(&format!(
        "     always allows: {}\n",
        request.kinds.join(", ")
    ));
    format!(
        "**Approve this tool call?**\n```\n{}```\n{WITH_BUTTONS} `y` to run it, `n` to decline \
         (`n <reason>` tells the model why), `a` to allow this kind for the rest of this \
         conversation. Anything else declines with your words as the reason. No answer within \
         {} declines. In a channel, reply in this thread.",
        fence_safe(&details),
        human(timeout)
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn targets_round_trip() {
        for target in [
            Target {
                channel: "D123".into(),
                thread_ts: None,
            },
            Target {
                channel: "C123".into(),
                thread_ts: Some("1700000000.000100".into()),
            },
        ] {
            assert_eq!(Target::parse(&target.key()), target);
        }
    }

    #[test]
    fn picks_the_image_files() {
        let file = |name: &str, mimetype: &str, size: u64| {
            json!({"id": "F1", "name": name, "mimetype": mimetype, "size": size,
                   "mode": "hosted", "is_external": false,
                   "url_private": format!("https://files.slack.com/files-pri/T1-F1/{name}"),
                   "url_private_download":
                       format!("https://files.slack.com/files-pri/T1-F1/download/{name}")})
        };
        let mut remote = file("remote.png", "image/png", 10);
        remote["mode"] = json!("external");
        remote["is_external"] = json!(true);
        remote["url_private"] = json!("https://evil.example/x.png");
        let mut hidden = file("hidden.png", "image/png", 10);
        hidden["file_access"] = json!("check_file_info");
        let event = json!({"files": [
            file("shot.png", "image/png", 10),
            file("notes.txt", "text/plain", 10),
            file("huge.png", "image/png", MAX_IMAGE_BYTES + 1),
            file("bad\u{202e}gnp.exe", "application/x-msdownload", 10),
            remote,
            hidden,
        ]});
        let (files, notes) = attachments(&event);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "shot.png");
        assert!(files[0].url.contains("/download/shot.png"));
        assert_eq!(notes.len(), 5, "{notes:?}");
        assert!(notes[0].contains("not an image"));
        assert!(notes[1].contains("up to 5 MB"), "{notes:?}");
        assert!(notes[2].contains("\\u{202e}"), "{notes:?}");
        assert!(notes[3].contains("outside Slack"));
        assert!(notes[4].contains("not available"));
        assert_eq!(attachments(&json!({"text": "hi"})), (vec![], vec![]));

        let many: Vec<Value> = (0..12)
            .map(|n| file(&format!("{n}.png"), "image/png", 1))
            .collect();
        let (files, notes) = attachments(&json!({"files": many}));
        assert_eq!((files.len(), notes.len()), (MAX_ATTACHMENTS, 2));
    }

    #[test]
    fn a_long_question_is_cut_so_its_last_part_fits_a_section() {
        let request = Request {
            tool: "bash".into(),
            target: format!("echo {}", "a&b<c> ".repeat(450)),
            reasons: vec!["runs a command".into()],
            preview: None,
            kinds: vec!["bash: run".into()],
        };
        let text = question(&request, Duration::from_secs(300));
        assert!(
            length(&text) <= MESSAGE_LIMIT,
            "one message before escaping"
        );
        assert!(length(&to_mrkdwn(&text)) > SECTION_LIMIT);
        let (parts, last) = button_parts(&text);
        assert!(!parts.is_empty());
        assert!(length(&to_mrkdwn(&last)) <= SECTION_LIMIT);
        assert!(last.contains(WITH_BUTTONS), "{last}");
        // Every part keeps its code block closed.
        for part in parts.iter().chain([&last]) {
            assert_eq!(part.matches("```").count() % 2, 0, "{part}");
        }
        // A short one stays whole.
        let short = question(
            &Request {
                target: "ls".into(),
                ..request
            },
            Duration::from_secs(9),
        );
        assert_eq!(button_parts(&short), (vec![], short.clone()));
    }

    #[test]
    fn buttons_carry_the_question_id() {
        let mut question = Question {
            id: "0123456789abcdef0123456789abcdef".into(),
            text: "q".into(),
            always: false,
        };
        let blocks = question_blocks("*q* &amp;", &question);
        assert_eq!(
            blocks[0],
            json!({"type": "section",
                   "text": {"type": "mrkdwn", "text": "*q* &amp;", "verbatim": true}})
        );
        let values: Vec<&str> = blocks[1]["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["value"].as_str().unwrap())
            .collect();
        assert_eq!(
            values,
            [
                "junees:0123456789abcdef0123456789abcdef:y",
                "junees:0123456789abcdef0123456789abcdef:n"
            ]
        );
        question.always = true;
        let blocks = question_blocks("q", &question);
        assert_eq!(blocks[1]["elements"][1]["text"]["text"], "Always allow");
        assert_eq!(
            blocks[1]["elements"][1]["action_id"],
            blocks[1]["elements"][1]["value"]
        );
    }

    #[test]
    fn the_question_becomes_mrkdwn() {
        let request = Request {
            tool: "bash".into(),
            target: "rm -r build && echo <done>".into(),
            reasons: vec!["deletes files".into()],
            preview: None,
            kinds: vec!["bash: delete".into()],
        };
        let text = to_mrkdwn(&question(&request, Duration::from_secs(300)));
        assert!(
            text.starts_with("*Approve this tool call?*\n```\n"),
            "{text}"
        );
        assert!(
            text.contains("approve bash: rm -r build &amp;&amp; echo &lt;done&gt;"),
            "{text}"
        );
        assert!(text.contains("within 5m"), "{text}");
        assert!(!text.contains("**"), "{text}");
    }

    #[test]
    fn audio_files_are_kept_for_transcription() {
        let event = json!({"files": [
            {"name": "clip.webm", "mimetype": "audio/webm", "size": 90_000_000u64,
             "url_private_download": "https://files.slack.com/files-pri/T-F/clip.webm"},
            {"name": "notes.txt", "mimetype": "text/plain", "size": 3,
             "url_private_download": "https://files.slack.com/files-pri/T-F/notes.txt"}
        ]});
        let (files, notes) = attachments(&event);
        assert_eq!(files.len(), 1);
        assert_eq!(
            (files[0].name.as_str(), files[0].size, &files[0].kind),
            (
                "clip.webm",
                90_000_000,
                &Kind::Audio {
                    mime: "audio/webm".into()
                }
            )
        );
        assert_eq!(
            notes,
            ["\"notes.txt\" is not an image or audio; only those are read"]
        );
    }

    #[test]
    fn a_sign_in_page_is_no_audio() {
        assert!(looks_like_html(b"\n  <!DOCTYPE html><html>"));
        assert!(looks_like_html(b"<HTML lang=en>"));
        assert!(!looks_like_html(b"\x1aE\xdf\xa3 webm"));
        assert!(!looks_like_html(b"OggS"));
    }
}
