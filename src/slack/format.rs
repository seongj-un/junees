//! Text between the model and Slack.
//!
//! Outgoing: the model writes Markdown, Slack shows `mrkdwn` ("Formatting
//! message text", https://docs.slack.dev/messaging/formatting-message-text).
//! [`to_mrkdwn`] turns the common pieces into their mrkdwn spelling
//! (`**bold**` -> `*bold*`, `*italic*` -> `_italic_`, `~~strike~~` ->
//! `~strike~`, `[label](https://...)` -> `<https://...|label>`, headings
//! -> bold lines, `- item` -> `• item`, fences without their language)
//! and escapes the three control characters everywhere else ("Escaping
//! text": `&` -> `&amp;`, `<` -> `&lt;`, `>` -> `&gt;`). Escaping is what
//! keeps the model's text from mentioning anyone: user, group and special
//! mentions (`<@U...>`, `<!subteam^...>`, `<!here>`) all need `<`, and a
//! plain `@here` is not parsed unless a post sets `link_names`, which the
//! gateway never does. Markers without a partner stay as typed.
//!
//! Incoming: Slack sends a message's text in the same markup
//! ("Retrieving messages"; links as `<https://x|label>`, mentions as
//! `<@U123>`). [`from_slack`] turns it back into plain text for the model.

/// `text` with `&`, `<` and `>` escaped.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(&mut out, c);
    }
    out
}

fn push_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        c => out.push(c),
    }
}

/// The model's Markdown `text` as Slack mrkdwn (see the module docs).
pub fn to_mrkdwn(text: &str) -> String {
    let mut out = Vec::new();
    // The fence that opened the code block we are in (``` or ~~~, and
    // its length).
    let mut fence: Option<(char, usize)> = None;
    for line in text.split('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let marker = fence_marker(trimmed).filter(|_| indent <= 3);
        match (fence, marker) {
            // ```ls -la``` on one line is inline code, and a fence's
            // language is one word: anything else is no opening fence.
            (None, Some((c, n, rest))) if opens_block(rest) => {
                fence = Some((c, n));
                out.push("```".to_string());
            }
            (Some((open, len)), Some((c, n, rest))) if c == open && n >= len && rest.is_empty() => {
                fence = None;
                out.push("```".to_string());
            }
            (Some(_), _) => out.push(escape(&line.replace("```", "`\u{200b}``"))),
            (None, _) => out.push(block_line(line)),
        }
    }
    let mut text = out.join("\n");
    if fence.is_some() {
        // A code block the model left open: closed, as Markdown would.
        text.push_str("\n```");
    }
    text
}

/// The fence a line starts with (three or more backticks or tildes): its
/// character, length and the rest of the line.
fn fence_marker(line: &str) -> Option<(char, usize, &str)> {
    let c = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let n = line.chars().take_while(|x| *x == c).count();
    (n >= 3).then(|| (c, n, line[n..].trim()))
}

/// Whether a fence with `info` after it opens a code block: no info or
/// a single word (a language) without backticks.
fn opens_block(info: &str) -> bool {
    !info.contains(char::is_whitespace) && !info.contains('`')
}

/// One line outside a code block: headings, quotes, list items and rules,
/// then the inline marks.
fn block_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    // `# Title` (one to six #, then a space).
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        let title = trimmed[hashes..].trim().trim_end_matches('#').trim_end();
        if !title.is_empty() {
            return format!("{indent}*{}*", inline(&strip_bold(title)));
        }
    }
    // `> quoted`: mrkdwn quotes with a `>` at the start of the line, the
    // one `>` that is not escaped.
    if let Some(rest) = trimmed.strip_prefix('>') {
        return format!("{indent}>{}", block_line(rest));
    }
    // A thematic break (`---`, `***`, `___`).
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|m| compact.chars().all(|c| c == *m))
    {
        return format!("{indent}──────────");
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(item) = trimmed.strip_prefix(bullet) {
            return format!("{indent}• {}", inline(item));
        }
    }
    inline(line)
}

