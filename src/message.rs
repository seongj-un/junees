//! Conversation types shared by the agent loop and LLM providers.
//!
//! The shapes follow the Anthropic Messages API so they serialize directly
//! into request bodies.

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::images::{Image, MediaType, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One block of a message. Junees works with text, images, tool calls and
/// tool results; every other block the API sends (thinking,
/// redacted_thinking, server tool calls and their results, ...) is kept as
/// [`ContentBlock::Other`] and sent back as it came.
///
/// Written in the API's shape ([`Serialize`] below); read through a strict
/// mirror of the known types (`try_from = "Value"`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "Value")]
pub enum ContentBlock {
    Text {
        text: String,
        /// The sources the API attached to the text (web search results
        /// it cites), kept as it sent them: their `encrypted_index` must
        /// go back with the text in later requests.
        citations: Option<Value>,
    },
    /// An image the user attached (`/image`, `--image`, a Discord
    /// attachment).
    Image(Image),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        /// Images the tool gave back (`read_file` of a picture). With any,
        /// `content` goes out as a list: a text block, then the images.
        images: Vec<Image>,
    },
    /// A block of another type, kept as the JSON object the API sent,
    /// `type` included. The API rejects thinking blocks that come back
    /// changed (their `signature` covers them), so it is never edited.
    Other(Value),
}

impl ContentBlock {
    /// A text block of Junees's own, citing nothing.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            citations: None,
        }
    }

    /// A tool result without images.
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
            images: Vec::new(),
        }
    }

    /// The `type` of an [`ContentBlock::Other`] block.
    pub fn other_type(&self) -> Option<&str> {
        match self {
            ContentBlock::Other(value) => value.get("type").and_then(Value::as_str),
            _ => None,
        }
    }

    /// A `thinking` or `redacted_thinking` block.
    pub fn is_thinking(&self) -> bool {
        matches!(self.other_type(), Some("thinking" | "redacted_thinking"))
    }

    /// The images of the block: an image block's own, a tool result's.
    pub fn images(&self) -> &[Image] {
        match self {
            ContentBlock::Image(image) => std::slice::from_ref(image),
            ContentBlock::ToolResult { images, .. } => images,
            _ => &[],
        }
    }
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // A map, not a struct: the request wraps blocks in a flattened
        // struct to add `cache_control`.
        let mut map = serializer.serialize_map(None)?;
        match self {
            ContentBlock::Text { text, citations } => {
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", text)?;
                if let Some(citations) = citations {
                    map.serialize_entry("citations", citations)?;
                }
            }
            ContentBlock::Image(image) => {
                map.serialize_entry("type", "image")?;
                map.serialize_entry("source", &SourceOut(image))?;
            }
            ContentBlock::ToolUse { id, name, input } => {
                map.serialize_entry("type", "tool_use")?;
                map.serialize_entry("id", id)?;
                map.serialize_entry("name", name)?;
                map.serialize_entry("input", input)?;
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
            } => {
                map.serialize_entry("type", "tool_result")?;
                map.serialize_entry("tool_use_id", tool_use_id)?;
                if images.is_empty() {
                    map.serialize_entry("content", content)?;
                } else {
                    map.serialize_entry("content", &ResultContentOut { content, images })?;
                }
                if *is_error {
                    map.serialize_entry("is_error", &true)?;
                }
            }
            // Always an object: only those with a string `type` are read.
            ContentBlock::Other(value) => {
                for (key, field) in value.as_object().into_iter().flatten() {
                    map.serialize_entry(key, field)?;
                }
            }
        }
        map.end()
    }
}

/// The `source` of an image block.
struct SourceOut<'a>(&'a Image);

impl Serialize for SourceOut<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        match &self.0.source {
            Source::Base64(data) => {
                map.serialize_entry("type", "base64")?;
                map.serialize_entry("media_type", self.0.media_type.as_str())?;
                map.serialize_entry("data", &**data)?;
            }
            // Only in session journals ([`crate::images::store`]).
            Source::Stored { sha256 } => {
                map.serialize_entry("type", STORED_SOURCE)?;
                map.serialize_entry("media_type", self.0.media_type.as_str())?;
                map.serialize_entry("sha256", sha256)?;
            }
        }
        map.end()
    }
}

/// The `type` of an image source that names a file of the session's image
/// folder; never sent to the API.
pub const STORED_SOURCE: &str = "junees_file";

/// The `content` of a tool result with images: its text, then the images.
struct ResultContentOut<'a> {
    content: &'a str,
    images: &'a [Image],
}

