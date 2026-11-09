//! Conversation types shared by the agent loop and LLM providers.
//!
//! The shapes follow the Anthropic Messages API so they serialize directly
//! into request bodies.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One block of a message. Junees works with text, tool calls and tool
/// results; every other block the API sends (thinking, redacted_thinking,
/// server tool calls and their results, ...) is kept as [`ContentBlock::Other`]
/// and sent back as it came.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", try_from = "Value")]
pub enum ContentBlock {
    Text {
        text: String,
        /// The sources the API attached to the text (web search results
        /// it cites), kept as it sent them: their `encrypted_index` must
        /// go back with the text in later requests.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        citations: Option<Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
    /// A block of another type, kept as the JSON object the API sent,
    /// `type` included. The API rejects thinking blocks that come back
    /// changed (their `signature` covers them), so it is never edited.
    #[serde(untagged)]
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
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl TryFrom<Value> for ContentBlock {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, String> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("a content block must be an object with a string `type`")?;
        if !matches!(kind, "text" | "tool_use" | "tool_result") {
            return Ok(ContentBlock::Other(value));
        }
        Ok(
            match serde_json::from_value(value).map_err(|err| err.to_string())? {
                // `citations: null` reads as `None`.
                Known::Text { text, citations } => ContentBlock::Text { text, citations },
                Known::ToolUse { id, name, input } => ContentBlock::ToolUse { id, name, input },
                Known::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                },
            },
        )
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
        self.server_tool_use.web_search_requests += other.server_tool_use.web_search_requests;
    }

    /// Web searches the API ran.
    pub fn web_searches(&self) -> u64 {
        self.server_tool_use.web_search_requests
    }

    /// Size of the conversation after this call: everything it read plus
    /// what it wrote, which the next request sends back.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_result_omits_is_error_when_false() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "ok".into(),
            is_error: false,
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
