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

/// Whether `c` can be part of a credential and so of the word a window's
/// edge may cut in two: ASCII graphic characters, bar the ones that
/// delimit a value in text (quotes, brackets, commas, backslash). Space,
/// line breaks and every non-ASCII character end a word.
fn word_char(c: char) -> bool {
    c.is_ascii_graphic()
        && !matches!(
            c,
            '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '{' | '}' | '[' | ']' | '<' | '>' | '\\'
        )
}

/// The header line of a private key block, which a window that starts
/// inside the block puts back in front of itself ([`tail`]).
const KEY_BEGIN: &str = concat!("-----BEGIN ", "PRIVATE KEY-----\n");

/// Whether byte `at` of `text` lies inside a private key block: after
/// the last `-----BEGIN ... PRIVATE KEY` line and before any `-----END`.
fn inside_private_key(text: &str, at: usize) -> bool {
    let Some(begin) = text[..at].rfind("-----BEGIN") else {
        return false;
    };
    let header = text[begin..].lines().next().unwrap_or("");
    header.contains("PRIVATE KEY") && !text[begin..at].contains("-----END")
}

/// The end of `text` past its first `shown` characters, at most `chars`
/// of it, with its secrets hidden, and how many characters lie between
/// the two; `None` when `text` is no longer than `shown`. Secrets are
/// hidden in what lies up to [`SECRET_MARGIN`] before the end too, as
/// [`cut`] does after its cut. Where that window starts inside a word,
/// the part of the word in the window is left out, only when that part
/// is [`SECRET_MARGIN`] characters or fewer: the secret check never saw
/// the whole word, so it cannot say that the rest is not a piece of a key.
/// A window that starts inside a private key block is hidden whole.
/// A hiding marker is never cut: when the end would start inside one and
/// lies away from the preview, it starts at the marker, so it can be a
/// few characters longer than `chars` (up to a marker's length), and
/// `omitted_chars` shrinks by as many; when the end follows the preview
/// directly (`omitted_chars` 0), the preview already ends with the whole
/// marker, so the end starts after it.
fn tail(settings: &Settings, text: &str, shown: usize, chars: usize) -> Option<(String, usize)> {
    let total = text.chars().count();
    let past = total.checked_sub(shown).filter(|past| *past > 0)?;
    let kept = past.min(chars);
    let start = total - kept;
    let mut window = start.saturating_sub(SECRET_MARGIN);
    if window > 0 {
        let mut chars_at = text.chars().skip(window - 1);
        if chars_at.next().is_some_and(word_char) {
            // A word that goes on past the margin keeps the window as it
            // is, as before: the check looks at the part of the word in
            // the window, and only if that part looks like a secret is the
            // word hidden whole; otherwise the window start is shown.
            let word = chars_at
                .take(SECRET_MARGIN + 1)
                .take_while(|c| word_char(*c))
                .count();
            if word <= SECRET_MARGIN {
                window += word;
            }
        }
    }
    let mut near: String = text.chars().skip(window).collect();
    if window > 0 {
        let byte = text
            .char_indices()
            .nth(window)
            .map_or(text.len(), |(at, _)| at);
        if inside_private_key(text, byte) {
            near.insert_str(0, KEY_BEGIN);
        }
    }
    crate::doctor::hide_secrets_in_user_text(settings, std::iter::once(&mut near));
    let count = near.chars().count();
    let mut from = count.saturating_sub(kept);
    let mut omitted = start - shown;
    if let Some((marker_start, marker_end)) = marker_across(&near, from) {
        if omitted == 0 {
            from = marker_end;
        } else {
            omitted = omitted.saturating_sub(from - marker_start);
            from = marker_start;
        }
    }
    // A field hidden whole is one marker that no `marker_across` finds
    // (it starts at 0); the preview shows it already.
    if omitted == 0 && crate::doctor::HIDDEN_MARKERS.contains(&near.as_str()) {
        from = count;
    }
    let end: String = near.chars().skip(from).collect();
    Some((end, omitted))
}

/// The character range of the hiding marker in `hidden` that the boundary
/// before character `at` falls inside of, if one does. A marker the
/// boundary only touches is whole on one side, so it does not count.
fn marker_across(hidden: &str, at: usize) -> Option<(usize, usize)> {
    crate::doctor::HIDDEN_MARKERS
        .iter()
        .flat_map(|marker| hidden.match_indices(marker))
        .map(|(byte, marker)| {
            let start = hidden[..byte].chars().count();
            (start, start + marker.chars().count())
        })
        .find(|(start, end)| *start < at && at < *end)
}

