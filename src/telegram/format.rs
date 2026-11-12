//! The model's Markdown as Telegram HTML (`parse_mode` HTML), cut into
//! messages.
//!
//! Bot API, "Formatting options"
//! (https://core.telegram.org/bots/api#formatting-options, read
//! 2026-11-12 with curl), "HTML style":
//!
//! - Supported tags include `<b>`, `<i>`, `<s>`, `<a href="...">`,
//!   `<code>`, `<pre>`, `<pre><code class="language-python">` and
//!   `<blockquote>`; "Only the tags mentioned above are currently
//!   supported."
//! - "All `<`, `>` and `&` symbols that are not a part of a tag or an HTML
//!   entity must be replaced with the corresponding HTML entities (`<` with
//!   `&lt;`, `>` with `&gt;` and `&` with `&amp;`)", and of the named
//!   entities only `&lt;`, `&gt;`, `&amp;` and `&quot;` are known.
//!   That short list is why HTML is used rather than MarkdownV2, where
//!   eighteen characters must be escaped everywhere outside code.
//! - Nesting: "bold, italic, underline, strikethrough, and spoiler entities
//!   can contain and can be part of any other entities, except pre and
//!   code"; "blockquote and expandable_blockquote entities can't be
//!   nested"; "All other entities can't contain each other". So bold and
//!   the like are closed around inline code and opened again after it,
//!   code inside link text stays plain text, and a mention (an entity of
//!   its own) cannot sit inside `code` or `pre`: `@name` there notifies no
//!   one and is left as it is (#63).
//!
//! What is converted: fenced code blocks (with a language), inline code,
//! bold (`**`, `__`), italic (`*`, `_`), strikethrough (`~~`), links to
//! http(s) addresses, and `>` quotes. Lists, headings and tables stay as
//! written. A marker without its partner stays as typed, and so does a
//! code block without its closing fence.
//!
//! [`render`] measures the HTML (tags and entities included), so each
//! message fits `sendMessage`'s 4096 characters however Telegram counts
//! them, and cuts between characters only: a tag open at a cut is closed
//! at the end of the message and opened again at the start of the next.

use reqwest::Url;

use super::platform::{NO_MENTION, mention_points};
use crate::gateway::split::length;

/// The longest language name kept on a code block.
const MAX_LANGUAGE: usize = 32;
/// The longest link address kept as a link; a longer one stays as typed,
/// so a cut message always has room for the tags it opens again.
const MAX_URL: usize = 1024;

/// One message: its HTML, and the same text without formatting for when
/// Telegram refuses the HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub html: String,
    pub plain: String,
}

