//! `GET /api/sessions/<id>`: one session's conversation, a page at a
//! time, as the journal recorded it ([`crate::session::read_conversation`]).
//! Text is cut to a length and its secrets hidden first; images and
//! thinking go out as their kind only.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::api::{Failure, home, number, time};
use crate::config_file::Settings;
use crate::message::{ContentBlock, Role};

/// `limit` without one, and the most it may be.
const PAGE: (u32, u32) = (100, 200);
/// Characters of a text block, and of a tool call's input or a result.
const TEXT_CHARS: usize = 8_000;
pub(super) const PREVIEW_CHARS: usize = 2_000;
/// Characters of the end of a tool result cut at [`PREVIEW_CHARS`]
/// (`text_tail`), where a tool puts its status (a `draft`'s status line).
const TAIL_CHARS: usize = 500;
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
    // The title is what the person wrote first, not a note of Junees's.
    let first_line = conversation
        .first()
        .filter(|logged| logged.message.role == Role::User)
        .map(|logged| {
            logged
                .message
                .content
                .iter()
                .enumerate()
                .filter(|(index, _)| !logged.notices.contains(index))
                .filter_map(|(_, block)| match block {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
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
    let drafts = drafts(settings, &conversation);
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
                    .enumerate()
                    .map(|(index, block)| {
                        let mut out = block_json(settings, block);
                        // The result of a `draft` call: how it ended.
                        if let ContentBlock::ToolResult { tool_use_id, .. } = block
                            && let Some(draft) = drafts.get(tool_use_id)
                        {
                            out["draft"] = draft.clone();
                        }
                        // A note Junees put before the input (an undo's
                        // `[되돌림]` line), not the person's words.
                        if logged.notices.contains(&index) {
                            out["notice"] = Value::Bool(true);
                        }
                        out
                    })
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
        ContentBlock::ToolUse { id, name, input } => {
            out.insert("type".into(), json!("tool_use"));
            out.insert(
                "tool_use_id".into(),
                Value::String(cut(settings, id, 200).0),
            );
            out.insert("name".into(), Value::String(cut(settings, name, 200).0));
            put_text(&mut out, "input_preview", &input_text(input), PREVIEW_CHARS);
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            images,
        } => {
            out.insert("type".into(), json!("tool_result"));
            out.insert(
                "tool_use_id".into(),
                Value::String(cut(settings, tool_use_id, 200).0),
            );
            out.insert("is_error".into(), Value::Bool(*is_error));
            put_text(&mut out, "text_preview", content, PREVIEW_CHARS);
            // The end too, where tools put their status, and how much lies
            // between the two.
            if let Some((tail, omitted)) = tail(settings, content, PREVIEW_CHARS, TAIL_CHARS) {
                out.insert("text_tail".into(), Value::String(tail));
                out.insert("omitted_chars".into(), json!(omitted));
            }
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

/// How each `draft` call of the conversation ended, by tool use id: the
/// `draft` field of its result. `null` when its result does not read as a
/// draft's (its input was bad, the limit was used up before it, the turn
/// ended before it ran, no `draft` tool was offered) or there is none.
///
/// Everything comes from what Junees wrote: the result's opening words and
/// status line ([`crate::write::tool::read_status`], which never reads the
/// model's text) and the draft's token lines ([`crate::session::Logged`]
/// `draft_usage`), never from the text the model wrote. What the journal
/// does not keep is `null`: `by_level` always, and `cache_read_tokens`
/// when the token lines cannot be matched to the call ([`cache_reads`]).
fn drafts(settings: &Settings, conversation: &[crate::session::Logged]) -> HashMap<String, Value> {
    let mut drafts = HashMap::new();
    for (index, logged) in conversation.iter().enumerate() {
        if logged.message.role != Role::Assistant {
            continue;
        }
        let ids: Vec<&str> = logged
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, .. } if name == crate::write::tool::NAME => {
                    Some(id.as_str())
                }
                _ => None,
            })
            .collect();
        if ids.is_empty() {
            continue;
        }
        // The results are in the message after the calls.
        let results = conversation
            .get(index + 1)
            .filter(|next| next.message.role == Role::User);
        let statuses: Vec<Option<crate::write::tool::Status>> = ids
            .iter()
            .map(|id| {
                results?
                    .message
                    .content
                    .iter()
                    .find_map(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } if tool_use_id == id => {
                            crate::write::tool::read_status(content, *is_error)
                        }
                        _ => None,
                    })
            })
            .collect();
        let caches = cache_reads(&statuses, &logged.draft_usage);
        for ((id, status), cache) in ids.iter().zip(&statuses).zip(caches) {
            let draft = status
                .as_ref()
                .map_or(Value::Null, |status| draft_json(settings, status, cache));
            drafts.insert((*id).to_string(), draft);
        }
    }
    drafts
}

