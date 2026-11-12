//! Slack as the channel engine's platform ([`crate::gateway::platform`]):
//! posts through `chat.postMessage` in mrkdwn ([`super::format`]) and
//! images from `files.slack.com`. Bots have no typing indicator in the
//! Web API (it belonged to the retired RTM API), so `typing` does
//! nothing.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::api::Api;
use super::format::to_mrkdwn;
use crate::approval::{Request, render};
use crate::gateway::ask::{fence_safe, human};
use crate::gateway::platform::Platform;
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

/// An image file attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackFile {
    /// The file name, hidden characters escaped.
    pub name: String,
    /// `url_private_download` (or `url_private`).
    pub url: String,
    /// Bytes, as Slack reports them (0 when it does not).
    pub size: u64,
}

/// The images in `event`'s `files`, and a note for what is left out: a
/// file that is no image, one over [`MAX_IMAGE_BYTES`], a remote file
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
        let image = text("mimetype").is_some_and(|kind| IMAGE_TYPES.contains(&kind));
        if remote {
            notes.push(format!(
                "\"{name}\" is stored outside Slack; not downloaded"
            ));
        } else if text("file_access").is_some_and(|access| access != "visible") {
            notes.push(format!("\"{name}\" is not available to the bot"));
        } else if !image {
            notes.push(format!("\"{name}\" is not an image; only images are read"));
        } else if size > MAX_IMAGE_BYTES {
            notes.push(format!("\"{name}\": {}", too_large(size)));
        } else if let Some(url) = url {
            if files.len() == MAX_ATTACHMENTS {
                notes.push(format!(
                    "\"{name}\" left out: at most {MAX_ATTACHMENTS} images per message"
                ));
            } else {
                files.push(SlackFile {
                    name,
                    url: url.to_string(),
                    size,
                });
            }
        } else {
            notes.push(format!("\"{name}\" has no download address"));
        }
    }
    (files, notes)
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

/// Slack, for the channel engine.
pub struct Slack {
    pub api: Arc<Api>,
}

impl Slack {
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
        "**Approve this tool call?**\n```\n{}```\nReply `y` to run it, `n` to decline (`n \
         <reason>` tells the model why), `a` to allow this kind for the rest of this \
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
}