/// `text` without `**` pairs (a heading is bold as a whole already).
fn strip_bold(text: &str) -> String {
    text.replace("**", "")
}

/// The inline marks of `text` in mrkdwn, everything else escaped.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let c = rest.chars().next().unwrap_or_default();
        if let Some((piece, used)) = code_span(rest)
            .or_else(|| link(rest))
            .or_else(|| autolink(rest))
            .or_else(|| emphasis(text, at))
        {
            out.push_str(&piece);
            at += used;
            continue;
        }
        push_escaped(&mut out, c);
        at += c.len_utf8();
    }
    out
}

/// A code span at the start of `rest` (a run of backticks and the same
/// run later), as single-backtick mrkdwn code; its length in `rest`.
fn code_span(rest: &str) -> Option<(String, usize)> {
    let n = rest.chars().take_while(|c| *c == '`').count();
    if n == 0 {
        return None;
    }
    let ticks = &rest[..n];
    let mut from = n;
    while let Some(found) = rest[from..].find(ticks) {
        let start = from + found;
        let run = rest[start..].chars().take_while(|c| *c == '`').count();
        if run == n {
            let inner = &rest[n..start];
            let inner = if n > 1 { inner.trim() } else { inner };
            if inner.trim().is_empty() {
                return None;
            }
            // mrkdwn code cannot hold a backtick.
            let inner = escape(&inner.replace('`', "'"));
            return Some((format!("`{inner}`"), start + n));
        }
        from = start + run;
    }
    None
}

/// `[label](url)` at the start of `rest`: a link for http(s) and mailto
/// addresses, the label and the address as text for anything else.
fn link(rest: &str) -> Option<(String, usize)> {
    let label_end = rest.strip_prefix('[')?.find("](")? + 1;
    let label = &rest[1..label_end];
    let after = &rest[label_end + 2..];
    let url_end = url_end(after)?;
    let url = after[..url_end].trim();
    if label.is_empty() || label.contains('\n') || url.is_empty() {
        return None;
    }
    let used = label_end + 2 + url_end + 1;
    let label = inline(label);
    if safe_url(url) {
        Some((format!("<{}|{label}>", escape(url)), used))
    } else {
        Some((format!("{label} ({})", escape(url)), used))
    }
}

/// Where the address of a link ends in `after` (the text behind `](`):
/// the `)` that balances the parentheses inside it, as in
/// `https://en.wikipedia.org/wiki/Rust_(programming_language)`.
fn url_end(after: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, c) in after.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(at),
            ')' => depth -= 1,
            '\n' => return None,
            _ => {}
        }
    }
    None
}

/// `<https://...>` at the start of `rest`.
fn autolink(rest: &str) -> Option<(String, usize)> {
    let end = rest.strip_prefix('<')?.find('>')? + 1;
    let url = &rest[1..end];
    safe_url(url).then(|| (format!("<{}>", escape(url)), end + 1))
}

/// Whether `url` can go into `<url|label>` as it is: http, https or
/// mailto, without spaces, `|`, `<` or `>` (which end the link).
fn safe_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme) && lower.len() > scheme.len())
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '|' | '<' | '>'))
}

/// Bold (`**x**`), strike (`~~x~~`) or italic (`*x*`) starting at byte
/// `at` of `text`, with its partner later on the same line. `__x__` is
/// left alone: Python's `__init__` is far more common in replies.
fn emphasis(text: &str, at: usize) -> Option<(String, usize)> {
    let rest = &text[at..];
    let before = text[..at].chars().next_back();
    for (open, wrap) in [("**", "*"), ("~~", "~"), ("*", "_")] {
        if !rest.starts_with(open) {
            continue;
        }
        // `snake_case` and `2*3*4` are no marks.
        if before.is_some_and(|c| c.is_alphanumeric() || c == '\\') {
            return None;
        }
        let inner_start = open.len();
        let first = rest[inner_start..].chars().next()?;
        if first.is_whitespace() || rest[inner_start..].starts_with(open) {
            continue;
        }
        let close = closing(&rest[inner_start..], open)?;
        let inner = &rest[inner_start..inner_start + close];
        let used = inner_start + close + open.len();
        return Some((format!("{wrap}{}{wrap}", inline(inner)), used));
    }
    None
}

