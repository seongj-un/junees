//! Checks for text that Junees saved from the model (memory entries,
//! skills) before it reaches a system prompt or the terminal.
//!
//! A hidden character ([`is_hidden`]) can rewrite a terminal line (`ESC[2K`,
//! `\r`) or make a line of the system prompt read differently from what it
//! holds (bidirectional overrides). Writers refuse them with
//! [`check_plain`]; readers write what a file edited by hand still holds as
//! escapes with [`escape_hidden`].

/// Whether `c` is a control character, a bidirectional formatting
/// character or a line/paragraph separator. Tab and the line end `\n` are
/// control characters too; the text functions below allow them.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{2028}'
                | '\u{2029}'
        )
}

/// The hidden characters of `text` with their byte offsets, other than
/// tab, `\n` and a `\r` right before `\n`.
fn hidden(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        while let Some((at, c)) = chars.next() {
            let allowed = match c {
                '\t' | '\n' => true,
                '\r' => chars.peek().is_some_and(|&(_, next)| next == '\n'),
                c => !is_hidden(c),
            };
            if !allowed {
                return Some((at, c));
            }
        }
        None
    })
}

/// Refuses `text` (described as `what`, for example "`content`") when it
/// holds a hidden character other than tab and line ends. The error is a
/// message for the model.
pub fn check_plain(what: &str, text: &str) -> Result<(), String> {
    match hidden(text).next() {
        None => Ok(()),
        Some((_, c)) => Err(format!(
            "{what} contains U+{:04X}, a control or bidirectional formatting character; \
             write it as plain text",
            c as u32
        )),
    }
}

/// `text` with every hidden character other than tab and line ends
/// written out as an escape (`\u{1b}`, `\r`, `\u{202e}`), so it shows
/// instead of acting.
pub fn escape_hidden(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (at, c) in hidden(text) {
        out.push_str(&text[last..at]);
        out.extend(c.escape_default());
        last = at + c.len_utf8();
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_hidden_characters() {
        for text in [
            "ok\u{1b}[2Kfake",
            "a\rb",
            "a\r",
            "abc\u{202e}cba",
            "x\u{2066}y",
            "x\u{200f}",
            "x\u{2028}y",
            "nul\0",
            "del\u{7f}",
            "c1\u{9b}",
        ] {
            let err = check_plain("`content`", text).unwrap_err();
            assert!(err.starts_with("`content` contains U+"), "{text:?}: {err}");
        }
        assert!(check_plain("x", "ok\u{1b}").unwrap_err().contains("U+001B"));
        for text in ["plain 한국어 text", "tab\there", "two\nlines\r\n", ""] {
            assert_eq!(check_plain("x", text), Ok(()), "{text:?}");
        }
    }

    #[test]
    fn escapes_what_it_would_refuse() {
        assert_eq!(
            escape_hidden("ok\u{1b}[2K\rfake line\u{202e}abc"),
            "ok\\u{1b}[2K\\rfake line\\u{202e}abc"
        );
        assert_eq!(escape_hidden("a\tb\nc\r\n한글"), "a\tb\nc\r\n한글");
        assert_eq!(escape_hidden("\u{2029}"), "\\u{2029}");
        for c in ['\u{1b}', '\u{202e}', '\u{2069}', '\u{85}'] {
            assert!(is_hidden(c), "{c:?}");
            assert_eq!(check_plain("x", &escape_hidden(&format!("a{c}b"))), Ok(()));
        }
    }
}
