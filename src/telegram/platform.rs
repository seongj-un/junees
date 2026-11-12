//! Telegram as the channel engine's platform ([`crate::gateway::platform`]):
//! posts through `sendMessage` in plain text, typing through
//! `sendChatAction`, and images through `getFile` and the file download.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;

use super::api::{Api, Target};
use crate::approval::{Request, render};
use crate::gateway::ask::human;
use crate::gateway::platform::Platform;
use crate::gateway::split::split_plain;
use crate::images::{Image, MAX_IMAGE_BYTES, too_large};
use crate::prompt_text::escape_hidden;

/// The longest `text` of `sendMessage` ("1-4096 characters after entities
/// parsing"; with no `parse_mode` the text is sent as it is).
pub const MESSAGE_LIMIT: usize = 4096;

/// Images of one message (or one album) that are read; an album holds
/// at most 10.
pub const MAX_ATTACHMENTS: usize = 10;

/// Time one download may take.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// U+2060 WORD JOINER: shows as nothing, and an `@name` with it after the
/// `@` is no mention to Telegram.
pub(super) const NO_MENTION: char = '\u{2060}';

/// `text` with each `@name` (an `@` not right after a letter, digit or
/// `_`, followed by a letter, as usernames start) broken by an invisible
/// word joiner after the `@`. A group reply then notifies no one: the
/// model's text may repeat names from web pages or files, and the Bot
/// API has no switch like Discord's `allowed_mentions`. E-mail addresses
/// stay as they are, and so do names in the path of a web address
/// (`https://medium.com/@alice/post`), which Telegram takes as part of
/// the link, so the hidden character would only spoil a copied link.
/// In plain text code in backticks is broken too: a backtick is no code
/// to Telegram there and `@name` in it still notifies. HTML replies leave
/// code entities alone ([`super::format`]).
pub fn defuse_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut from = 0;
    for at in mention_points(text) {
        out.push_str(&text[from..=at]);
        out.push(NO_MENTION);
        from = at + 1;
    }
    out.push_str(&text[from..]);
    out
}

/// The byte offsets of the `@`s in `text` that Telegram would take for
/// mentions, as [`defuse_mentions`] describes.
pub(super) fn mention_points(text: &str) -> Vec<usize> {
    let mut points = Vec::new();
    let mut previous: Option<char> = None;
    // Where the current run of non-space characters starts.
    let mut word = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c.is_whitespace() {
            word = at + c.len_utf8();
        }
        let starts_word = previous.is_none_or(|p| !(p.is_alphanumeric() || p == '_'));
        if c == '@'
            && starts_word
            && chars
                .peek()
                .is_some_and(|(_, next)| next.is_ascii_alphabetic())
            && !in_url_path(&text[word..at])
        {
            points.push(at);
        }
        previous = Some(c);
    }
    points
}

/// Whether what comes right after `before` (a run without spaces) is in
/// the path of an http(s) address: a scheme, a host with a dot, then a
/// `/`.
fn in_url_path(before: &str) -> bool {
    ["https://", "http://"].iter().any(|scheme| {
        before.rfind(scheme).is_some_and(|at| {
            let rest = &before[at + scheme.len()..];
            rest.find('/')
                .is_some_and(|slash| rest[..slash].contains('.') && !rest[..slash].contains('@'))
        })
    })
}

/// An image attached to a message: a photo, or a document that is an
/// image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramFile {
    /// How the file is named in notes: the document's file name (hidden
    /// characters escaped), or `photo`.
    pub name: String,
    pub file_id: String,
    /// Bytes, as Telegram reports them (0 when it does not).
    pub size: u64,
}

/// The image of `message` (`photo`, or a `document` with an image MIME
/// type), and a note for what is left out: a document that is no image,
/// a file over [`MAX_IMAGE_BYTES`], or other media.
pub fn attachments(message: &Value) -> (Vec<TelegramFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut notes = Vec::new();
    if let Some(sizes) = message.get("photo").and_then(Value::as_array) {
        // Sizes come smallest first; take the largest one within the
        // limit (a size without `file_size` counts as fitting).
        let size_of = |size: &Value| size.get("file_size").and_then(Value::as_u64);
        let pick = sizes
            .iter()
            .rev()
            .find(|size| size_of(size).is_none_or(|bytes| bytes <= MAX_IMAGE_BYTES));
        match pick.and_then(|size| Some((size.get("file_id")?.as_str()?, size))) {
            Some((file_id, size)) => files.push(TelegramFile {
                name: "photo".to_string(),
                file_id: file_id.to_string(),
                size: size_of(size).unwrap_or(0),
            }),
            None => {
                let largest = sizes.iter().filter_map(size_of).max().unwrap_or(0);
                notes.push(format!("the photo: {}", too_large(largest)));
            }
        }
    }
    if let Some(document) = message.get("document") {
        let text = |key: &str| document.get(key).and_then(Value::as_str);
        let name = escape_hidden(text("file_name").unwrap_or("file"));
        let image = match text("mime_type") {
            Some(kind) => matches!(
                kind,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ),
            None => has_image_extension(&name),
        };
        let size = document
            .get("file_size")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match (image, text("file_id")) {
            (false, _) => notes.push(format!("\"{name}\" is not an image; only images are read")),
            (true, _) if size > MAX_IMAGE_BYTES => {
                notes.push(format!("\"{name}\": {}", too_large(size)))
            }
            (true, None) => notes.push(format!("\"{name}\" has no file id")),
            (true, Some(file_id)) => files.push(TelegramFile {
                name,
                file_id: file_id.to_string(),
                size,
            }),
        }
    }
    for (key, what) in [
        ("video", "a video"),
        ("animation", "an animation"),
        ("audio", "an audio file"),
        ("voice", "a voice message"),
        ("video_note", "a video message"),
        ("sticker", "a sticker"),
    ] {
        // An animation also comes as a document; one note is enough.
        if message.get(key).is_some() && !(key == "animation" && message.get("document").is_some())
        {
            notes.push(format!("{what} is not read; only images are"));
        }
    }
    (files, notes)
}