/// Where `mark` closes in `text`: not after a space, not inside code, not
/// right before a letter or digit (for `*`), and before the line ends.
fn closing(text: &str, mark: &str) -> Option<usize> {
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if rest.starts_with('\n') {
            return None;
        }
        if rest.starts_with('`')
            && let Some((_, used)) = code_span(rest)
        {
            at += used;
            continue;
        }
        if rest.starts_with(mark) && at > 0 {
            let before = text[..at].chars().next_back();
            let after = rest[mark.len()..].chars().next();
            let doubled = mark.len() == 1 && after.is_some_and(|c| mark.starts_with(c));
            if before.is_some_and(|c| !c.is_whitespace())
                && !after.is_some_and(char::is_alphanumeric)
                && !doubled
            {
                return Some(at);
            }
        }
        at += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// A Slack message's `text` as plain text: `<https://x|label>` ->
/// `label (https://x)`, `<https://x>` -> `https://x`, `<@U123>` ->
/// `@U123`, `<#C123|general>` -> `#general`, `<!here>` -> `@here`, and
/// `&amp;`, `&lt;`, `&gt;` back to their characters.
pub fn from_slack(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        out.push_str(&unescape(&rest[..open]));
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            out.push_str(&unescape(&rest[open..]));
            rest = "";
            break;
        };
        out.push_str(&markup(&after[..close]));
        rest = &after[close + 1..];
    }
    out.push_str(&unescape(rest));
    out
}