/// The cache reads of each `draft` call of one model reply, from the token
/// lines its calls wrote (`usage`, in call order, one line per answering
/// model and none for a call that used no tokens). Each call that ran
/// takes lines in turn until they add up to the tokens its status line
/// gives; calls without a status did not run and take none. When the lines
/// do not add up exactly (a provider that named no model writes an
/// unmarked line), no call gets a number: guessing would give one call
/// another's tokens.
fn cache_reads(
    statuses: &[Option<crate::write::tool::Status>],
    usage: &[crate::message::Usage],
) -> Vec<Option<u64>> {
    let mut lines = usage.iter();
    let mut caches = Vec::with_capacity(statuses.len());
    for status in statuses {
        let Some(status) = status else {
            caches.push(None);
            continue;
        };
        let (mut input, mut output, mut cache) = (0u64, 0u64, 0u64);
        while input < status.input_tokens || output < status.output_tokens {
            let Some(line) = lines.next() else {
                return vec![None; statuses.len()];
            };
            input +=
                line.input_tokens + line.cache_creation_input_tokens + line.cache_read_input_tokens;
            output += line.output_tokens;
            cache += line.cache_read_input_tokens;
        }
        if (input, output) != (status.input_tokens, status.output_tokens) {
            return vec![None; statuses.len()];
        }
        caches.push(Some(cache));
    }
    if lines.next().is_some() {
        return vec![None; statuses.len()];
    }
    caches
}

/// A `draft` result's `draft` field: `draft_done`'s fields, as far as the
/// journal keeps them.
fn draft_json(
    settings: &Settings,
    status: &crate::write::tool::Status,
    cache_read: Option<u64>,
) -> Value {
    json!({
        "stop": status.stop,
        "error": status.error.as_deref().map(|error| cut(settings, error, PREVIEW_CHARS).0),
        "levels": status.levels,
        "levels_done": status.levels_done,
        "polished": status.polished,
        "sections": status.sections,
        "failed_sections": status.failed_sections,
        "calls": status.calls,
        "input_tokens": status.input_tokens,
        "output_tokens": status.output_tokens,
        "cache_read_tokens": cache_read,
        "elapsed_ms": status.elapsed_ms,
        "sequential_ms": status.sequential_ms,
        "by_level": Value::Null,
    })
}