fn has_image_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// What the commands do, for `/help` (and `/start`, which Telegram sends
/// when a user opens the bot).
pub fn help(username: &str) -> String {
    format!(
        "\
/new start a new conversation in this chat
/stop stop the turn that runs here (and drop the queued messages)
/status what runs here: session, queue, tokens
/memory what the persistent memory holds (in a group only how much; the contents in a private chat)
/skills the skills Junees knows
/cron the scheduled jobs (in a group without their prompts and folders)
/webhooks the webhook routes (in a group without their prompts and folders)
/help this list
The commands also work as !new, !stop and so on. In a group, mention the bot (@{username}) or reply \
to one of its messages; a command there may name it (/stop@{username} or @{username} /stop)."
    )
}

/// Telegram, for the channel engine.
pub struct Telegram {
    pub api: Arc<Api>,
    /// The bot's username, for `/help`.
    pub username: String,
}

impl Telegram {
    fn target(channel: &str) -> Result<Target> {
        Target::parse(channel).ok_or_else(|| anyhow!("`{channel}` is not a Telegram chat"))
    }

    async fn fetch(&self, file: &TelegramFile) -> Result<Image, String> {
        let name = if file.name == "photo" {
            "the photo".to_string()
        } else {
            format!("\"{}\"", file.name)
        };
        let failed = |why: String| format!("{name} could not be downloaded: {why}");
        let info = self
            .api
            .get_file(&file.file_id)
            .await
            .map_err(|err| failed(err.to_string()))?;
        if let Some(size) = info.size
            && size > MAX_IMAGE_BYTES
        {
            return Err(format!("{name}: {}", too_large(size)));
        }
        let bytes = match self
            .api
            .download(&info.path, MAX_IMAGE_BYTES, DOWNLOAD_TIMEOUT)
            .await
        {
            Ok(bytes) => bytes,
            Err(Ok(len)) => return Err(format!("{name}: {}", too_large(len))),
            Err(Err(why)) => return Err(failed(why)),
        };
        let (image, _) = Image::from_bytes(&bytes).map_err(|err| format!("{name}: {err}"))?;
        Ok(image)
    }
}

#[async_trait]
impl Platform for Telegram {
    type Attachment = TelegramFile;

    fn place(&self) -> &'static str {
        "chat"
    }

    fn message_limit(&self) -> usize {
        MESSAGE_LIMIT
    }

    fn code_blocks(&self) -> bool {
        false
    }

    /// In a group (Telegram gives groups negative chat ids), `@name`s are
    /// broken first ([`defuse_mentions`]); the few added characters may
    /// need one more message.
    async fn post(&self, channel: &str, text: &str) -> Result<()> {
        let target = Self::target(channel)?;
        if target.chat >= 0 {
            return self
                .api
                .send_message(target, text)
                .await
                .map_err(Into::into);
        }
        for part in split_plain(&defuse_mentions(text), MESSAGE_LIMIT) {
            self.api.send_message(target, &part).await?;
        }
        Ok(())
    }

    async fn typing(&self, channel: &str) -> Result<()> {
        self.api
            .typing(Self::target(channel)?)
            .await
            .map_err(Into::into)
    }

    async fn fetch_all(&self, attachments: &[TelegramFile]) -> (Vec<Image>, Vec<String>) {
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
        format!("/{word}")
    }

    fn help(&self) -> String {
        help(&self.username)
    }

    fn task_note(&self) -> String {
        "/task is not available in the gateway yet; run junees task \"<goal>\" on the machine."
            .to_string()
    }

    fn tool_lines(&self, lines: &str) -> String {
        lines.to_string()
    }

    fn private_place(&self) -> &'static str {
        "a private chat"
    }

    fn question(request: &Request, timeout: Duration) -> String {
        question(request, timeout)
    }
}

