//! Cutting a reply into Discord messages: at most 2000 characters each
//! (Create Message, `content`), on line breaks where possible, with a code
//! block that spans a cut closed at the end of one message and opened
//! again, with the same language, at the start of the next. Plain-text
//! platforms (Telegram) cut the same way without touching code blocks
//! ([`split_plain`]): a fence there is shown as typed.

/// The longest `content` of a message.
pub const MESSAGE_LIMIT: usize = 2000;

/// The length Discord's limit counts. UTF-16 code units are never fewer
/// than characters, so a message within the limit by this count is within
/// it however Discord counts.
pub fn length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text` as messages of at most `limit` (at least 16) units each. Blank
/// messages are left out: Discord refuses empty content.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    split(text, limit, true)
}

/// [`split_message`] for plain text: no code block is closed or opened
/// again at a cut, so the messages joined by line breaks are `text`.
pub fn split_plain(text: &str, limit: usize) -> Vec<String> {
    split(text, limit, false)
}

fn split(text: &str, limit: usize, code_blocks: bool) -> Vec<String> {
    let limit = limit.max(16);
    let mut out = Splitter {
        limit,
        code_blocks,
        messages: Vec::new(),
        current: String::new(),
        fence: None,
    };
    for line in text.trim_end().split('\n') {
        out.line(line);
    }
    out.finish()
}

struct Splitter {
    limit: usize,
    /// Whether code blocks are kept whole across cuts.
    code_blocks: bool,
    messages: Vec<String>,
    current: String,
    /// How the code block the text is in opens again in the next message
    /// (```` ```rust ````, see [`reopen_line`]).
    fence: Option<String>,
}

const CLOSE: &str = "```";

/// The longest language a reopened code block repeats.
const MAX_LANGUAGE: usize = 32;

impl Splitter {
    fn line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        let toggles = match self.fence {
            _ if !self.code_blocks => false,
            Some(_) => trimmed.starts_with(CLOSE),
            // ```` ```code``` ```` closes on its own line: no block.
            None => trimmed
                .strip_prefix(CLOSE)
                .is_some_and(|info| !info.contains(CLOSE)),
        };
        // A line opening a block needs room for the block's closing after
        // it, or the message could not be closed within the limit.
        let opening = if toggles && self.fence.is_none() {
            length(CLOSE) + 1
        } else {
            0
        };
        // A fence line itself fits whole; a long ordinary line is cut.
        let mut rest = line;
        loop {
            let room = self.room().saturating_sub(opening);
            let sep = usize::from(!self.current.is_empty());
            if length(rest) + sep <= room {
                if sep == 1 {
                    self.current.push('\n');
                }
                self.current.push_str(rest);
                break;
            }
            if self.current.is_empty() || self.current == self.reopen() {
                // Not even an empty message has room: cut the line.
                let (head, tail) = cut(rest, room.saturating_sub(sep).max(1));
                if sep == 1 {
                    self.current.push('\n');
                }
                self.current.push_str(head);
                self.flush();
                rest = tail;
                if rest.is_empty() {
                    break;
                }
                continue;
            }
            self.flush();
        }
        if toggles {
            self.fence = match self.fence {
                Some(_) => None,
                None => Some(reopen_line(line)),
            };
        }
    }

    /// What is left of the limit once the open code block is closed.
    fn room(&self) -> usize {
        let closing = if self.fence.is_some() {
            length(CLOSE) + 1
        } else {
            0
        };
        self.limit
            .saturating_sub(closing)
            .saturating_sub(length(&self.current))
    }

    /// How a message continuing the open code block starts.
    fn reopen(&self) -> String {
        self.fence.clone().unwrap_or_default()
    }

    /// Ends the current message (closing an open code block) and starts the
    /// next (opening it again).
    fn flush(&mut self) {
        let reopen = self.reopen();
        let mut message = std::mem::take(&mut self.current);
        if self.fence.is_some() {
            // A block opened right at the end moves whole to the next
            // message instead of showing empty.
            let opened_at_end = message.strip_suffix(&format!("\n{reopen}")).map(str::len);
            if message == reopen {
                message.clear();
            } else if let Some(keep) = opened_at_end {
                message.truncate(keep);
            } else {
                message.push('\n');
                message.push_str(CLOSE);
            }
        }
        if !message.trim().is_empty() {
            self.messages.push(message);
        }
        self.current = reopen;
    }

    fn finish(mut self) -> Vec<String> {
        if !self.current.trim().is_empty() && self.current != self.reopen() {
            let mut message = std::mem::take(&mut self.current);
            // An unclosed block in the reply stays as it was.
            if self.fence.is_some() && !self.messages.is_empty() {
                message.push('\n');
                message.push_str(CLOSE);
            }
            self.messages.push(message);
        }
        self.messages
    }
}

/// How a code block that `line` opened starts again in a later message:
/// the fence and the language, which is all Discord reads of the line. A
/// long info string is left out, or every message would repeat it (and
/// one that is too long would leave no room for the text).
fn reopen_line(line: &str) -> String {
    let line = line.trim();
    let info = line.trim_start_matches('`');
    let fence = &line[..line.len() - info.len()];
    match info.split_whitespace().next() {
        Some(language) if length(language) <= MAX_LANGUAGE => format!("{fence}{language}"),
        _ => fence.to_string(),
    }
}