/// The end of `text` past its first `shown` characters, at most `chars`
/// of it, with its secrets hidden, and how many characters lie between
/// the two; `None` when `text` is no longer than `shown`. The end does
/// not overlap what [`cut`] shows. Secrets are hidden in what lies up to
/// [`SECRET_MARGIN`] before the end too, as [`cut`] does after its cut.
fn tail(settings: &Settings, text: &str, shown: usize, chars: usize) -> Option<(String, usize)> {
    let total = text.chars().count();
    let past = total.checked_sub(shown).filter(|past| *past > 0)?;
    let kept = past.min(chars);
    let start = total - kept;
    let mut near: String = text
        .chars()
        .skip(start.saturating_sub(SECRET_MARGIN))
        .collect();
    crate::doctor::hide_secrets_in_user_text(settings, std::iter::once(&mut near));
    let count = near.chars().count();
    let end: String = near.chars().skip(count.saturating_sub(kept)).collect();
    Some((end, start - shown))
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
    fn the_tail_of_a_cut_result_follows_the_preview_with_its_secrets_hidden() {
        let settings = settings();
        // Longer than preview and tail: a gap between them.
        let text = format!("{}{}", "가".repeat(2_500), "나".repeat(500));
        let (end, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert_eq!((end, omitted), ("나".repeat(500), 500));
        // Shorter: the tail starts where the preview ends.
        let text = format!("{}{}", "a ".repeat(1_000), "b ".repeat(60));
        assert_eq!(
            tail(&settings, &text, 2_000, 500),
            Some(("b ".repeat(60), 0))
        );
        assert_eq!(tail(&settings, &"a".repeat(2_000), 2_000, 500), None);
        assert_eq!(tail(&settings, "", 2_000, 500), None);
        // A key at the end, and one that crosses the tail's start: no piece
        // of either may show.
        let key = format!("sk-ant-api03-{}", "Q".repeat(40));
        // 53 characters each; the tail starts 26 characters into the first.
        let text = format!("{}{key}{}{key}", "a ".repeat(1_500), "b ".repeat(210));
        assert_eq!(text.chars().count() - 500, 3_026);
        let (end, _) = tail(&settings, &text, 2_000, 500).unwrap();
        assert!(!end.contains("sk-") && !end.contains("QQQQ"), "{end}");
        assert!(end.contains("b b b"), "{end}");
    }

    /// A draft's status as its result's status line gives it.
    fn ran(input_tokens: u64, output_tokens: u64) -> Option<crate::write::tool::Status> {
        Some(crate::write::tool::Status {
            stop: "done",
            error: None,
            levels: 3,
            levels_done: 3,
            polished: false,
            sections: 3,
            failed_sections: Vec::new(),
            calls: 2,
            input_tokens,
            output_tokens,
            elapsed_ms: 1_000,
            sequential_ms: None,
        })
    }

    fn line(input: u64, cache_read: u64, output: u64) -> crate::message::Usage {
        crate::message::Usage {
            input_tokens: input,
            cache_creation_input_tokens: 1,
            cache_read_input_tokens: cache_read,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[test]
    fn draft_token_lines_go_to_the_calls_they_add_up_to() {
        // Call one answered by two models, call two never ran (no status,
        // no lines), call three used no tokens, call four one model.
        let statuses = [
            ran(10 + 1 + 2 + 5 + 1 + 3, 7),
            None,
            ran(0, 0),
            ran(4 + 1 + 6, 9),
        ];
        let lines = [line(10, 2, 3), line(5, 3, 4), line(4, 6, 9)];
        assert_eq!(
            cache_reads(&statuses, &lines),
            [Some(5), None, Some(0), Some(6)]
        );
        // Lines that do not add up to a call, run out, or are left over:
        // no call gets a number.
        for lines in [
            vec![line(10, 2, 3), line(5, 3, 5), line(4, 6, 9)],
            vec![line(10, 2, 3), line(5, 3, 4)],
            vec![line(10, 2, 3), line(5, 3, 4), line(4, 6, 9), line(1, 0, 1)],
        ] {
            assert_eq!(cache_reads(&statuses, &lines), [None; 4], "{lines:?}");
        }
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
            json!({"type": "tool_use", "tool_use_id": "t", "name": "bash", "input_preview": "ls -la"})
        );
        let block = ContentBlock::tool_result("t", "x".repeat(2_001), true);
        let shown = block_json(&settings, &block);
        assert_eq!(shown["tool_use_id"], "t");
        assert_eq!(shown["is_error"], true);
        assert_eq!(shown["truncated"], true);
        assert_eq!(shown["text_preview"].as_str().unwrap().len(), 2_000);
        assert_eq!(
            (&shown["text_tail"], &shown["omitted_chars"]),
            (&json!("x"), &json!(0))
        );
        let shown = block_json(
            &settings,
            &ContentBlock::tool_result("t", "x".repeat(2_000), false),
        );
        assert!(
            shown.get("text_tail").is_none() && shown.get("truncated").is_none(),
            "{shown}"
        );
        let shown = block_json(&settings, &ContentBlock::text("y".repeat(8_000)));
        assert!(shown.get("truncated").is_none(), "{shown}");
    }
}
