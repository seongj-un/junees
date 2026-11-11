//! What the model is asked when the conversation is compacted.
//!
//! The part of the history being replaced is sent as plain text in one user
//! message, so the summary call needs no tools and tool calls in it cannot
//! be left without their results.

use crate::message::{ContentBlock, Message, Role};
use crate::web_search;

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
                ContentBlock::Text { text: body, .. } => {
                    text.push_str(&format!("[{role}]\n{body}\n"));
                }
                ContentBlock::ToolUse { id, name, input } => {
                    text.push_str(&format!("[tool call {id}: {name}] {input}\n"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    images,
                } => {
                    let error = if *is_error { ", error" } else { "" };
                    text.push_str(&format!("[tool result {tool_use_id}{error}]\n{content}\n"));
                    for image in images {
                        text.push_str(&format!("[image: {}]\n", image.describe()));
                    }
                }
                // Images by their format and size: the summary call is
                // text only, and the model saw them when they came.
                ContentBlock::Image(image) => {
                    text.push_str(&format!("[{role}: image, {}]\n", image.describe()));
                }
                // Thinking is the model's own working; its outcome is in
                // the text and tool calls around it.
                block if block.is_thinking() => {}
                // Web searches by their query and results by title and
                // URL: the pages themselves would blow up the summary call.
                // Other raw blocks by their type.
                ContentBlock::Other(value) => {
                    let line = web_search::summary_line(value).unwrap_or_else(|| {
                        let kind = value["type"].as_str().unwrap_or("unknown");
                        format!("{kind} block")
                    });
                    text.push_str(&format!("[{role}: {line}]\n"));
                }
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
    fn images_are_named_not_sent() {
        let image = crate::images::Image::from_bytes(&crate::images::testing::png(30, 20))
            .unwrap()
            .0;
        let messages = [
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Image(image.clone()),
                    ContentBlock::text("what is it?"),
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "Image a.png".into(),
                    is_error: false,
                    images: vec![image],
                }],
            },
        ];
        let request = summary_request(&messages);
        assert_eq!(request.content.len(), 1);
        let text = request.text();
        assert!(
            text.contains("[user: image, image/png, 30x20 px]\n[user]\nwhat is it?\n"),
            "{text}"
        );
        assert!(
            text.contains("[tool result toolu_1]\nImage a.png\n[image: image/png, 30x20 px]\n"),
            "{text}"
        );
        assert!(!text.contains("iVBOR"), "{text}");
    }

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
                    images: Vec::new(),
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

    #[test]
    fn skips_thinking_and_labels_other_raw_blocks() {
        let messages = [
            Message::user_text("search it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Other(json!({"type": "thinking", "thinking": "secret plan",
                                               "signature": "sig"})),
                    ContentBlock::Other(json!({"type": "redacted_thinking", "data": "opaque"})),
                    ContentBlock::Other(json!({"type": "server_tool_use", "id": "srvtoolu_1",
                                               "name": "web_search", "input": {}})),
                    ContentBlock::text("found it"),
                ],
            },
        ];
        assert_eq!(
            summary_request(&messages).text(),
            "Summarize this conversation:\n\n<conversation>\n\
             [user]\nsearch it\n\
             [assistant: web search]\n\
             [assistant]\nfound it\n\
             </conversation>"
        );
    }

    #[test]
    fn web_search_blocks_shrink_to_queries_titles_and_urls() {
        let results: Vec<_> = (0..20)
            .map(|i| {
                json!({"type": "web_search_result", "url": format!("https://{i}.example/"),
                       "title": format!("Page {i}"), "encrypted_content": "E".repeat(50_000)})
            })
            .collect();
        let messages = [
            Message::user_text("what is new in rust?"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Other(json!({"type": "server_tool_use", "id": "srvtoolu_1",
                        "name": "web_search", "input": {"query": "rust release notes"}})),
                    ContentBlock::Other(json!({"type": "web_search_tool_result",
                        "tool_use_id": "srvtoolu_1", "content": results})),
                    ContentBlock::Other(json!({"type": "server_tool_use", "id": "srvtoolu_2",
                        "name": "code_execution", "input": {"code": "print(1)"}})),
                    ContentBlock::Text {
                        text: "Rust 1.95 is out.".into(),
                        citations: Some(json!([{"type": "web_search_result_location",
                            "url": "https://0.example/", "encrypted_index": "Eo8B"}])),
                    },
                ],
            },
        ];
        let text = summary_request(&messages).text();
        assert_eq!(
            text,
            "Summarize this conversation:\n\n<conversation>\n\
             [user]\nwhat is new in rust?\n\
             [assistant: web search: rust release notes]\n\
             [assistant: web search results: Page 0 (https://0.example/); \
             Page 1 (https://1.example/); Page 2 (https://2.example/); \
             Page 3 (https://3.example/); Page 4 (https://4.example/); 15 more]\n\
             [assistant: server_tool_use block]\n\
             [assistant]\nRust 1.95 is out.\n\
             </conversation>"
        );
    }
}