/// `markdown` as Telegram HTML messages of at most `limit` UTF-16 units
/// each (by [`length`], tags and entities counted). With `defuse`
/// (group chats) every `@name` that Telegram would take for a mention gets
/// an invisible word joiner after the `@`, except inside code, where
/// Telegram makes no mentions; the plain text breaks those too, since it
/// has no code.
pub fn render(markdown: &str, defuse: bool, limit: usize) -> Vec<Part> {
    let mut atoms = parse(markdown.trim_end());
    if defuse {
        atoms = defuse_mentions(atoms);
    }
    split(&atoms, limit.max(64))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tag {
    Bold,
    Italic,
    Strike,
    Code,
    /// A code block, with its language.
    Pre(Option<String>),
    /// A link to this (checked) address.
    Link(String),
    Quote,
}

impl Tag {
    fn open(&self) -> String {
        match self {
            Tag::Bold => "<b>".into(),
            Tag::Italic => "<i>".into(),
            Tag::Strike => "<s>".into(),
            Tag::Code => "<code>".into(),
            Tag::Pre(None) => "<pre>".into(),
            Tag::Pre(Some(language)) => format!("<pre><code class=\"language-{language}\">"),
            Tag::Link(url) => format!("<a href=\"{}\">", escape(url, true)),
            Tag::Quote => "<blockquote>".into(),
        }
    }

    fn close(&self) -> &'static str {
        match self {
            Tag::Bold => "</b>",
            Tag::Italic => "</i>",
            Tag::Strike => "</s>",
            Tag::Code => "</code>",
            Tag::Pre(None) => "</pre>",
            Tag::Pre(Some(_)) => "</code></pre>",
            Tag::Link(_) => "</a>",
            Tag::Quote => "</blockquote>",
        }
    }

    fn is_code(&self) -> bool {
        matches!(self, Tag::Code | Tag::Pre(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Atom {
    Open(Tag),
    /// Closes the tag opened last.
    Close,
    Char(char),
    /// A word joiner in the plain text only (an `@name` in code).
    PlainOnly(char),
}

// ------------------------------------------------------------ Parsing

fn parse(markdown: &str) -> Vec<Atom> {
    let lines: Vec<&str> = markdown.split('\n').collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        if at > 0 {
            out.push(Atom::Char('\n'));
        }
        let line = lines[at];
        if let Some(fence) = Fence::opening(line)
            && let Some(end) = (at + 1..lines.len()).find(|&n| fence.closes(lines[n]))
        {
            out.push(Atom::Open(Tag::Pre(fence.language)));
            out.extend(lines[at + 1..end].join("\n").chars().map(Atom::Char));
            out.push(Atom::Close);
            at = end + 1;
            continue;
        }
        if let Some(first) = quoted(line) {
            out.push(Atom::Open(Tag::Quote));
            inline(first, &mut out);
            at += 1;
            while let Some(next) = lines.get(at).and_then(|line| quoted(line)) {
                out.push(Atom::Char('\n'));
                inline(next, &mut out);
                at += 1;
            }
            out.push(Atom::Close);
            continue;
        }
        inline(line, &mut out);
        at += 1;
    }
    out
}

/// A code block's opening fence.
struct Fence {
    marker: char,
    count: usize,
    language: Option<String>,
}

impl Fence {
    /// ```` ```rust ```` or `~~~` (up to three spaces in), as CommonMark
    /// writes them; a backtick fence whose info string holds a backtick
    /// is none.
    fn opening(line: &str) -> Option<Fence> {
        let trimmed = indented(line)?;
        let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let count = trimmed.chars().take_while(|&c| c == marker).count();
        if count < 3 {
            return None;
        }
        let info = trimmed[count..].trim();
        if marker == '`' && info.contains('`') {
            return None;
        }
        let language = info
            .split_whitespace()
            .next()
            .filter(|word| {
                word.chars().count() <= MAX_LANGUAGE
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+#._-".contains(c))
            })
            .map(str::to_string);
        Some(Fence {
            marker,
            count,
            language,
        })
    }

    /// Whether `line` is a closing fence for this one: the same marker,
    /// at least as many, nothing after.
    fn closes(&self, line: &str) -> bool {
        indented(line).is_some_and(|trimmed| {
            let trimmed = trimmed.trim_end();
            trimmed.chars().take_while(|&c| c == self.marker).count() >= self.count
                && trimmed.chars().all(|c| c == self.marker)
        })
    }
}

/// `line` without up to three leading spaces; `None` when it is indented
/// more.
fn indented(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    (line.len() - trimmed.len() <= 3).then_some(trimmed)
}

/// The text of a `>` quote line (`> text`, `>text`).
fn quoted(line: &str) -> Option<&str> {
    let rest = indented(line)?.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// One line of inline Markdown.
fn inline(text: &str, out: &mut Vec<Atom>) {
    let chars: Vec<char> = text.chars().collect();
    Inline {
        chars: &chars,
        out,
        styles: Vec::new(),
        in_link: false,
    }
    .run(0, chars.len());
}

struct Inline<'a> {
    chars: &'a [char],
    out: &'a mut Vec<Atom>,
    /// Bold, italic and strikethrough open around the current position.
    styles: Vec<Tag>,
    in_link: bool,
}

impl Inline<'_> {
    fn run(&mut self, start: usize, end: usize) {
        let chars = self.chars;
        let mut at = start;
        while at < end {
            let c = chars[at];
            if c == '\\' && at + 1 < end && chars[at + 1].is_ascii_punctuation() {
                self.out.push(Atom::Char(chars[at + 1]));
                at += 2;
                continue;
            }
            if c == '`' {
                let (count, content) = code_span(chars, at, end);
                match content {
                    Some((from, to)) => {
                        self.code(from, to);
                        at = to + count;
                    }
                    None => {
                        self.out.extend((0..count).map(|_| Atom::Char('`')));
                        at += count;
                    }
                }
                continue;
            }
            if c == '['
                && !self.in_link
                && let Some((text_end, url, after)) = link(chars, at, end)
            {
                self.out.push(Atom::Open(Tag::Link(url)));
                self.in_link = true;
                self.run(at + 1, text_end);
                self.in_link = false;
                self.out.push(Atom::Close);
                at = after;
                continue;
            }
            // A bare web address is copied as written (GFM autolinks):
            // `__init__` in its path is no bold, and Telegram links it.
            if let Some(stop) = bare_url(chars, at, end) {
                self.out
                    .extend(chars[at..stop].iter().map(|&c| Atom::Char(c)));
                at = stop;
                continue;
            }
            if matches!(c, '*' | '_' | '~')
                && let Some((tag, width, close)) = emphasis(chars, at, end)
            {
                self.out.push(Atom::Open(tag.clone()));
                self.styles.push(tag);
                self.run(at + width, close);
                self.styles.pop();
                self.out.push(Atom::Close);
                at = close + width;
                continue;
            }
            // A run of markers that opens nothing stays as typed whole,
            // so its tail is not taken for an opener.
            let run = if matches!(c, '*' | '_' | '~' | '`') {
                run_length(chars, at, end)
            } else {
                1
            };
            self.out
                .extend(chars[at..at + run].iter().map(|&c| Atom::Char(c)));
            at += run;
        }
    }

    /// Inline code: bold and the like may not contain it, so they are
    /// closed around it; in link text (a link may not contain code
    /// either) it stays plain text.
    fn code(&mut self, from: usize, to: usize) {
        let mut content = &self.chars[from..to];
        if content.len() >= 2
            && content[0] == ' '
            && content[content.len() - 1] == ' '
            && content.iter().any(|&c| c != ' ')
        {
            content = &content[1..content.len() - 1];
        }
        if self.in_link {
            self.out.extend(content.iter().map(|&c| Atom::Char(c)));
            return;
        }
        let styles = self.styles.clone();
        self.out.extend(styles.iter().map(|_| Atom::Close));
        self.out.push(Atom::Open(Tag::Code));
        self.out.extend(content.iter().map(|&c| Atom::Char(c)));
        self.out.push(Atom::Close);
        self.out.extend(styles.into_iter().map(Atom::Open));
    }
}

/// How many of `chars[at]` follow one another from `at` (before `end`).
fn run_length(chars: &[char], at: usize, end: usize) -> usize {
    chars[at..end]
        .iter()
        .take_while(|&&c| c == chars[at])
        .count()
}

/// The backtick run at `at` and, if a run of the same length closes it
/// before `end`, the content between.
fn code_span(chars: &[char], at: usize, end: usize) -> (usize, Option<(usize, usize)>) {
    let count = run_length(chars, at, end);
    let mut look = at + count;
    while look < end {
        if chars[look] == '`' {
            let run = run_length(chars, look, end);
            if run == count {
                return (count, Some((at + count, look)));
            }
            look += run;
        } else {
            look += 1;
        }
    }
    (count, None)
}

/// `[text](address)` at `at`: where the text ends, the address, and where
/// the link ends. Only http(s) addresses make links ([`link_address`]).
fn link(chars: &[char], at: usize, end: usize) -> Option<(usize, String, usize)> {
    let mut depth = 0;
    let mut look = at + 1;
    let text_end = loop {
        match chars.get(look).filter(|_| look < end)? {
            '\\' => look += 1,
            '[' => depth += 1,
            ']' if depth == 0 => break look,
            ']' => depth -= 1,
            _ => {}
        }
        look += 1;
    };
    if text_end == at + 1 || chars.get(text_end + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0;
    let mut look = text_end + 2;
    let url_end = loop {
        match chars.get(look).filter(|_| look < end)? {
            '(' => depth += 1,
            ')' if depth == 0 => break look,
            ')' => depth -= 1,
            _ => {}
        }
        look += 1;
    };
    let raw: String = chars[text_end + 2..url_end].iter().collect();
    let url = link_address(raw.trim())?;
    Some((text_end, url, url_end + 1))
}

/// `raw` if it is an http(s) address a link may point to: no spaces or
/// control characters, a host, at most [`MAX_URL`] long once escaped for
/// the `href` (`&` grows to `&amp;`; the cut reserves room for the tag
/// by that length). Any other scheme
/// (`javascript:`, `data:`, `tg://user?id=` which would notify a user,
/// a relative path) stays as typed.
fn link_address(raw: &str) -> Option<String> {
    if raw.is_empty()
        || length(&escape(raw, true)) > MAX_URL
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let parsed = Url::parse(raw).ok()?;
    let scheme_ok = matches!(parsed.scheme(), "http" | "https");
    let written = raw.to_ascii_lowercase();
    (scheme_ok
        && (written.starts_with("http://") || written.starts_with("https://"))
        && parsed.host_str().is_some_and(|host| !host.is_empty()))
    .then(|| raw.to_string())
}

/// Where a bare `http://` or `https://` address starting at `at` ends
/// (before `end`), as GFM's extended autolinks see it: not after a
/// letter or digit, up to a space or `<`, without trailing `?!.,:*_~'"`
/// and without a `)` that closes nothing inside it.
fn bare_url(chars: &[char], at: usize, end: usize) -> Option<usize> {
    if !matches!(chars[at], 'h' | 'H') || (at > 0 && chars[at - 1].is_alphanumeric()) {
        return None;
    }
    let head: String = chars[at..end.min(at + 8)].iter().collect();
    let head = head.to_ascii_lowercase();
    let scheme = if head.starts_with("https://") {
        8
    } else if head.starts_with("http://") {
        7
    } else {
        return None;
    };
    let mut stop = at + scheme;
    while stop < end && !chars[stop].is_whitespace() && chars[stop] != '<' {
        stop += 1;
    }
    while stop > at + scheme {
        let last = chars[stop - 1];
        let unbalanced = last == ')' && {
            let inside = &chars[at..stop];
            let open = inside.iter().filter(|&&c| c == '(').count();
            let close = inside.iter().filter(|&&c| c == ')').count();
            close > open
        };
        if "?!.,:*_~'\"".contains(last) || unbalanced {
            stop -= 1;
        } else {
            break;
        }
    }
    (stop > at + scheme).then_some(stop)
}

/// Characters that join the parts of a path or address
/// (`pkg/__init__.py`, `a.__x__`): `_` next to them is no marker.
fn joins_path(c: char) -> bool {
    "/\\.:-=@#%+?&".contains(c)
}

/// Whether the marker run ending before `to` is glued to more of a path
/// or file name (`__init__.py`).
fn glued(chars: &[char], to: usize) -> bool {
    chars.get(to).is_some_and(|&c| joins_path(c))
        && chars.get(to + 1).is_some_and(|c| c.is_alphanumeric())
}

/// Unicode punctuation as CommonMark uses it, near enough: anything that
/// is no letter, digit or space (symbols included).
fn is_punctuation(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace()
}

/// Whether a run of markers from `from` to `to` (exclusive) is
/// left-flanking and right-flanking (CommonMark, "Emphasis and strong
/// emphasis"): left-flanking runs may open, right-flanking ones close.
/// The line's start and end count as spaces.
fn flanking(chars: &[char], from: usize, to: usize) -> (bool, bool) {
    let before = from.checked_sub(1).map(|at| chars[at]);
    let after = chars.get(to).copied();
    let space = |c: Option<char>| c.is_none_or(char::is_whitespace);
    let punct = |c: Option<char>| c.is_some_and(is_punctuation);
    let left = !space(after) && (!punct(after) || space(before) || punct(before));
    let right = !space(before) && (!punct(before) || space(after) || punct(after));
    (left, right)
}

/// Whether the marker run from `from` to `to` may open (`opening`) or
/// close emphasis. `*` and `~` follow the flanking rules alone, so they
/// work inside words (`**설정 파일**을`, `**PR**s`); `_` does not
/// (`snake_case`). A `*` between two digits is multiplication (`2*3*4`).
fn can(chars: &[char], from: usize, to: usize, opening: bool) -> bool {
    let (left, right) = flanking(chars, from, to);
    let before = from.checked_sub(1).map(|at| chars[at]);
    let after = chars.get(to).copied();
    match chars[from] {
        // Not inside a path either: `src/pkg/__init__.py`.
        '_' if opening => {
            left && (!right || before.is_some_and(is_punctuation))
                && !before.is_some_and(joins_path)
        }
        '_' => right && (!left || after.is_some_and(is_punctuation)) && !glued(chars, to),
        '*' if before.is_some_and(|c| c.is_ascii_digit())
            && after.is_some_and(|c| c.is_ascii_digit()) =>
        {
            false
        }
        _ if opening => left,
        _ => right,
    }
}

/// Bold, italic or strikethrough opening at `at`: the tag, the marker
/// width, and where the closing marker starts. Openers and closers
/// follow CommonMark's flanking rules ([`can`]). Code spans are skipped
/// while looking for the closer.
fn emphasis(chars: &[char], at: usize, end: usize) -> Option<(Tag, usize, usize)> {
    let marker = chars[at];
    let run = run_length(chars, at, end);
    if !can(chars, at, at + run, true) {
        return None;
    }
    let (tag, width) = match (marker, run) {
        ('~', 2) => (Tag::Strike, 2),
        ('*' | '_', 1) => (Tag::Italic, 1),
        ('*' | '_', 2 | 3) => (Tag::Bold, 2),
        _ => return None,
    };
    let start = at + width;
    if chars.get(start).is_none_or(|c| c.is_whitespace()) || start >= end {
        return None;
    }
    let mut look = start;
    // A run of three opens bold around italic: the bold's content starts
    // with the italic's marker, so the search starts after it.
    if run == 3 {
        look += 1;
    }
    while look < end {
        let c = chars[look];
        if c == '\\' {
            look += 2;
            continue;
        }
        if c == '`' {
            let (count, content) = code_span(chars, look, end);
            look = content.map_or(look + count, |(_, to)| to + count);
            continue;
        }
        if let Some(stop) = bare_url(chars, look, end) {
            look = stop;
            continue;
        }
        if c != marker {
            look += 1;
            continue;
        }
        let length = run_length(chars, look, end);
        let fits = length == width || (width == 2 && length == 3 && marker != '~');
        let after = look + length;
        let closes = fits && can(chars, look, after, false);
        // `__main__.py`: the opener names a file, not emphasis.
        if !closes && fits && marker == '_' && glued(chars, after) {
            return None;
        }
        if closes {
            let close = after - width;
            return (close > start).then_some((tag, width, close));
        }
        look = after;
    }
    None
}

// ----------------------------------------------------------- Mentions

/// [`mention_points`] over the text Telegram shows: a word joiner after
/// each `@` that would be a mention, as a character outside code and in
/// the plain text only inside code.
fn defuse_mentions(atoms: Vec<Atom>) -> Vec<Atom> {
    let mut shown = String::new();
    // For each character of `shown`: its atom, and whether it is code.
    let mut owners = Vec::new();
    let mut code = Vec::new();
    for (index, atom) in atoms.iter().enumerate() {
        match atom {
            Atom::Open(tag) => code.push(tag.is_code()),
            Atom::Close => {
                code.pop();
            }
            Atom::Char(c) => {
                shown.push(*c);
                owners.push((index, code.iter().any(|&b| b)));
            }
            Atom::PlainOnly(_) => {}
        }
    }
    let starts: Vec<usize> = shown.char_indices().map(|(at, _)| at).collect();
    let mut after = vec![None; atoms.len()];
    for at in mention_points(&shown) {
        let Ok(nth) = starts.binary_search(&at) else {
            continue;
        };
        let (index, in_code) = owners[nth];
        after[index] = Some(if in_code {
            Atom::PlainOnly(NO_MENTION)
        } else {
            Atom::Char(NO_MENTION)
        });
    }
    let mut out = Vec::with_capacity(atoms.len());
    for (atom, extra) in atoms.into_iter().zip(after) {
        out.push(atom);
        out.extend(extra);
    }
    out
}

// ------------------------------------------------------------ Cutting

fn escape(text: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(&mut out, c, attribute);
    }
    out
}

fn push_escaped(out: &mut String, c: char, attribute: bool) {
    match c {
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '&' => out.push_str("&amp;"),
        '"' if attribute => out.push_str("&quot;"),
        _ => out.push(c),
    }
}

/// Where a message may end: before a line break or a space.
#[derive(Clone)]
struct Cut {
    html: usize,
    plain: usize,
    used: usize,
    open: Vec<Tag>,
    /// The atom the next message starts with (the break itself is
    /// dropped).
    next: usize,
}

struct Splitter {
    limit: usize,
    parts: Vec<Part>,
    html: String,
    plain: String,
    /// [`length`] of `html`.
    used: usize,
    /// Tags open in `html`, outermost first.
    open: Vec<Tag>,
    /// Whether anything was added since the message started.
    added: bool,
    line_cut: Option<Cut>,
    space_cut: Option<Cut>,
}

impl Splitter {
    fn closing(&self) -> usize {
        self.open.iter().map(|tag| length(tag.close())).sum()
    }

    fn cut_here(&self, next: usize) -> Cut {
        Cut {
            html: self.html.len(),
            plain: self.plain.len(),
            used: self.used,
            open: self.open.clone(),
            next,
        }
    }

    /// Ends the message at `cut` (or here) and starts the next with the
    /// open tags opened again.
    fn finish(&mut self, cut: Option<Cut>) {
        if let Some(cut) = cut {
            self.html.truncate(cut.html);
            self.plain.truncate(cut.plain);
            self.used = cut.used;
            self.open = cut.open;
        }
        let mut html = std::mem::take(&mut self.html);
        for tag in self.open.iter().rev() {
            html.push_str(tag.close());
        }
        let plain = std::mem::take(&mut self.plain);
        // Telegram refuses a message with nothing to show.
        if !plain.trim().is_empty() {
            self.parts.push(Part { html, plain });
        }
        for tag in &self.open {
            self.html.push_str(&tag.open());
        }
        self.used = length(&self.html);
        self.added = false;
        self.line_cut = None;
        self.space_cut = None;
    }
}

fn split(atoms: &[Atom], limit: usize) -> Vec<Part> {
    let mut out = Splitter {
        limit,
        parts: Vec::new(),
        html: String::new(),
        plain: String::new(),
        used: 0,
        open: Vec::new(),
        added: false,
        line_cut: None,
        space_cut: None,
    };
    let mut at = 0;
    while at < atoms.len() {
        let atom = &atoms[at];
        let mut escaped = String::new();
        let cost = match atom {
            Atom::Open(tag) => length(&tag.open()) + length(tag.close()),
            Atom::Close => 0,
            Atom::Char(c) => {
                push_escaped(&mut escaped, *c, false);
                length(&escaped)
            }
            Atom::PlainOnly(_) => 0,
        };
        if out.added && out.used + cost + out.closing() > out.limit {
            let cut = out.line_cut.clone().or_else(|| out.space_cut.clone());
            match cut {
                Some(cut) => {
                    at = cut.next;
                    out.finish(Some(cut));
                }
                None => out.finish(None),
            }
            continue;
        }
        match atom {
            Atom::Open(tag) => {
                out.html.push_str(&tag.open());
                out.used += length(&tag.open());
                out.open.push(tag.clone());
            }
            Atom::Close => {
                if let Some(tag) = out.open.pop() {
                    out.html.push_str(tag.close());
                    out.used += length(tag.close());
                }
            }
            Atom::Char(c) => {
                if out.added && *c == '\n' {
                    out.line_cut = Some(out.cut_here(at + 1));
                } else if out.added && *c == ' ' {
                    out.space_cut = Some(out.cut_here(at + 1));
                }
                out.html.push_str(&escaped);
                out.plain.push(*c);
                out.used += cost;
            }
            Atom::PlainOnly(c) => out.plain.push(*c),
        }
        out.added = true;
        at += 1;
    }
    out.finish(None);
    out.parts
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 4096;

    /// The HTML of `markdown` as one message (no mention breaking).
    fn html(markdown: &str) -> String {
        let parts = render(markdown, false, LIMIT);
        assert_eq!(parts.len(), 1, "{parts:#?}");
        parts[0].html.clone()
    }

    #[test]
    fn converts_each_element() {
        assert_eq!(html("**bold** and __bold__"), "<b>bold</b> and <b>bold</b>");
        assert_eq!(html("*it* and _it_"), "<i>it</i> and <i>it</i>");
        assert_eq!(html("~~gone~~"), "<s>gone</s>");
        assert_eq!(html("run `cargo test`"), "run <code>cargo test</code>");
        assert_eq!(html("`` a ` b ``"), "<code>a ` b</code>");
        assert_eq!(
            html("see [the docs](https://example.com/a_(b)?x=1&y=2)"),
            "see <a href=\"https://example.com/a_(b)?x=1&amp;y=2\">the docs</a>"
        );
        assert_eq!(
            html("> quoted *line*\n>second\nafter"),
            "<blockquote>quoted <i>line</i>\nsecond</blockquote>\nafter"
        );
        assert_eq!(
            html("code:\n```rust\nfn main() {}\n```\ndone"),
            "code:\n<pre><code class=\"language-rust\">fn main() {}</code></pre>\ndone"
        );
        assert_eq!(html("~~~\nplain\n~~~"), "<pre>plain</pre>");
        // Lists and headings stay as written.
        assert_eq!(
            html("# Title\n- one\n* two\n1. three"),
            "# Title\n- one\n* two\n1. three"
        );
    }

    #[test]
    fn nests_and_keeps_code_out_of_bold() {
        assert_eq!(html("***both***"), "<b><i>both</i></b>");
        assert_eq!(
            html("**bold _italic_ ~~gone~~**"),
            "<b>bold <i>italic</i> <s>gone</s></b>"
        );
        assert_eq!(
            html("[**bold** link](https://a.b)"),
            "<a href=\"https://a.b\"><b>bold</b> link</a>"
        );
        // Bold may not contain code (Bot API): closed around it.
        assert_eq!(
            html("**use `x` here**"),
            "<b>use </b><code>x</code><b> here</b>"
        );
        // Nor may a link: the code is plain link text.
        assert_eq!(
            html("[`cmd` docs](https://a.b)"),
            "<a href=\"https://a.b\">cmd docs</a>"
        );
        // Markers inside code are code.
        assert_eq!(html("`**not bold**`"), "<code>**not bold**</code>");
        assert_eq!(html("**a `**` b**"), "<b>a </b><code>**</code><b> b</b>");
        // Code blocks hold no other formatting.
        assert_eq!(
            html("```\n**x** [a](https://a.b)\n```"),
            "<pre>**x** [a](https://a.b)</pre>"
        );
    }

    #[test]
    fn emphasis_works_next_to_korean_particles_and_inside_words() {
        assert_eq!(
            html("**설정 파일**을 고치세요"),
            "<b>설정 파일</b>을 고치세요"
        );
        assert_eq!(
            html("`config.toml`의 **telegram_format**을 html로"),
            "<code>config.toml</code>의 <b>telegram_format</b>을 html로"
        );
        assert_eq!(
            html("두 **PR**s and *한*글"),
            "두 <b>PR</b>s and <i>한</i>글"
        );
        assert_eq!(html("값은 ~~예전~~이다"), "값은 <s>예전</s>이다");
        // Punctuation around the markers (CommonMark's flanking rules).
        assert_eq!(html("(**x**)"), "(<b>x</b>)");
        // `_` still needs a word boundary.
        assert_eq!(html("_설정_을 x_y_z"), "_설정_을 x_y_z");
    }

    #[test]
    fn web_addresses_and_paths_keep_their_underscores() {
        let url = "https://github.com/o/r/blob/main/pkg/__init__.py";
        assert_eq!(html(&format!("see {url}")), format!("see {url}"));
        let parts = render(&format!("see {url}"), false, LIMIT);
        assert_eq!(parts[0].plain, format!("see {url}"));
        assert_eq!(
            html("edit src/pkg/__init__.py now"),
            "edit src/pkg/__init__.py now"
        );
        assert_eq!(
            html("run __main__.py or a.__x__"),
            "run __main__.py or a.__x__"
        );
        assert_eq!(
            html("HTTP://A.B/x_*y*_z and http://a.b/~~x~~"),
            "HTTP://A.B/x_*y*_z and http://a.b/~~x~~"
        );
        // Trailing punctuation and markers are not part of the address.
        assert_eq!(
            html("**see https://a.b/x_y_z**, (https://a.b/c_(d)) _x_"),
            "<b>see https://a.b/x_y_z</b>, (https://a.b/c_(d)) <i>x</i>"
        );
        // Bold and italic on their own still work.
        assert_eq!(html("__bold__ and _it_."), "<b>bold</b> and <i>it</i>.");
    }

    #[test]
    fn markers_without_a_partner_stay_as_typed() {
        for text in [
            "a * b",
            "2*3*4",
            "snake_case_name and x_y_z",
            "**open only",
            "close only**",
            "`open code",
            "~one tilde~",
            "[text] (https://a.b)",
            "[text](https://a.b",
            "*** rule",
            "****x****",
            "\\*escaped\\*",
        ] {
            let expected = text.replace("\\*", "*");
            assert_eq!(html(text), escape(&expected, false), "{text}");
        }
        // An unclosed code block is no code block.
        assert_eq!(html("```rust\nlet x = 1;"), "```rust\nlet x = 1;");
        // A closing fence needs the same marker.
        assert_eq!(html("```\nx\n~~~"), "```\nx\n~~~");
    }

    #[test]
    fn escapes_the_three_characters_everywhere() {
        assert_eq!(html("a <b> & c > d"), "a &lt;b&gt; &amp; c &gt; d");
        assert_eq!(html("`<tag> &amp;`"), "<code>&lt;tag&gt; &amp;amp;</code>");
        assert_eq!(
            html("```html\n<p>\"x\"</p>\n```"),
            "<pre><code class=\"language-html\">&lt;p&gt;\"x\"&lt;/p&gt;</code></pre>"
        );
        // A strange language name is left out, not put in the attribute.
        assert_eq!(html("```a\"b\nx\n```"), "<pre>x</pre>");
        assert_eq!(
            html("[q](https://a.b/?q=\"x\"&r=<1>)"),
            "<a href=\"https://a.b/?q=&quot;x&quot;&amp;r=&lt;1&gt;\">q</a>"
        );
    }

    #[test]
    fn only_web_links_become_links() {
        for text in [
            "[x](javascript:alert(1))",
            "[x](JavaScript:alert(1))",
            "[x](data:text/html,hi)",
            "[x](tg://user?id=123)",
            "[x](mailto:a@b.c)",
            "[x](/relative/path)",
            "[x](https://)",
            "[x](https://a.b/with space)",
            "[x](ftp://a.b/f)",
            "[](https://a.b)",
        ] {
            let shown = html(text);
            assert!(!shown.contains("<a "), "{text}: {shown}");
            assert_eq!(shown, escape(text, false), "{text}");
        }
        let long = format!("[x](https://a.b/{})", "y".repeat(MAX_URL));
        assert!(!html(&long).contains("<a "));
        assert_eq!(
            html("[x](HTTPS://A.B/c)"),
            "<a href=\"HTTPS://A.B/c\">x</a>"
        );
    }

    #[test]
    fn an_address_too_long_once_escaped_stays_text() {
        // 1012 characters as written, five times that in the `href`.
        let text = format!("[hello world](https://a.b/?{})", "&".repeat(1000));
        let parts = render(&text, false, LIMIT);
        check(&parts, LIMIT);
        assert!(parts.iter().all(|p| !p.html.contains("<a ")), "{parts:#?}");
        // Cut by length (no per-character pieces): about 5000 characters.
        assert!(parts.len() <= 3, "{}", parts.len());
        // Cut once at the space (dropped), then between characters.
        let joined: String = parts.iter().map(|p| p.plain.as_str()).collect();
        assert_eq!(joined, text.replacen(' ', "", 1));
        // Just under the limit once escaped: still a link.
        let fits = format!("https://a.b/{}", "&".repeat((MAX_URL - 12) / 5));
        assert!(html(&format!("[x]({fits})")).starts_with("<a href="));
    }

    /// Checks every message: within `limit`, tags balanced and in order.
    fn check(parts: &[Part], limit: usize) {
        for part in parts {
            assert!(
                length(&part.html) <= limit,
                "{} > {limit}",
                length(&part.html)
            );
            let mut stack = Vec::new();
            let mut rest = part.html.as_str();
            while let Some(at) = rest.find('<') {
                let end = rest[at..].find('>').expect("tag ends") + at;
                let tag = &rest[at + 1..end];
                if let Some(name) = tag.strip_prefix('/') {
                    assert_eq!(stack.pop(), Some(name.to_string()), "{}", part.html);
                } else {
                    stack.push(tag.split(' ').next().unwrap().to_string());
                }
                rest = &rest[end + 1..];
            }
            assert!(stack.is_empty(), "unclosed {stack:?}: {}", part.html);
            // No entity is cut.
            for piece in part.html.split('&').skip(1) {
                assert!(
                    ["lt;", "gt;", "amp;", "quot;"]
                        .iter()
                        .any(|e| piece.starts_with(e)),
                    "{}",
                    part.html
                );
            }
        }
    }

    #[test]
    fn a_cut_closes_the_open_tags_and_opens_them_again() {
        let code: Vec<String> = (0..40).map(|n| format!("let x{n} = a < b && c;")).collect();
        let text = format!(
            "> **quoted** start\n\n```rust\n{}\n```\nend",
            code.join("\n")
        );
        let parts = render(&text, false, 200);
        check(&parts, 200);
        assert!(parts.len() > 3, "{parts:#?}");
        for part in &parts[1..parts.len() - 1] {
            assert!(
                part.html
                    .starts_with("<pre><code class=\"language-rust\">let x"),
                "{}",
                part.html
            );
            assert!(part.html.ends_with("</code></pre>"), "{}", part.html);
        }
        // Cut on line breaks: the plain texts joined are the text shown.
        let plain: Vec<&str> = parts.iter().map(|p| p.plain.as_str()).collect();
        assert_eq!(
            plain.join("\n"),
            format!("quoted start\n\n{}\nend", code.join("\n"))
        );
    }

    #[test]
    fn a_long_line_is_cut_inside_a_tag_and_never_inside_an_entity() {
        let text = format!("**{}**", "a&b ".repeat(100).trim_end());
        for limit in [64, 65, 66, 67, 100, 333] {
            let parts = render(&text, false, limit);
            check(&parts, limit);
            assert!(parts.len() > 1);
            for part in &parts {
                assert!(part.html.starts_with("<b>"), "{}", part.html);
                assert!(part.html.ends_with("</b>"), "{}", part.html);
            }
            let words: usize = parts.iter().map(|p| p.plain.split(' ').count()).sum();
            assert_eq!(words, 100, "{limit}");
        }
        // No space at all: cut between characters, Hangul too.
        let text = format!("`{}`", "가<".repeat(300));
        let parts = render(&text, false, 100);
        check(&parts, 100);
        let joined: String = parts.iter().map(|p| p.plain.as_str()).collect();
        assert_eq!(joined, "가<".repeat(300));
    }

    #[test]
    fn a_link_cut_in_two_keeps_its_address_in_both() {
        let text = format!("[{}](https://example.com/x)", "word ".repeat(40).trim_end());
        let parts = render(&text, false, 100);
        check(&parts, 100);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(
                part.html.starts_with("<a href=\"https://example.com/x\">"),
                "{}",
                part.html
            );
        }
    }

    #[test]
    fn mentions_are_broken_outside_code_only() {
        let broken = "@\u{2060}";
        let parts = render(
            "ask @alice, `@dataclass` and\n```py\n@decorator\n```\n[@bob](https://a.b) a@b.com",
            true,
            LIMIT,
        );
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        assert_eq!(
            part.html,
            format!(
                "ask {broken}alice, <code>@dataclass</code> and\n<pre><code class=\"language-py\">\
                 @decorator</code></pre>\n<a href=\"https://a.b\">{broken}bob</a> a@b.com"
            )
        );
        // The plain text has no code, so every mention is broken there.
        assert_eq!(
            part.plain,
            format!(
                "ask {broken}alice, {broken}dataclass and\n{broken}decorator\n{broken}bob a@b.com"
            )
        );
        // Names in a web address stay whole, as before.
        let parts = render("see https://medium.com/@alice/post", true, LIMIT);
        assert_eq!(parts[0].html, "see https://medium.com/@alice/post");
        // Context runs across tags, as Telegram sees the text.
        let parts = render("**x**@bob", true, LIMIT);
        assert_eq!(parts[0].html, "<b>x</b>@bob");
        // Private chats are left alone.
        let parts = render("ask @alice", false, LIMIT);
        assert_eq!(parts[0].html, "ask @alice");
    }

    #[test]
    fn blank_messages_are_left_out() {
        assert!(render("", false, LIMIT).is_empty());
        assert!(render("  \n\n", false, LIMIT).is_empty());
        assert!(render("```\n\n```", false, LIMIT).is_empty());
    }
}
