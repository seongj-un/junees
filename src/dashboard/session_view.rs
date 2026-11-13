//! `GET /api/sessions/<id>`: one session's conversation, a page at a
//! time, as the journal recorded it ([`crate::session::read_conversation`]).
//! Text is cut to a length and its secrets hidden first; images and
//! thinking go out as their kind only.

use serde_json::{Map, Value, json};

use super::api::{Failure, home, number, time};
use crate::config_file::Settings;
use crate::message::{ContentBlock, Role};

/// `limit` without one, and the most it may be.
const PAGE: (u32, u32) = (100, 200);
/// Characters of a text block, and of a tool call's input or a result.
const TEXT_CHARS: usize = 8_000;
pub(super) const PREVIEW_CHARS: usize = 2_000;
/// Characters kept past a cut before secrets are hidden: a secret that
/// crosses the cut is whole within it, so it is found and hidden.
const SECRET_MARGIN: usize = 512;
/// The longest session id: `[A-Za-z0-9_-]`, 1 to this many.
const ID_CHARS: usize = 64;

/// Whether `id` may name a session: 1 to 64 of `A-Z`, `a-z`, `0-9`, `_`
/// and `-`, so it can never leave the sessions folder.
pub(crate) fn valid_id(id: &str) -> bool {
    (1..=ID_CHARS).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `offset` (0 or more, default 0) and `limit` (1 to 200, default 100).
pub(super) fn paging(query: &str) -> Result<(usize, usize), Failure> {
    let limit = number(query, "limit", PAGE)? as usize;
    let mut offset = 0usize;
    for pair in query.split('&') {
        let (key, text) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "offset" {
            offset = text.parse::<u32>().map_err(|_| {
                Failure::bad_query("offset must be a whole number, 0 or more".to_string())
            })? as usize;
        }
    }
    Ok((offset, limit))
}

/// The page of session `id` from message `offset`, at most `limit`
/// messages.
pub(super) fn detail(
    settings: &Settings,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<Value, Failure> {
    if !valid_id(id) {
        return Err(Failure::new(
            400,
            "bad_id",
            "a session id is 1 to 64 letters, digits, `_` or `-`",
        ));
    }
    let not_found = || Failure::new(404, "not_found", "there is no such session");
    let home = home(settings).ok_or_else(not_found)?;
    let path = crate::session::sessions_dir(&home).join(format!("{id}.jsonl"));
    // Not followed: a link in the sessions folder is not a session.
    let meta = std::fs::symlink_metadata(&path).map_err(|_| not_found())?;
    if !meta.is_file() {
        return Err(not_found());
    }
    let head = crate::session::read_meta(&path).ok_or_else(not_found)?;
    let conversation =
        crate::session::read_conversation(&path).map_err(|err| Failure::internal(&err))?;
    let first_line = conversation
        .first()
        .filter(|logged| logged.message.role == Role::User)
        .map(|logged| logged.message.text())
        .unwrap_or_default();
    let first_line = first_line
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let updated = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(Value::Null, |since| {
            time(i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
        });
    let messages: Vec<Value> = conversation
        .iter()
        .enumerate()
        .skip(offset)
        .take(limit)
        .map(|(index, logged)| {
            json!({
                "index": index,
                "role": match logged.message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                "at": logged.at.map_or(Value::Null, |at| time(i64::try_from(at).unwrap_or(i64::MAX))),
                "blocks": logged
                    .message
                    .content
                    .iter()
                    .map(|block| block_json(settings, block))
                    .collect::<Vec<Value>>(),
            })
        })
        .collect();
    Ok(json!({
        "id": id,
        "title": cut(settings, first_line, 80).0,
        "cwd": head.cwd,
        "updated_at": updated,
        "total": conversation.len(),
        "offset": offset,
        "messages": messages,
    }))
}

/// [`detail`] for the rest of Junees (`junees mcp serve`'s
/// `junees_session`): the same page, or why there is none, in words.
pub(crate) fn page(
    settings: &Settings,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    detail(settings, id, offset, limit).map_err(|failure| failure.message)
}

/// One block as the API shows it.
fn block_json(settings: &Settings, block: &ContentBlock) -> Value {
    let mut out = Map::new();
    let put_text = |out: &mut Map<String, Value>, key: &str, text: &str, chars: usize| {
        let (text, truncated) = cut(settings, text, chars);
        out.insert(key.into(), Value::String(text));
        if truncated {
            out.insert("truncated".into(), Value::Bool(true));
        }
    };
    match block {
        ContentBlock::Text { text, .. } => {
            out.insert("type".into(), json!("text"));
            put_text(&mut out, "text", text, TEXT_CHARS);
        }
        ContentBlock::ToolUse { name, input, .. } => {
            out.insert("type".into(), json!("tool_use"));
            out.insert("name".into(), Value::String(cut(settings, name, 200).0));
            put_text(&mut out, "input_preview", &input_text(input), PREVIEW_CHARS);
        }
        ContentBlock::ToolResult {
            content,
            is_error,
            images,
            ..
        } => {
            out.insert("type".into(), json!("tool_result"));
            out.insert("is_error".into(), Value::Bool(*is_error));
            put_text(&mut out, "text_preview", content, PREVIEW_CHARS);
            // Screenshots and the like: their kind only, as image blocks.
            if !images.is_empty() {
                let kinds: Vec<&str> = images
                    .iter()
                    .map(|image| image.media_type.as_str())
                    .collect();
                out.insert("images".into(), json!(kinds));
            }
        }
        ContentBlock::Image(image) => {
            out.insert("type".into(), json!("image"));
            out.insert("media_type".into(), json!(image.media_type.as_str()));
        }
        ContentBlock::Other(_) => match block.other_type() {
            Some("thinking" | "redacted_thinking") => {
                out.insert("type".into(), json!("thinking"));
            }
            // Server tool calls and their results (web search): the kind
            // only, as for thinking.
            other => {
                out.insert("type".into(), json!("other"));
                let kind: String = other
                    .unwrap_or("unknown")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(64)
                    .collect();
                out.insert("kind".into(), Value::String(kind));
            }
        },
    }
    Value::Object(out)
}

/// A tool call's input as text: the command of a `bash` call (or the
/// only string of an input with one field), else its JSON.
pub(super) fn input_text(input: &Value) -> String {
    if let Some(object) = input.as_object() {
        if let Some(command) = object.get("command").and_then(Value::as_str) {
            return command.to_string();
        }
        if object.len() == 1
            && let Some(text) = object.values().next().and_then(Value::as_str)
        {
            return text.to_string();
        }
    }
    input.to_string()
}

/// The first `chars` characters of `text` with its secrets hidden, and
/// whether it was cut. Secrets are hidden before the cut, in what lies
/// up to [`SECRET_MARGIN`] past it.
pub(super) fn cut(settings: &Settings, text: &str, chars: usize) -> (String, bool) {
    let mut near: String = text.chars().take(chars + SECRET_MARGIN).collect();
    crate::doctor::hide_secrets_in_user_text(settings, std::iter::once(&mut near));
    let truncated = text.chars().nth(chars).is_some();
    let kept: String = near.chars().take(chars).collect();
    // Hiding can make the text longer or shorter; what counts is whether
    // the original went on past the cut.
    (kept, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::new(
            |_: &str| None,
            crate::config_file::ConfigFile::default(),
            None,
            false,
        )
    }

    #[test]
    fn ids_cannot_leave_the_sessions_folder() {
        for id in ["20261113-010203-abcd", "a", "A_b-9", &"x".repeat(64)] {
            assert!(valid_id(id), "{id}");
        }
        for id in [
            "",
            "..",
            "../x",
            "a/b",
            "a.jsonl",
            "a b",
            "%2e%2e",
            "é",
            &"x".repeat(65),
        ] {
            assert!(!valid_id(id), "{id}");
        }
    }

    #[test]
    fn paging_is_checked() {
        assert_eq!(paging("").unwrap(), (0, 100));
        assert_eq!(paging("offset=5&limit=200").unwrap(), (5, 200));
        for query in ["offset=-1", "offset=x", "offset=", "limit=0", "limit=201"] {
            assert_eq!(paging(query).unwrap_err().status, 400, "{query}");
        }
    }

    #[test]
    fn text_is_cut_after_its_secrets_are_hidden() {
        let key = "sk-ant-api03-Abc1Def2Ghi3Jkl4Mno5Pqr6Stu7Vwx8";
        // The key crosses the cut: no piece of it may show.
        let text = format!("{}{key} tail", "a".repeat(20));
        let (shown, truncated) = cut(&settings(), &text, 30);
        assert!(truncated);
        assert!(!shown.contains("sk-"), "{shown}");
        assert_eq!(cut(&settings(), "short", 30), ("short".to_string(), false));
        let (exact, truncated) = cut(&settings(), &"b".repeat(30), 30);
        assert_eq!((exact.len(), truncated), (30, false));
    }

    #[test]
    fn blocks_show_their_kind_and_no_image_data_or_thinking() {
        let settings = settings();
        let block: ContentBlock = serde_json::from_value(json!({
            "type": "thinking", "thinking": "secret plan", "signature": "sig"
        }))
        .unwrap();
        assert_eq!(block_json(&settings, &block), json!({"type": "thinking"}));
        let block: ContentBlock = serde_json::from_value(json!({
            "type": "server_tool_use", "id": "x", "name": "web_search", "input": {}
        }))
        .unwrap();
        assert_eq!(
            block_json(&settings, &block),
            json!({"type": "other", "kind": "server_tool_use"})
        );
        let block = ContentBlock::ToolUse {
            id: "t".into(),
            name: "bash".into(),
            input: json!({"command": "ls -la", "timeout": 5}),
        };
        assert_eq!(
            block_json(&settings, &block),
            json!({"type": "tool_use", "name": "bash", "input_preview": "ls -la"})
        );
        let block = ContentBlock::tool_result("t", "x".repeat(2_001), true);
        let shown = block_json(&settings, &block);
        assert_eq!(shown["is_error"], true);
        assert_eq!(shown["truncated"], true);
        assert_eq!(shown["text_preview"].as_str().unwrap().len(), 2_000);
        let shown = block_json(&settings, &ContentBlock::text("y".repeat(8_000)));
        assert!(shown.get("truncated").is_none(), "{shown}");
    }
}