/// An approval question in plain text (no Markdown on Telegram).
pub fn question(request: &Request, timeout: Duration) -> String {
    let mut details = render(request);
    details.push_str(&format!(
        "     always allows: {}\n",
        request.kinds.join(", ")
    ));
    format!(
        "Approve this tool call?\n{details}Reply y to run it, n to decline (n <reason> tells the \
         model why), a to allow this kind for the rest of this chat's conversation. Anything \
         else declines with your words as the reason. No answer within {} declines. In a \
         group, reply to this message.",
        human(timeout)
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn mentions_are_broken_and_addresses_kept() {
        let broken = |name: &str| format!("@\u{2060}{name}");
        assert_eq!(
            defuse_mentions("@alice and (@bob_2), ask @carol."),
            format!(
                "{} and ({}), ask {}.",
                broken("alice"),
                broken("bob_2"),
                broken("carol")
            )
        );
        for kept in [
            "mail me at a@example.com",
            "x_@y",
            "@ alone",
            "@123",
            "ends with @",
            "한글@name",
        ] {
            assert_eq!(defuse_mentions(kept), kept, "{kept}");
        }
        assert_eq!(
            defuse_mentions("줄\n@dave"),
            format!("줄\n{}", broken("dave"))
        );
    }

    #[test]
    fn names_in_a_web_address_path_stay_whole() {
        let broken = |name: &str| format!("@\u{2060}{name}");
        for kept in [
            "see https://medium.com/@alice/post",
            "(http://example.org/x/@bob)",
            "https://한국.kr/@carol",
        ] {
            assert_eq!(defuse_mentions(kept), kept, "{kept}");
        }
        // Not in a path: the host part, no host, a host without a dot, a
        // name after the address, and code (plain text has no code).
        for (text, name) in [
            ("https://@alice", "alice"),
            ("https:///@alice", "alice"),
            ("https://x/@alice", "alice"),
            ("https://u@a.com/@alice", "alice"),
            ("https://medium.com/x @alice", "alice"),
            ("x://a.b/@alice", "alice"),
            ("`@dataclass`", "dataclass"),
        ] {
            assert!(
                defuse_mentions(text).contains(&broken(name)),
                "{text}: {}",
                defuse_mentions(text)
            );
        }
    }

    #[test]
    fn picks_the_largest_photo_within_the_limit_and_image_documents() {
        let message = json!({"photo": [
            {"file_id": "small", "file_unique_id": "a", "width": 90, "height": 60, "file_size": 1000},
            {"file_id": "big", "file_unique_id": "b", "width": 1280, "height": 853, "file_size": 90000},
            {"file_id": "huge", "file_unique_id": "c", "width": 9000, "height": 6000,
             "file_size": MAX_IMAGE_BYTES + 1}
        ]});
        let (files, notes) = attachments(&message);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_id, "big");
        assert_eq!(files[0].size, 90000);
        assert!(notes.is_empty());

        let message = json!({"photo": [
            {"file_id": "huge", "file_unique_id": "c", "width": 9000, "height": 6000,
             "file_size": MAX_IMAGE_BYTES + 1}
        ]});
        let (files, notes) = attachments(&message);
        assert!(files.is_empty());
        assert!(notes[0].contains("up to 5 MB"), "{notes:?}");

        let message = json!({"document": {"file_id": "d", "file_unique_id": "d",
            "file_name": "shot.png", "mime_type": "image/png", "file_size": 10}});
        let (files, notes) = attachments(&message);
        assert_eq!(files[0].name, "shot.png");
        assert!(notes.is_empty());

        let message = json!({"document": {"file_id": "d", "file_unique_id": "d",
            "file_name": "bad\u{202e}gnp.exe", "mime_type": "application/x-msdownload"}});
        let (files, notes) = attachments(&message);
        assert!(files.is_empty());
        assert!(notes[0].contains("\\u{202e}") && notes[0].contains("not an image"));

        let message = json!({"sticker": {"file_id": "s"}, "voice": {"file_id": "v"}});
        let (files, notes) = attachments(&message);
        assert!(files.is_empty());
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(attachments(&json!({"text": "hi"})), (vec![], vec![]));
    }

    #[test]
    fn the_question_is_plain_text() {
        let request = Request {
            tool: "bash".into(),
            target: "rm -r build".into(),
            reasons: vec!["deletes files".into()],
            preview: None,
            kinds: vec!["bash: delete".into()],
        };
        let text = question(&request, Duration::from_secs(300));
        assert!(text.starts_with("Approve this tool call?\n"), "{text}");
        assert!(text.contains("approve bash: rm -r build"), "{text}");
        assert!(text.contains("within 5m"), "{text}");
        assert!(!text.contains("```") && !text.contains("**"), "{text}");
    }
}