/// The first `chars` characters of `text` with its secrets hidden, and
/// whether it was cut. Secrets are hidden before the cut, in what lies
/// up to [`SECRET_MARGIN`] past it. Where that window ends inside a word,
/// the part of the word in the window is left out, only when that part
/// is [`SECRET_MARGIN`] characters or fewer: hiding can shorten the window to
/// `chars` or less, and what then shows its end is a piece of a word the
/// secret check never saw whole. A hiding marker is never cut: when the
/// cut would fall inside one, the text runs on to its end.
pub(super) fn cut(settings: &Settings, text: &str, chars: usize) -> (String, bool) {
    let window = chars + SECRET_MARGIN;
    let mut seen: Vec<char> = text.chars().take(window + 1).collect();
    let truncated = seen.len() > chars;
    let next = seen.get(window).copied();
    seen.truncate(window);
    if next.is_some_and(word_char) {
        // A longer word stays, as before: the check looks at the part of
        // it in the window, and only if that part looks like a secret is
        // the word hidden whole; otherwise the window end is shown.
        let word = seen.iter().rev().take_while(|c| word_char(**c)).count();
        if word <= SECRET_MARGIN {
            seen.truncate(seen.len() - word);
        }
    }
    let mut near: String = seen.into_iter().collect();
    crate::doctor::hide_secrets_in_user_text(settings, std::iter::once(&mut near));
    let upto = marker_across(&near, chars).map_or(chars, |(_, end)| end);
    let kept: String = near.chars().take(upto).collect();
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

    /// Every way a hiding marker can show up cut: a prefix or a suffix of
    /// one that is not the whole marker.
    fn marker_fragment(text: &str) -> bool {
        crate::doctor::HIDDEN_MARKERS.iter().any(|marker| {
            (1..marker.len())
                .filter(|n| marker.is_char_boundary(*n))
                .any(|n| text.starts_with(&marker[n..]) || text.ends_with(&marker[..n]))
        })
    }

    /// A field the hiding replaces as a whole (no word of it is a secret).
    const KEY_LINE: &str = concat!("-----BEGIN ", "PRIVATE KEY-----");
    /// A word the hiding replaces by `[hidden]`.
    const KEY_WORD: &str = concat!("ghp_", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4");

    #[test]
    fn a_whole_field_hidden_is_not_cut_to_a_fragment_by_the_tail() {
        let settings = settings();
        // 2,011 characters, a key in the last lines: the end window is one
        // marker, and 11 characters are left to show.
        let key = format!("\n{KEY_LINE}\n");
        let text = format!("{}{key}", "ab ".repeat((2_011 - key.len()) / 3 + 1));
        let text: String = text
            .chars()
            .take(2_011 - key.len())
            .chain(key.chars())
            .collect();
        assert_eq!(text.chars().count(), 2_011);
        let (tail, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert!(!marker_fragment(&tail), "{tail:?}");
        // The preview is that marker whole; the end follows it, so it has
        // nothing left to show.
        let shown = block_json(&settings, &ContentBlock::tool_result("t", text, false));
        assert_eq!(
            shown["text_preview"],
            "[hidden: it looks like it holds a credential]"
        );
        assert_eq!(tail, "");
        assert_eq!(omitted, 0);
    }

    #[test]
    fn a_hidden_word_on_the_edge_of_the_tail_is_shown_whole() {
        let settings = settings();
        // The 500-character end starts 4 characters into `[hidden]`.
        let end = format!("{KEY_WORD} {}", "ab ".repeat(165));
        let end = &end[..KEY_WORD.len() + 1 + 495];
        let text = format!("{}{end}", "ab ".repeat(900));
        let (tail, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert!(tail.starts_with("[hidden] "), "{tail:?}");
        assert!(!tail.contains("ghp_"), "{tail:?}");
        assert!(!marker_fragment(&tail), "{tail:?}");
        // 3,228 characters: the end starts at the 2,728th, 4 characters
        // inside the marker, so 728 - 4 lie between.
        assert_eq!(text.chars().count(), 3_228);
        assert_eq!(omitted, 724);
    }

    #[test]
    fn a_hidden_word_on_the_edge_of_the_preview_is_shown_whole() {
        let settings = settings();
        // The word starts 3 characters before the 2,000th: its marker would
        // be cut after `[hi`.
        let text = format!(
            "{}{KEY_WORD} {}",
            "ab ".repeat(665) + "a",
            "cd ".repeat(100)
        );
        assert_eq!(text.chars().take(1_996).count(), 1_996);
        let shown = block_json(&settings, &ContentBlock::tool_result("t", text, false));
        let preview = shown["text_preview"].as_str().unwrap();
        assert!(preview.ends_with("[hidden]"), "{preview:?}");
        assert!(!marker_fragment(preview), "{preview:?}");
        assert_eq!(shown["truncated"], true);
    }

    /// A word the secret check takes for a token (120 characters with upper
    /// case, lower case and digits): hidden whole, 8 characters are left.
    fn long_token() -> String {
        "Ab1".repeat(40)
    }

    /// `len` characters of plain words, the last one a space.
    fn filler(len: usize) -> String {
        let mut text: String = "ab ".repeat(len / 3 + 1).chars().take(len - 1).collect();
        text.push(' ');
        text
    }

    /// The part of a block's text a client reads: the preview and the end
    /// joined where `omitted_chars` says nothing lies between.
    fn shown_parts(text: String) -> (String, String, u64) {
        let shown = block_json(&settings(), &ContentBlock::tool_result("t", text, false));
        (
            shown["text_preview"].as_str().unwrap().to_string(),
            shown["text_tail"].as_str().unwrap().to_string(),
            shown["omitted_chars"].as_u64().unwrap(),
        )
    }

    #[test]
    fn a_word_cut_at_the_window_end_is_not_shown_when_hiding_shortens_the_window() {
        let settings = settings();
        // 12 long tokens in front: hiding takes 12 * 112 characters off, far
        // more than the margin, so the whole window is left under the 2,000.
        let head: String = (0..12).map(|_| format!("{} ", long_token())).collect();
        assert_eq!(head.chars().count(), 12 * 121);
        // The key starts 15 characters before the window's end (2,512): of
        // its `ghp_` and 28 characters, only 11 would be in the window.
        let at = 2_000 + SECRET_MARGIN - 15;
        let text = format!(
            "{head}{}{KEY_WORD} {}",
            filler(at - head.chars().count()),
            "cd ".repeat(100)
        );
        assert_eq!(text.find(KEY_WORD), Some(at));
        let (shown, truncated) = cut(&settings, &text, 2_000);
        assert!(truncated);
        assert!(!shown.contains("ghp_"), "{shown:?}");
        assert!(!shown.contains("a1B2c3D4"), "{shown:?}");
        // What is left is the 12 hidden tokens and the plain words.
        assert_eq!(shown.chars().count(), 12 * 9 + (at - head.chars().count()));
    }

    #[test]
    fn a_word_cut_at_the_window_start_of_the_tail_is_not_shown_when_hiding_shortens_the_window() {
        let settings = settings();
        // The window of the end starts 17 characters into the key, so its
        // last 15 are in the window; 6 long tokens after them shrink the
        // window to under the 500 the end keeps.
        let before = format!("{}{}", filler(2_583), &KEY_WORD[..17]);
        assert_eq!(before.chars().count(), 2_600);
        let near = format!(
            "{} {}{}",
            &KEY_WORD[17..],
            (0..6)
                .map(|_| format!("{} ", long_token()))
                .collect::<String>(),
            "cd ".repeat(90)
        );
        assert_eq!(near.chars().count(), 1_012);
        let text = format!("{before}{near}");
        let (end, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert!(!end.contains("7H8i9J0k"), "{end:?}");
        assert!(!end.contains("9J0k1L2m"), "{end:?}");
        assert_eq!(omitted, text.chars().count() - 500 - 2_000);
    }

    /// Lines of a private key's body: no word of them reads as a token.
    fn key_body(lines: usize) -> String {
        "abcdefghijklmnopqrstuvwxyzabcd/abcdefghijklmnopqrstuvwxyzabcd/\n".repeat(lines)
    }

    #[test]
    fn the_tail_of_a_text_inside_a_private_key_block_is_hidden_whole() {
        let settings = settings();
        let text = format!("{KEY_LINE}\n{}", key_body(60));
        let (end, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert_eq!(end, "[hidden: it looks like it holds a credential]");
        assert_eq!(omitted, text.chars().count() - 500 - 2_000);
        // A block that ended before the end window is no reason to hide it.
        let text = format!(
            "{KEY_LINE}\n{}-----END PRIVATE KEY-----\n{}",
            key_body(2),
            "ab ".repeat(1_500)
        );
        let (end, _) = tail(&settings, &text, 2_000, 500).unwrap();
        assert_eq!(end.len(), 500);
        assert!(!end.contains("[hidden"), "{end:?}");
    }

    #[test]
    fn a_private_key_block_in_the_window_of_the_preview_hides_all_of_it() {
        let text = format!("{}\n{KEY_LINE}\n{}", filler(1_990), key_body(60));
        let (shown, truncated) = cut(&settings(), &text, 2_000);
        assert!(truncated);
        assert_eq!(shown, "[hidden: it looks like it holds a credential]");
    }

    #[test]
    fn a_long_word_without_secrets_is_cut_where_it_was_before() {
        let (shown, truncated) = cut(&settings(), &"a".repeat(3_000), 2_000);
        assert_eq!((shown.len(), truncated), (2_000, true));
        let (end, omitted) = tail(&settings(), &"a".repeat(3_000), 2_000, 500).unwrap();
        assert_eq!((end.len(), omitted), (500, 500));
    }

    #[test]
    fn a_marker_is_across_a_boundary_only_when_the_boundary_is_inside_it() {
        let hidden = "ab[hidden]cd";
        // The marker is characters 2 to 10.
        assert_eq!(marker_across(hidden, 2), None);
        assert_eq!(marker_across(hidden, 3), Some((2, 10)));
        assert_eq!(marker_across(hidden, 9), Some((2, 10)));
        assert_eq!(marker_across(hidden, 10), None);
        // Several markers: only the one the boundary is inside of.
        let hidden = "[hidden] [hidden]";
        assert_eq!(marker_across(hidden, 4), Some((0, 8)));
        assert_eq!(marker_across(hidden, 8), None);
        assert_eq!(marker_across(hidden, 9), None);
        assert_eq!(marker_across(hidden, 12), Some((9, 17)));
        // Characters before the marker are counted as characters, not bytes.
        let hidden = "가나다[hidden]라";
        assert_eq!(marker_across(hidden, 3), None);
        assert_eq!(marker_across(hidden, 4), Some((3, 11)));
        assert_eq!(marker_across(hidden, 11), None);
        // The long marker too.
        let long = format!("x{}y", crate::doctor::HIDDEN_MARKERS[1]);
        assert_eq!(marker_across(&long, 1), None);
        assert_eq!(marker_across(&long, 2), Some((1, 46)));
        assert_eq!(marker_across(&long, 46), None);
    }

    #[test]
    fn a_boundary_that_only_touches_a_marker_does_not_move() {
        let settings = settings();
        // The preview's boundary is right before the marker, and right
        // after it: the preview is exactly 2,000 characters either way.
        let rest = format!("{KEY_WORD} {}", "cd ".repeat(50));
        let (shown, _) = cut(&settings, &format!("{}{rest}", filler(2_000)), 2_000);
        assert_eq!(shown, filler(2_000));
        let (shown, _) = cut(&settings, &format!("{}{rest}", filler(1_992)), 2_000);
        assert_eq!(shown, format!("{}[hidden]", filler(1_992)));
        // The same for the start of the end: 500 characters after the
        // marker, and 508 with it.
        let after = format!(
            " {}",
            "cd ".repeat(167).chars().take(499).collect::<String>()
        );
        assert_eq!(after.chars().count(), 500);
        let text = format!("{}{KEY_WORD}{after}", "ab ".repeat(900));
        assert_eq!(text.chars().count(), 3_232);
        assert_eq!(
            tail(&settings, &text, 2_000, 500),
            Some((after.clone(), 732))
        );
        assert_eq!(
            tail(&settings, &text, 2_000, 508),
            Some((format!("[hidden]{after}"), 724))
        );
    }

    #[test]
    fn several_markers_and_wide_characters_in_front_leave_the_counts_right() {
        let settings = settings();
        // Hangul (3 bytes each) before a word on the edge of the preview:
        // the marker's place is found by characters, so the preview still
        // runs to the end of the marker the cut is inside of.
        let text = format!(
            "{}{KEY_WORD} {KEY_WORD} {}",
            "가나다 ".repeat(499),
            "cd ".repeat(100)
        );
        let before = text.find(KEY_WORD).unwrap();
        assert_eq!(text[..before].chars().count(), 1_996);
        let (shown, _) = cut(&settings, &text, 2_000);
        assert!(shown.ends_with("[hidden]"), "{shown:?}");
        assert_eq!(shown.chars().count(), 1_996 + 8);
        // The second marker is not part of it.
        assert_eq!(shown.matches("[hidden]").count(), 1);
    }

    #[test]
    fn a_hidden_word_across_the_end_of_the_preview_shows_once() {
        // The word starts 30 characters before the 2,000th and hiding takes
        // 24 off it: the end starts inside its marker, which the preview
        // already shows whole.
        let text = format!("{}{KEY_WORD} {}", "x ".repeat(985), "cd ".repeat(89));
        let (preview, end, omitted) = shown_parts(text);
        assert_eq!(omitted, 0);
        assert_eq!(format!("{preview}{end}").matches("[hidden]").count(), 1);
        // A field hidden whole, in the last lines: one marker, not two.
        let key = format!("\n{KEY_LINE}\n");
        let text: String = "ab "
            .repeat(700)
            .chars()
            .take(2_011 - key.len())
            .chain(key.chars())
            .collect();
        let (preview, end, omitted) = shown_parts(text);
        assert_eq!(omitted, 0);
        assert_eq!(format!("{preview}{end}").matches("[hidden").count(), 1);
    }

    #[test]
    fn a_field_hidden_whole_shows_its_marker_once_for_any_length_past_the_preview() {
        let key = format!("\n{KEY_LINE}\n");
        // 45 to 500 characters past the preview: the end window is the
        // marker alone.
        for total in [2_046, 2_100, 2_500] {
            let text: String = "ab "
                .repeat(900)
                .chars()
                .take(total - key.len())
                .chain(key.chars())
                .collect();
            assert_eq!(text.chars().count(), total);
            let (preview, end, omitted) = shown_parts(text);
            assert_eq!(omitted, 0, "{total}");
            assert_eq!(
                format!("{preview}{end}").matches("[hidden").count(),
                1,
                "{total}"
            );
        }
    }

    #[test]
    fn a_word_longer_than_the_margin_at_the_end_of_the_preview_window_is_hidden_whole() {
        let settings = settings();
        // The token is 702 characters; 543, 513 and 512 of them are in the
        // window (it ends at 2,512). As on main, the secret check sees them
        // all (or, at 512, they are held back), so no piece of it shows.
        for start in [1_969, 1_999, 2_000] {
            let text = format!("{}{} tail", filler(start), "Ab1".repeat(234));
            let (shown, truncated) = cut(&settings, &text, 2_000);
            assert!(truncated);
            assert!(!shown.contains("Ab1"), "{start}: {shown:?}");
            if start == 2_000 {
                // Exactly 512 characters of the token are in the window:
                // they are left out, so the text ends where the cut is.
                assert_eq!(shown, filler(2_000));
            } else {
                // 543 and 513 are more than the margin: the window stays,
                // the token is hidden whole and the marker is not cut.
                assert!(shown.ends_with("[hidden]"), "{start}: {shown:?}");
                assert_eq!(shown, format!("{}[hidden]", filler(start)), "{start}");
                assert_eq!(shown.chars().count(), start + 8, "{start}");
            }
        }
    }

    #[test]
    fn a_word_of_512_characters_in_the_tail_window_is_left_out_and_one_of_513_is_not() {
        let settings = settings();
        // A 564-character token from 2,600; the window of the end starts
        // inside it, with 512 or 513 of its characters in the window.
        let word = "Ab1".repeat(188);
        let rest = |len: usize| -> String {
            format!(" {}", "cd ".repeat(200))
                .chars()
                .take(len)
                .collect()
        };
        // 512: the token's end is 500 characters before the text's end.
        let text = format!("{}{word}{}", filler(2_600), rest(500));
        assert_eq!(text.chars().count(), 3_664);
        let (end, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert_eq!(end, rest(500));
        assert_eq!(omitted, 3_164 - 2_000);
        // 513: one character more of it is in the window, so the window
        // stays and the check hides the piece whole.
        let text = format!("{}{word}{}", filler(2_600), rest(499));
        assert_eq!(text.chars().count(), 3_663);
        let (end, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert_eq!(end, format!("[hidden]{}", rest(499)));
        assert_eq!(omitted, 3_163 - 2_000 - 7);
    }

    #[test]
    fn a_word_longer_than_the_margin_at_the_start_of_the_tail_window_is_hidden_whole() {
        let settings = settings();
        // A 564-character token from 2,600 to 3,164, which is 20 characters
        // into the last 500 (the window of the end starts at 2,632, inside
        // the token and 532 characters before the token's end).
        let rest: String = format!(" {}", "cd ".repeat(160))
            .chars()
            .take(480)
            .collect();
        let text = format!("{}{}{rest}", filler(2_600), "Ab1".repeat(188));
        assert_eq!(text.chars().count(), 3_644);
        let (end, omitted) = tail(&settings, &text, 2_000, 500).unwrap();
        assert!(!end.contains("Ab1"), "{end:?}");
        assert_eq!(omitted, 3_144 - 2_000);
    }
}
