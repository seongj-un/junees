//! Search queries: words that must all appear, double-quoted phrases,
//! case ignored, any script (Hangul included) as typed.

/// Terms a query may hold.
pub const MAX_TERMS: usize = 16;

/// Characters a query may hold.
pub const MAX_QUERY_CHARS: usize = 500;

/// A parsed query: its terms in matching form ([`flatten`], lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    terms: Vec<String>,
}

impl Query {
    /// Splits `text` on whitespace into terms; `"..."` keeps a phrase
    /// together (an unclosed quote runs to the end). Repeated terms count
    /// once. The error is a message for the model or the user.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.chars().count() > MAX_QUERY_CHARS {
            return Err(format!(
                "the query is longer than {MAX_QUERY_CHARS} characters; use a few distinctive words"
            ));
        }
        let mut terms: Vec<String> = Vec::new();
        let mut rest = text;
        loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                break;
            }
            let (term, next) = match rest.strip_prefix('"') {
                Some(after) => match after.find('"') {
                    Some(end) => (&after[..end], &after[end + 1..]),
                    None => (after, ""),
                },
                None => {
                    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    rest.split_at(end)
                }
            };
            let term = lower(term).trim().to_string();
            if !term.is_empty() && !terms.contains(&term) {
                terms.push(term);
            }
            rest = next;
        }
        if terms.is_empty() {
            return Err("the query is empty: give the words to look for".into());
        }
        if terms.len() > MAX_TERMS {
            return Err(format!(
                "the query has {} terms; use at most {MAX_TERMS}",
                terms.len()
            ));
        }
        Ok(Self { terms })
    }

    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    /// Whether every term appears in `text`, given in matching form
    /// ([`lower`]).
    pub fn matches(&self, lowered: &str) -> bool {
        self.terms
            .iter()
            .all(|term| lowered.contains(term.as_str()))
    }

    /// The query as it is matched: terms with spaces in quotes.
    pub fn display(&self) -> String {
        self.terms
            .iter()
            .map(|term| {
                if term.contains(' ') {
                    format!("\"{term}\"")
                } else {
                    term.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// `text` with every whitespace run made one space, case kept.
pub fn flatten(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// `text` in matching form: [`flatten`]ed and lowercased.
pub fn lower(text: &str) -> String {
    flatten(text).to_lowercase()
}

/// About `width` characters of `text`, flattened, around the first place
/// a term of `query` appears, with `...` where it was cut. Case is kept.
pub fn snippet(text: &str, query: &Query, width: usize) -> String {
    let flat = flatten(text);
    // Lowercase char by char, remembering where each lowercase char came
    // from: a few characters change length when lowercased.
    let mut lowered = String::with_capacity(flat.len());
    let mut origin = Vec::with_capacity(flat.len());
    for (at, c) in flat.char_indices() {
        for l in c.to_lowercase() {
            origin.push((lowered.len(), at));
            lowered.push(l);
        }
    }
    let first = query
        .terms
        .iter()
        .filter_map(|term| lowered.find(term.as_str()))
        .min()
        .unwrap_or(0);
    let at = origin
        .get(origin.partition_point(|&(low, _)| low < first))
        .map_or(flat.len(), |&(_, at)| at);
    let before = width / 3;
    let start = flat[..at]
        .char_indices()
        .rev()
        .nth(before.saturating_sub(1))
        .map_or(0, |(i, _)| i);
    let end = flat[start..]
        .char_indices()
        .nth(width)
        .map_or(flat.len(), |(i, _)| start + i);
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.push_str(&flat[start..end]);
    if end < flat.len() {
        out.push_str("...");
    }
    out
}

/// The first `max` bytes of `text`, cut on a char boundary.
pub fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    #[test]
    fn parses_words_and_phrases() {
        assert_eq!(q("  Build  failed ").terms(), ["build", "failed"]);
        assert_eq!(
            q("\"Cargo  Clippy\" 경고 \"빌드 실패").terms(),
            ["cargo clippy", "경고", "빌드 실패"]
        );
        assert_eq!(q("x X \"x\"").terms(), ["x"]);
        assert_eq!(q("a\"b c").terms(), ["a\"b", "c"]);
        assert_eq!(q("\"two words\" one").display(), "\"two words\" one");
        for empty in ["", "   ", "\"\"", "\" \""] {
            assert!(
                Query::parse(empty).unwrap_err().contains("empty"),
                "{empty:?}"
            );
        }
        let many = (0..=MAX_TERMS).map(|n| format!("w{n}")).collect::<Vec<_>>();
        assert!(
            Query::parse(&many.join(" "))
                .unwrap_err()
                .contains("at most")
        );
        assert!(Query::parse(&"x".repeat(MAX_QUERY_CHARS + 1)).is_err());
        assert!(Query::parse(&"한".repeat(MAX_QUERY_CHARS)).is_ok());
    }

    #[test]
    fn every_term_must_appear_case_ignored() {
        let query = q("Clippy \"BUILD failed\" 빌드");
        assert!(query.matches(&lower("cargo clippy: the build\n  FAILED in 빌드 단계")));
        assert!(!query.matches(&lower("cargo clippy: the build passed 빌드")));
        assert!(!query.matches(&lower("clippy build failed")));
        assert!(!query.matches(&lower("build failed 빌드 only")));
        assert!(q("ÄÖ").matches(&lower("äö")));
    }

    #[test]
    fn snippets_show_the_first_match_in_original_case() {
        let query = q("needle");
        assert_eq!(snippet("a NEEDLE\n\tb", &query, 100), "a NEEDLE b");
        let long = format!("{} Needle {}", "앞".repeat(200), "뒤".repeat(200));
        let cut = snippet(&long, &query, 30);
        assert!(cut.starts_with("...앞앞"), "{cut}");
        assert!(cut.ends_with("뒤..."), "{cut}");
        assert!(cut.contains("Needle"), "{cut}");
        assert_eq!(cut.chars().count(), 30 + 6, "{cut}");
        // A character that grows when lowercased before the match.
        assert_eq!(snippet("İİ needle", &query, 100), "İİ needle");
        assert_eq!(snippet("needle at start", &query, 6), "needle...");
    }

    #[test]
    fn cuts_on_char_boundaries() {
        assert_eq!(cut("한글", 4), "한");
        assert_eq!(cut("한글", 6), "한글");
        assert_eq!(cut("ab", 1), "a");
    }
}