/// The first `units` of `text` (by [`length`], on a character boundary,
/// at least one character) and the rest.
fn cut(text: &str, units: usize) -> (&str, &str) {
    let mut used = 0;
    for (at, c) in text.char_indices() {
        used += c.len_utf16();
        if used > units {
            let at = if at == 0 { c.len_utf8() } else { at };
            return text.split_at(at);
        }
    }
    (text, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(messages: &[String], limit: usize) {
        for message in messages {
            assert!(
                length(message) <= limit,
                "{} > {limit}: {message}",
                length(message)
            );
            assert!(!message.trim().is_empty());
            assert_eq!(
                message.matches(CLOSE).count() % 2,
                0,
                "unbalanced fences: {message}"
            );
        }
    }

    #[test]
    fn short_text_is_one_message() {
        assert_eq!(split_message("hello\nworld\n", 2000), ["hello\nworld"]);
        assert!(split_message("  \n\n", 2000).is_empty());
    }

    #[test]
    fn cuts_on_line_breaks() {
        let text = (0..30)
            .map(|n| format!("line {n:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = split_message(&text, 40);
        check(&messages, 40);
        assert_eq!(messages.join("\n"), text);
        assert!(messages.iter().all(|m| m.starts_with("line")));
    }

    #[test]
    fn plain_text_keeps_fences_as_typed() {
        let code: Vec<String> = (0..40).map(|n| format!("let x{n} = {n};")).collect();
        let text = format!("look:\n```rust\n{}\n```\ndone", code.join("\n"));
        let messages = split_plain(&text, 100);
        assert!(messages.len() > 2, "{messages:#?}");
        assert!(messages.iter().all(|m| length(m) <= 100));
        assert_eq!(messages.join("\n"), text);
        assert_eq!(text.matches(CLOSE).count(), 2);
    }

    #[test]
    fn code_blocks_are_closed_and_opened_again() {
        let code: Vec<String> = (0..40).map(|n| format!("let x{n} = {n};")).collect();
        let text = format!("Here:\n```rust\n{}\n```\nDone.", code.join("\n"));
        let messages = split_message(&text, 120);
        check(&messages, 120);
        assert!(messages.len() > 2);
        for message in &messages[1..messages.len() - 1] {
            assert!(message.starts_with("```rust\n"), "{message}");
            assert!(message.ends_with("\n```"), "{message}");
        }
        // Taking the added fences away gives the text back.
        let joined = messages.join("\n").replace("\n```\n```rust\n", "\n");
        assert_eq!(joined, text);
    }

    #[test]
    fn long_lines_are_cut_on_character_boundaries() {
        let text = "가".repeat(50) + &"😀".repeat(30);
        let messages = split_message(&text, 32);
        check(&messages, 32);
        assert_eq!(messages.concat(), text);

        let code = format!("```\n{}\n```", "x".repeat(100));
        let messages = split_message(&code, 40);
        check(&messages, 40);
        for message in &messages {
            assert!(message.starts_with("```"), "{message}");
        }
    }

    #[test]
    fn a_long_fence_line_is_not_opened_again_in_every_message() {
        let fence = format!("```{}", "x".repeat(1995));
        let text = format!("{fence}\n{}\n```\nafter", "body ".repeat(10).trim_end());
        let messages = split_message(&text, MESSAGE_LIMIT);
        for message in &messages {
            assert!(length(message) <= MESSAGE_LIMIT, "{}", length(message));
        }
        assert!(messages.len() <= 3, "{} messages", messages.len());
        assert_eq!(messages.iter().filter(|m| m.contains("xxxx")).count(), 1);
        assert!(messages.last().unwrap().ends_with("after"));

        // A short language is opened again; the rest of the line is not.
        let code: Vec<String> = (0..20).map(|n| format!("let x{n} = {n};")).collect();
        let text = format!("```rust title=\"main.rs\"\n{}\n```", code.join("\n"));
        let messages = split_message(&text, 80);
        check(&messages, 80);
        assert!(messages[1].starts_with("```rust\nlet"), "{}", messages[1]);
    }

    #[test]
    fn a_fence_closed_on_its_own_line_opens_no_block() {
        let lines: Vec<String> = (0..30).map(|n| format!("line {n:02}")).collect();
        let text = format!("```rm -rf build```\n{}", lines.join("\n"));
        let messages = split_message(&text, 40);
        check(&messages, 40);
        assert_eq!(messages.join("\n"), text);
        assert!(
            messages[1..].iter().all(|m| m.starts_with("line")),
            "{messages:#?}"
        );
    }

    #[test]
    fn the_real_limit_holds_for_long_replies() {
        let block = format!("```\n{}\n```\n", "fn main() {}\n".repeat(200));
        let text = format!("{}\n{block}{}", "word ".repeat(700), "끝 ".repeat(900));
        let messages = split_message(&text, MESSAGE_LIMIT);
        check(&messages, MESSAGE_LIMIT);
        assert!(messages.len() >= 3);
    }
}
