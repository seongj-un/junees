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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
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
    /// Block types Junees does not handle yet (thinking, server tools, ...).
    /// Providers drop these before they reach the conversation history.
    #[serde(other)]
    Unsupported,
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
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// All text blocks of the message joined with newlines.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
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
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
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
    fn unknown_block_types_deserialize_as_unsupported() {
        let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
            {"type": "thinking", "thinking": "hmm", "signature": "abc"},
            {"type": "text", "text": "hi", "citations": null},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {"command": "ls"}}
        ]))
        .unwrap();
        assert_eq!(blocks[0], ContentBlock::Unsupported);
        assert_eq!(blocks[1], ContentBlock::Text { text: "hi".into() });
        assert!(matches!(&blocks[2], ContentBlock::ToolUse { name, .. } if name == "bash"));
    }

    #[test]
    fn message_text_joins_only_text_blocks() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({}),
                },
                ContentBlock::Text { text: "b".into() },
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