impl Serialize for ResultContentOut<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(None)?;
        if !self.content.is_empty() {
            seq.serialize_element(&ContentBlock::text(self.content))?;
        }
        for image in self.images {
            seq.serialize_element(&ImageOut(image))?;
        }
        seq.end()
    }
}

struct ImageOut<'a>(&'a Image);

impl Serialize for ImageOut<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("type", "image")?;
        map.serialize_entry("source", &SourceOut(self.0))?;
        map.end()
    }
}

/// The block types Junees reads, in the API's shape. Blocks of these types
/// must parse as such; any other type becomes [`ContentBlock::Other`].
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Known {
    Text {
        text: String,
        #[serde(default)]
        citations: Option<Value>,
    },
    Image {
        source: SourceIn,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ResultContentIn,
        #[serde(default)]
        is_error: bool,
    },
}

/// An image block's `source` as Junees writes it.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SourceIn {
    Base64 {
        media_type: String,
        data: String,
    },
    #[serde(rename = "junees_file")]
    Stored {
        media_type: String,
        sha256: String,
    },
}

impl SourceIn {
    fn image(self) -> Result<Image, String> {
        let (media_type, source) = match self {
            SourceIn::Base64 { media_type, data } => (media_type, Source::Base64(data.into())),
            SourceIn::Stored { media_type, sha256 } => {
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("`{sha256}` is not a SHA-256 in hex"));
                }
                (media_type, Source::Stored { sha256 })
            }
        };
        let media_type = MediaType::parse(&media_type)
            .ok_or_else(|| format!("unsupported image media_type `{media_type}`"))?;
        Ok(Image { media_type, source })
    }
}

/// A tool result's `content`: text, or text and image blocks.
#[derive(Deserialize)]
#[serde(untagged)]
enum ResultContentIn {
    Text(String),
    Blocks(Vec<ResultBlockIn>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResultBlockIn {
    Text { text: String },
    Image { source: SourceIn },
}

impl TryFrom<Value> for ContentBlock {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, String> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("a content block must be an object with a string `type`")?;
        if !matches!(kind, "text" | "image" | "tool_use" | "tool_result") {
            return Ok(ContentBlock::Other(value));
        }
        Ok(
            match serde_json::from_value(value).map_err(|err| err.to_string())? {
                // `citations: null` reads as `None`.
                Known::Text { text, citations } => ContentBlock::Text { text, citations },
                Known::Image { source } => ContentBlock::Image(source.image()?),
                Known::ToolUse { id, name, input } => ContentBlock::ToolUse { id, name, input },
                Known::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let (content, images) = match content {
                        ResultContentIn::Text(text) => (text, Vec::new()),
                        ResultContentIn::Blocks(blocks) => {
                            let mut texts = Vec::new();
                            let mut images = Vec::new();
                            for block in blocks {
                                match block {
                                    ResultBlockIn::Text { text } => texts.push(text),
                                    ResultBlockIn::Image { source } => images.push(source.image()?),
                                }
                            }
                            (texts.join("\n"), images)
                        }
                    };
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        images,
                    }
                }
            },
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// All text blocks of the message joined with newlines.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    #[serde(other)]
    Other,
}

/// Token counts of a model call. `input_tokens` excludes the prompt tokens
/// written to or read from the prompt cache, which are counted apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_creation_input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_input_tokens: u64,
    /// The part of `output_tokens` a reasoning model spent on reasoning the
    /// API does not give back (OpenAI's
    /// `completion_tokens_details.reasoning_tokens`): billed as output, but
    /// not part of the conversation the next request sends.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
    /// Calls of the tools the API runs itself, billed apart from tokens.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "ServerToolUsage::is_empty"
    )]
    pub server_tool_use: ServerToolUsage,
}

/// `usage.server_tool_use` of a response: how often the API ran its own
/// tools for the call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerToolUsage {
    #[serde(default)]
    pub web_search_requests: u64,
}

impl ServerToolUsage {
    fn is_empty(&self) -> bool {
        self.web_search_requests == 0
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The API sends `null` for a count object it has nothing for.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.server_tool_use.web_search_requests += other.server_tool_use.web_search_requests;
    }

    /// Web searches the API ran.
    pub fn web_searches(&self) -> u64 {
        self.server_tool_use.web_search_requests
    }

