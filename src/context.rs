//! What the model is asked when the conversation is compacted.
//!
//! The part of the history being replaced is sent as plain text in one user
//! message, so the summary call needs no tools and tool calls in it cannot
//! be left without their results.

use crate::message::{ContentBlock, Message, Role};

/// Output limit of a summary call.
pub const SUMMARY_MAX_TOKENS: u32 = 4096;

pub const SUMMARY_SYSTEM: &str = "\
You summarize the earlier part of a conversation between a user and an AI \
agent that works in the user's terminal, so that the agent can continue the \
work from your summary alone. Write down the user's goals, the decisions \
made, the files changed (with paths), the work that remains, and any \
important errors and how they were handled. Be specific and concise, and do \
not make anything up.";

/// The user message that asks for a summary of `messages`.
pub fn summary_request(messages: &[Message]) -> Message {
    let mut text = String::from("Summarize this conversation:\n\n<conversation>\n");
    for message in messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text: body } => {
                    text.push_str(&format!("[{role}]\n{body}\n"));
                }
                ContentBlock::ToolUse { id, name, input } => {
                    text.push_str(&format!("[tool call {id}: {name}] {input}\n"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let error = if *is_error { ", error" } else { "" };
                    text.push_str(&format!("[tool result {tool_use_id}{error}]\n{content}\n"));
                }
                ContentBlock::Unsupported => {}
            }
        }
    }
    text.push_str("</conversation>");
    Message::user_text(text)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_the_conversation_as_text() {
        let messages = [
            Message::user_text("list files"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "no such dir".into(),
                    is_error: true,
                }],
            },
        ];
        let request = summary_request(&messages);
        assert_eq!(request.role, Role::User);
        assert_eq!(
            request.text(),
            "Summarize this conversation:\n\n<conversation>\n\
             [user]\nlist files\n\
             [tool call toolu_1: bash] {\"command\":\"ls\"}\n\
             [tool result toolu_1, error]\nno such dir\n\
             </conversation>"
        );
    }
}