/// What `<inner>` shows as.
fn markup(inner: &str) -> String {
    let (target, label) = match inner.split_once('|') {
        Some((target, label)) => (target, Some(unescape(label))),
        None => (inner, None),
    };
    let target = unescape(target);
    if let Some(user) = target.strip_prefix('@') {
        return format!("@{}", label.unwrap_or_else(|| user.to_string()));
    }
    if let Some(channel) = target.strip_prefix('#') {
        return format!("#{}", label.unwrap_or_else(|| channel.to_string()));
    }
    if let Some(special) = target.strip_prefix('!') {
        return match label {
            Some(label) => label,
            None => format!("@{}", special.split('^').next().unwrap_or(special)),
        };
    }
    // Slack links what users type (`x.example`, an e-mail address) with
    // the typed text as the label.
    let bare = target
        .strip_prefix("mailto:")
        .or_else(|| target.split_once("://").map(|(_, rest)| rest))
        .unwrap_or(&target);
    match label {
        Some(label) if label != target && label != bare => format!("{label} ({target})"),
        _ if target.starts_with("mailto:") => bare.to_string(),
        _ => target.clone(),
    }
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_the_control_characters() {
        assert_eq!(escape("a < b && c > d"), "a &lt; b &amp;&amp; c &gt; d");
        // Mentions of every kind lose their `<`.
        for text in ["<!channel>", "<@U123>", "<!subteam^S1>", "<!here|here>"] {
            let out = to_mrkdwn(text);
            assert!(!out.contains('<') && !out.contains('>'), "{text}: {out}");
        }
        assert_eq!(to_mrkdwn("@here look"), "@here look");
    }

    #[test]
    fn converts_the_inline_marks() {
        let cases = [
            ("**bold** and __init__", "*bold* and __init__"),
            ("*italic* and ~~gone~~", "_italic_ and ~gone~"),
            (
                "**bold with *italic* inside**",
                "*bold with _italic_ inside*",
            ),
            ("`a <b> & *c*`", "`a &lt;b&gt; &amp; *c*`"),
            ("``code with ` tick``", "`code with ' tick`"),
            (
                "[docs](https://x.example/a?b=1&c=2)",
                "<https://x.example/a?b=1&amp;c=2|docs>",
            ),
            (
                "[**bold** label](http://x.example)",
                "<http://x.example|*bold* label>",
            ),
            ("[mail](mailto:a@b.example)", "<mailto:a@b.example|mail>"),
            ("see <https://x.example/p>", "see <https://x.example/p>"),
            // Unsafe or odd addresses stay text.
            ("[x](javascript:alert(1))", "x (javascript:alert(1))"),
            (
                "[x](https://a.example/|<!here>)",
                "x (https://a.example/|&lt;!here&gt;)",
            ),
            ("<javascript:x>", "&lt;javascript:x&gt;"),
            // Parentheses inside the address are part of it.
            (
                "[Rust](https://en.wikipedia.org/wiki/Rust_(programming_language)) ok",
                "<https://en.wikipedia.org/wiki/Rust_(programming_language)|Rust> ok",
            ),
            (
                "([a](https://x.example/f(1)(2)))",
                "(<https://x.example/f(1)(2)|a>)",
            ),
        ];
        for (markdown, want) in cases {
            assert_eq!(to_mrkdwn(markdown), want, "{markdown}");
        }
    }

    #[test]
    fn markers_without_a_partner_stay_as_typed() {
        for text in [
            "2 * 3 * 4",
            "a*b*c",
            "snake_case_name",
            "__init__.py",
            "**open only",
            "~~ spaced~~",
            "`unclosed",
            "[label](no close",
            "*",
            "**",
            "**bold\nnext**",
        ] {
            assert_eq!(to_mrkdwn(text), text, "{text}");
        }
    }

    #[test]
    fn converts_blocks_and_keeps_code_as_written() {
        let markdown = "# Title **x**\n\n- one\n  * two <x>\n> quoted *it*\n---\n1. first\n\
                        ```rust\nlet a = b && c; // **not bold** <@U1>\n```\nafter";
        let want = "*Title x*\n\n• one\n  • two &lt;x&gt;\n> quoted _it_\n──────────\n1. first\n\
                    ```\nlet a = b &amp;&amp; c; // **not bold** &lt;@U1&gt;\n```\nafter";
        assert_eq!(to_mrkdwn(markdown), want);
        // A block left open is closed; a fence inside it cannot end it.
        assert_eq!(to_mrkdwn("```\ncode"), "```\ncode\n```");
        assert_eq!(
            to_mrkdwn("~~~~\na ``` b\n~~~~"),
            "```\na `\u{200b}`` b\n```"
        );
        // A fence closed on its own line is inline code, not a block.
        assert_eq!(to_mrkdwn("```ls -la```\n**bold**"), "`ls -la`\n*bold*");
        assert_eq!(to_mrkdwn("```ls```"), "`ls`");
        // Words after a fence are no language: no block opens.
        assert_eq!(to_mrkdwn("``` not a fence\n*it*"), "``` not a fence\n_it_");
        assert_eq!(to_mrkdwn("```sh\nls\n```"), "```\nls\n```");
        // Korean text passes through whole.
        assert_eq!(
            to_mrkdwn("**한글** 문장 <태그>"),
            "*한글* 문장 &lt;태그&gt;"
        );
    }

    #[test]
    fn reads_slack_markup_back_as_plain_text() {
        let cases = [
            ("a &lt;b&gt; &amp;amp; c", "a <b> &amp; c"),
            (
                "<https://x.example|docs> and <https://y.example>",
                "docs (https://x.example) and https://y.example",
            ),
            ("<http://x.example|x.example>", "http://x.example"),
            ("<mailto:a@b.example|a@b.example>", "a@b.example"),
            ("hi <@U123> and <@U456|bob>", "hi @U123 and @bob"),
            ("join <#C123|general> or <#C456>", "join #general or #C456"),
            (
                "<!here> <!subteam^S1|@devs> <!date^1^{date}|Feb 18>",
                "@here @devs Feb 18",
            ),
            ("unclosed < bracket", "unclosed < bracket"),
            ("한글 &lt;", "한글 <"),
        ];
        for (slack, want) in cases {
            assert_eq!(from_slack(slack), want, "{slack}");
        }
    }
}