    /// Size of the conversation after this call: everything it read plus
    /// what it wrote, which the next request sends back. Reasoning the API
    /// keeps to itself is not sent back.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens.saturating_sub(self.reasoning_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasoning_tokens_count_as_output_but_not_as_conversation() {
        let mut usage = Usage {
            input_tokens: 100,
            output_tokens: 30,
            reasoning_tokens: 20,
            ..Usage::default()
        };
        assert_eq!(usage.context_tokens(), 110);
        usage.add(usage);
        assert_eq!((usage.output_tokens, usage.reasoning_tokens), (60, 40));
        // Kept in the journal only when there are some, and read back.
        let value = serde_json::to_value(usage).unwrap();
        assert_eq!(value["reasoning_tokens"], 40);
        assert_eq!(serde_json::from_value::<Usage>(value).unwrap(), usage);
        let plain = serde_json::to_value(Usage::default()).unwrap();
        assert!(plain.get("reasoning_tokens").is_none(), "{plain}");
    }

    #[test]
    fn tool_result_omits_is_error_when_false() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "ok".into(),
            is_error: false,
            images: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"})
        );
    }

    #[test]
    fn tool_result_keeps_is_error_when_true() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "boom".into(),
            is_error: true,
            images: Vec::new(),
        };
        assert_eq!(serde_json::to_value(&block).unwrap()["is_error"], true);
    }

    #[test]
    fn unknown_block_types_are_kept_as_they_came() {
        let thinking = json!({"type": "thinking", "thinking": "hmm", "signature": "abc"});
        let redacted = json!({"type": "redacted_thinking", "data": "EmwKAhgB"});
        let server = json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                            "input": {"query": "rust"}});
        let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
            thinking,
            {"type": "text", "text": "hi", "citations": null},
            redacted,
            server,
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
        ]))
        .unwrap();
        assert_eq!(blocks[0], ContentBlock::Other(thinking.clone()));
        assert_eq!(blocks[1], ContentBlock::text("hi"));
        assert_eq!(blocks[2], ContentBlock::Other(redacted.clone()));
        assert_eq!(blocks[3], ContentBlock::Other(server.clone()));
        assert!(matches!(&blocks[4], ContentBlock::ToolUse { name, .. } if name == "bash"));
        assert!(blocks[0].is_thinking() && blocks[2].is_thinking());
        assert!(!blocks[1].is_thinking() && !blocks[3].is_thinking());
        assert_eq!(blocks[3].other_type(), Some("server_tool_use"));

        // Written back, they are the objects that came in.
        let written = serde_json::to_value(&blocks).unwrap();
        assert_eq!(written[0], thinking);
        assert_eq!(written[2], redacted);
        assert_eq!(written[3], server);
    }

    #[test]
    fn known_block_types_must_parse() {
        for bad in [
            json!({"type": "text"}),
            json!({"type": "tool_use", "id": "t", "name": "bash"}),
            json!({"type": "tool_result", "content": "x"}),
            json!({"text": "no type"}),
            json!({"type": 7}),
            json!("text"),
        ] {
            assert!(
                serde_json::from_value::<ContentBlock>(bad.clone()).is_err(),
                "{bad} was accepted"
            );
        }
    }

    fn png_image() -> Image {
        Image::from_bytes(&crate::images::testing::png(4, 3))
            .unwrap()
            .0
    }

    #[test]
    fn image_blocks_take_the_api_shape() {
        let image = png_image();
        let Source::Base64(data) = &image.source else {
            panic!();
        };
        let data = data.to_string();
        let block = ContentBlock::Image(image.clone());
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(
            value,
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                               "data": data}})
        );
        assert_eq!(
            serde_json::from_value::<ContentBlock>(value).unwrap(),
            block
        );

        // A tool result with images: a text block, then the images.
        let result = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "Image a.png".into(),
            is_error: false,
            images: vec![image.clone()],
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(
            value,
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "Image a.png"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                             "data": data}}
            ]})
        );
        assert_eq!(
            serde_json::from_value::<ContentBlock>(value).unwrap(),
            result
        );
        assert_eq!(result.images(), std::slice::from_ref(&image));
        assert!(ContentBlock::text("x").images().is_empty());

        // Without images the content stays a string, as before.
        assert_eq!(
            serde_json::to_value(ContentBlock::tool_result("t", "ok", true)).unwrap(),
            json!({"type": "tool_result", "tool_use_id": "t", "content": "ok", "is_error": true})
        );
        // Text-only lists (as the API also takes them) read as text.
        let listed = json!({"type": "tool_result", "tool_use_id": "t",
                            "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]});
        assert_eq!(
            serde_json::from_value::<ContentBlock>(listed).unwrap(),
            ContentBlock::tool_result("t", "a\nb", false)
        );
    }

    #[test]
    fn stored_images_name_their_file() {
        let sha = "ab".repeat(32);
        let block = ContentBlock::Image(Image {
            media_type: MediaType::Webp,
            source: Source::Stored {
                sha256: sha.clone(),
            },
        });
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(
            value,
            json!({"type": "image", "source": {"type": STORED_SOURCE, "media_type": "image/webp",
                                               "sha256": sha}})
        );
        assert_eq!(
            serde_json::from_value::<ContentBlock>(value).unwrap(),
            block
        );
    }

    #[test]
    fn image_blocks_must_parse() {
        for bad in [
            json!({"type": "image"}),
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/svg+xml",
                                               "data": "x"}}),
            json!({"type": "image", "source": {"type": "url", "url": "https://x"}}),
            json!({"type": "image", "source": {"type": "junees_file", "media_type": "image/png",
                                               "sha256": "../../etc/passwd"}}),
            json!({"type": "tool_result", "tool_use_id": "t",
                   "content": [{"type": "document", "source": {}}]}),
            json!({"type": "tool_result", "tool_use_id": "t", "content": 5}),
        ] {
            assert!(
                serde_json::from_value::<ContentBlock>(bad.clone()).is_err(),
                "{bad} was accepted"
            );
        }
    }

    #[test]
    fn raw_blocks_and_cache_marks_still_serialize() {
        // The request adds `cache_control` by flattening a block into a
        // struct of its own.
        #[derive(Serialize)]
        struct Marked<'a> {
            #[serde(flatten)]
            block: &'a ContentBlock,
            cache_control: Value,
        }
        let mark = json!({"type": "ephemeral"});
        for (block, kind) in [
            (ContentBlock::Image(png_image()), "image"),
            (ContentBlock::text("hi"), "text"),
            (
                ContentBlock::Other(json!({"type": "server_tool_use", "id": "s"})),
                "server_tool_use",
            ),
        ] {
            let value = serde_json::to_value(Marked {
                block: &block,
                cache_control: mark.clone(),
            })
            .unwrap();
            assert_eq!(value["type"], kind);
            assert_eq!(value["cache_control"], mark);
        }
    }

    #[test]
    fn message_text_joins_only_text_blocks() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("a"),
                ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({}),
                },
                ContentBlock::text("b"),
            ],
        };
        assert_eq!(message.text(), "a\nb");
    }

    #[test]
    fn unknown_stop_reason_maps_to_other() {
        let reason: StopReason = serde_json::from_value(json!("something_new")).unwrap();
        assert_eq!(reason, StopReason::Other);
        let reason: StopReason = serde_json::from_value(json!("tool_use")).unwrap();
        assert_eq!(reason, StopReason::ToolUse);
    }

    #[test]
    fn usage_adds_up() {
        let mut total = Usage::default();
        total.add(Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Default::default()
        });
        total.add(Usage {
            input_tokens: 5,
            output_tokens: 1,
            cache_creation_input_tokens: 7,
            cache_read_input_tokens: 9,
            ..Default::default()
        });
        total.add(Usage {
            cache_read_input_tokens: 1,
            ..Default::default()
        });
        assert_eq!(
            total,
            Usage {
                input_tokens: 15,
                output_tokens: 3,
                cache_creation_input_tokens: 7,
                cache_read_input_tokens: 10,
                ..Default::default()
            }
        );
    }

    #[test]
    fn context_size_counts_cached_input_and_output() {
        let usage = Usage {
            input_tokens: 5,
            output_tokens: 40,
            cache_creation_input_tokens: 300,
            cache_read_input_tokens: 9000,
            ..Default::default()
        };
        assert_eq!(usage.context_tokens(), 9345);
    }

    #[test]
    fn usage_writes_cache_counts_only_when_set() {
        let plain = Usage {
            input_tokens: 3,
            output_tokens: 1,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(plain).unwrap(),
            json!({"input_tokens": 3, "output_tokens": 1})
        );
        let cached = Usage {
            cache_read_input_tokens: 50,
            ..plain
        };
        let value = serde_json::to_value(cached).unwrap();
        assert_eq!(value["cache_read_input_tokens"], 50);
        assert_eq!(serde_json::from_value::<Usage>(value).unwrap(), cached);
    }
}
