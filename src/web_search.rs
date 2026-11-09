//! Anthropic's web search tool, which the API runs itself (a server tool).
//! A request offers it in `tools`; the response then carries each search
//! (`server_tool_use`), its results (`web_search_tool_result`, with
//! `encrypted_content`) and text whose `citations` point at them. Junees
//! never runs a search: it keeps those blocks as the API sent them and
//! sends them back, as the docs require
//! (platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool).

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::cli::preview;
use crate::config::parse_flag;
use crate::message::ContentBlock;

/// The basic version of the tool. The later ones (`web_search_20260209`
/// and up) filter results with code execution by default, which brings a
/// container to send back across `pause_turn`, more block types, no
/// zero data retention, and is missing on some platforms; with
/// `allowed_callers: ["direct"]` they search just like this one.
pub const TOOL_TYPE: &str = "web_search_20250305";
pub const TOOL_NAME: &str = "web_search";

/// Searches allowed in one turn (`max_uses`) unless set otherwise.
pub const DEFAULT_MAX_USES: u32 = 5;

/// Added to the system prompt of every request that offers the tool.
pub const SYSTEM_NOTE: &str = "When an answer depends on current information (recent events, \
     releases, prices, documentation that may have changed), search the web with web_search \
     and name your sources.";

/// Sources listed after an answer; the rest are counted.
const SOURCES_SHOWN: usize = 10;

/// Results of one search a compaction summary names; the rest are counted.
const RESULTS_SUMMARIZED: usize = 5;
const TITLE_CHARS: usize = 80;
const URL_CHARS: usize = 120;
const QUERY_CHARS: usize = 200;

/// How the web search tool is offered. Built only when it is turned on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearch {
    /// Searches allowed in one turn; one past it gets a
    /// `max_uses_exceeded` error result, which the model handles.
    pub max_uses: u32,
    /// Search only these domains (`JUNEES_WEB_SEARCH_ALLOWED_DOMAINS`).
    pub allowed_domains: Vec<String>,
    /// Never search these (`JUNEES_WEB_SEARCH_BLOCKED_DOMAINS`).
    pub blocked_domains: Vec<String>,
}

impl WebSearch {
    /// Reads `JUNEES_WEB_SEARCH` (off by default: every search is billed
    /// on top of the tokens its results add), `JUNEES_WEB_SEARCH_MAX_USES`
    /// and the comma-separated `JUNEES_WEB_SEARCH_ALLOWED_DOMAINS` or
    /// `JUNEES_WEB_SEARCH_BLOCKED_DOMAINS`. `get` gives trimmed, non-empty
    /// values. `None` when web search is off; the other settings are then
    /// not read.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        if !parse_flag(get("JUNEES_WEB_SEARCH"), "JUNEES_WEB_SEARCH")? {
            return Ok(None);
        }
        let max_uses = match get("JUNEES_WEB_SEARCH_MAX_USES") {
            None => DEFAULT_MAX_USES,
            Some(raw) => match raw.parse::<u32>() {
                Ok(n) if n > 0 => n,
                _ => bail!("JUNEES_WEB_SEARCH_MAX_USES must be a positive integer, got `{raw}`"),
            },
        };
        let allowed_domains = domains(get("JUNEES_WEB_SEARCH_ALLOWED_DOMAINS"), "ALLOWED")?;
        let blocked_domains = domains(get("JUNEES_WEB_SEARCH_BLOCKED_DOMAINS"), "BLOCKED")?;
        // The API rejects a request with both (400).
        if !allowed_domains.is_empty() && !blocked_domains.is_empty() {
            bail!(
                "set JUNEES_WEB_SEARCH_ALLOWED_DOMAINS or JUNEES_WEB_SEARCH_BLOCKED_DOMAINS, \
                 not both"
            );
        }
        Ok(Some(Self {
            max_uses,
            allowed_domains,
            blocked_domains,
        }))
    }

    /// The tool as the request's `tools` lists it.
    pub fn definition(&self) -> Value {
        let mut tool = Map::new();
        tool.insert("type".into(), json!(TOOL_TYPE));
        tool.insert("name".into(), json!(TOOL_NAME));
        tool.insert("max_uses".into(), json!(self.max_uses));
        if !self.allowed_domains.is_empty() {
            tool.insert("allowed_domains".into(), json!(self.allowed_domains));
        }
        if !self.blocked_domains.is_empty() {
            tool.insert("blocked_domains".into(), json!(self.blocked_domains));
        }
        Value::Object(tool)
    }
}

/// A comma-separated domain list, checked against the docs' rules: no
/// scheme, ASCII only (look-alike letters get around filters), and `*`
/// only in a path after the domain.
fn domains(value: Option<String>, which: &str) -> Result<Vec<String>> {
    let name = format!("JUNEES_WEB_SEARCH_{which}_DOMAINS");
    let mut list = Vec::new();
    for entry in value.iter().flat_map(|value| value.split(',')) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let domain = entry.split('/').next().unwrap_or_default();
        if entry.contains("://") {
            bail!("{name}: write `{entry}` without the scheme (example.com, not https://...)");
        }
        if !entry.is_ascii() || entry.chars().any(char::is_whitespace) {
            bail!("{name}: `{entry}` must be a plain ASCII domain such as example.com");
        }
        if domain.is_empty() || domain.contains('*') {
            bail!("{name}: `{entry}` is not a domain (`*` may only appear in the path after it)");
        }
        list.push(entry.to_string());
    }
    Ok(list)
}

/// Adds the URLs the text blocks of `content` cite to `urls`, each once,
/// in the order they first appear.
pub fn collect_sources(content: &[ContentBlock], urls: &mut Vec<String>) {
    for block in content {
        let ContentBlock::Text {
            citations: Some(Value::Array(citations)),
            ..
        } = block
        else {
            continue;
        };
        for url in citations.iter().filter_map(|c| c["url"].as_str()) {
            if !urls.iter().any(|known| known == url) {
                urls.push(url.to_string());
            }
        }
    }
}

/// The source list shown after an answer, one URL per line.
pub fn sources_text(urls: &[String]) -> String {
    let mut text = String::from("Sources:\n");
    for url in urls.iter().take(SOURCES_SHOWN) {
        text.push_str(&format!("- {url}\n"));
    }
    if urls.len() > SOURCES_SHOWN {
        text.push_str(&format!("({} more)\n", urls.len() - SOURCES_SHOWN));
    }
    text
}

/// A short line for a web search block in the text a compaction
/// summarizes: the query, or the titles and URLs of the first results.
/// Never the results' content (`encrypted_content` is opaque and large).
/// `None` for blocks that are not about web search.
pub fn summary_line(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "server_tool_use" if block["name"] == TOOL_NAME => {
            let query = preview(
                block["input"]["query"].as_str().unwrap_or_default(),
                QUERY_CHARS,
            );
            Some(if query.is_empty() {
                "web search".into()
            } else {
                format!("web search: {query}")
            })
        }
        "web_search_tool_result" => {
            let content = &block["content"];
            let Some(results) = content.as_array() else {
                let code = content["error_code"].as_str().unwrap_or("unknown");
                return Some(format!("web search failed: {}", preview(code, TITLE_CHARS)));
            };
            if results.is_empty() {
                return Some("web search results: none".into());
            }
            let named: Vec<String> = results
                .iter()
                .take(RESULTS_SUMMARIZED)
                .map(|result| {
                    let title = result["title"].as_str().unwrap_or_default();
                    let url = result["url"].as_str().unwrap_or_default();
                    format!(
                        "{} ({})",
                        preview(title, TITLE_CHARS),
                        preview(url, URL_CHARS)
                    )
                })
                .collect();
            let mut line = format!("web search results: {}", named.join("; "));
            if results.len() > RESULTS_SUMMARIZED {
                line.push_str(&format!("; {} more", results.len() - RESULTS_SUMMARIZED));
            }
            Some(line)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read(pairs: &[(&str, &str)]) -> Result<Option<WebSearch>> {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        WebSearch::read(|key| map.get(key).map(|value| value.to_string()))
    }

    #[test]
    fn off_unless_turned_on() {
        assert_eq!(read(&[]).unwrap(), None);
        assert_eq!(read(&[("JUNEES_WEB_SEARCH", "off")]).unwrap(), None);
        // Off, the other settings are not even read.
        assert_eq!(
            read(&[("JUNEES_WEB_SEARCH_MAX_USES", "lots")]).unwrap(),
            None
        );
        let on = read(&[("JUNEES_WEB_SEARCH", "on")]).unwrap().unwrap();
        assert_eq!(
            on,
            WebSearch {
                max_uses: DEFAULT_MAX_USES,
                allowed_domains: vec![],
                blocked_domains: vec![],
            }
        );
        assert_eq!(
            on.definition(),
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5})
        );
        let err = read(&[("JUNEES_WEB_SEARCH", "maybe")]).unwrap_err();
        assert!(err.to_string().contains("JUNEES_WEB_SEARCH"), "{err}");
    }

    #[test]
    fn reads_max_uses_and_domains_into_the_definition() {
        let allowed = read(&[
            ("JUNEES_WEB_SEARCH", "1"),
            ("JUNEES_WEB_SEARCH_MAX_USES", "3"),
            (
                "JUNEES_WEB_SEARCH_ALLOWED_DOMAINS",
                "docs.rs, example.com/blog/* ,,",
            ),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            allowed.definition(),
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3,
                   "allowed_domains": ["docs.rs", "example.com/blog/*"]})
        );
        let blocked = read(&[
            ("JUNEES_WEB_SEARCH", "on"),
            ("JUNEES_WEB_SEARCH_BLOCKED_DOMAINS", "spam.example"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(
            blocked.definition(),
            json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 5,
                   "blocked_domains": ["spam.example"]})
        );
    }

    #[test]
    fn rejects_bad_settings() {
        for (key, value, expected) in [
            ("JUNEES_WEB_SEARCH_MAX_USES", "0", "positive integer"),
            ("JUNEES_WEB_SEARCH_MAX_USES", "-1", "positive integer"),
            (
                "JUNEES_WEB_SEARCH_ALLOWED_DOMAINS",
                "https://docs.rs",
                "without the scheme",
            ),
            ("JUNEES_WEB_SEARCH_BLOCKED_DOMAINS", "аmazon.com", "ASCII"),
            (
                "JUNEES_WEB_SEARCH_BLOCKED_DOMAINS",
                "*.example.com",
                "not a domain",
            ),
            ("JUNEES_WEB_SEARCH_ALLOWED_DOMAINS", "/blog", "not a domain"),
        ] {
            let err = read(&[("JUNEES_WEB_SEARCH", "on"), (key, value)])
                .err()
                .unwrap_or_else(|| panic!("{key}={value} was accepted"))
                .to_string();
            assert!(err.contains(expected), "{key}={value}: {err}");
            assert!(err.contains(key), "{err}");
        }
        let err = read(&[
            ("JUNEES_WEB_SEARCH", "on"),
            ("JUNEES_WEB_SEARCH_ALLOWED_DOMAINS", "a.com"),
            ("JUNEES_WEB_SEARCH_BLOCKED_DOMAINS", "b.com"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("not both"), "{err}");
    }

    fn cited(text: &str, urls: &[&str]) -> ContentBlock {
        let citations: Vec<Value> = urls
            .iter()
            .map(|url| {
                json!({"type": "web_search_result_location", "url": url, "title": "t",
                       "encrypted_index": "Eo8B", "cited_text": "..."})
            })
            .collect();
        ContentBlock::Text {
            text: text.into(),
            citations: Some(json!(citations)),
        }
    }

    #[test]
    fn sources_are_the_cited_urls_once_each_in_order() {
        let content = [
            cited("a", &["https://b.example/1", "https://a.example/"]),
            ContentBlock::text("plain"),
            cited("b", &["https://a.example/", "https://c.example/"]),
            ContentBlock::Other(json!({"type": "web_search_tool_result", "tool_use_id": "s",
                "content": [{"type": "web_search_result", "url": "https://not-cited"}]})),
        ];
        let mut urls = vec!["https://c.example/".to_string()];
        collect_sources(&content, &mut urls);
        assert_eq!(
            urls,
            [
                "https://c.example/",
                "https://b.example/1",
                "https://a.example/"
            ]
        );
        assert_eq!(
            sources_text(&urls[..2]),
            "Sources:\n- https://c.example/\n- https://b.example/1\n"
        );
        let many: Vec<String> = (0..13).map(|i| format!("https://{i}.example")).collect();
        let shown = sources_text(&many);
        assert!(
            shown.ends_with("- https://9.example\n(3 more)\n"),
            "{shown}"
        );
    }

    #[test]
    fn summary_lines_name_queries_and_results_but_not_their_content() {
        let call = json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                          "input": {"query": "rust 2026 edition"}});
        assert_eq!(
            summary_line(&call).unwrap(),
            "web search: rust 2026 edition"
        );
        let results: Vec<Value> = (0..40)
            .map(|i| {
                json!({"type": "web_search_result", "url": format!("https://{i}.example/page"),
                       "title": format!("제목 {i} {}", "긴 제목 ".repeat(50)),
                       "encrypted_content": "E".repeat(20_000), "page_age": null})
            })
            .collect();
        let result = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                            "content": results});
        let line = summary_line(&result).unwrap();
        assert!(line.starts_with("web search results: 제목 0 "), "{line}");
        assert!(line.contains("(https://4.example/page)"), "{line}");
        assert!(!line.contains("https://5.example"), "{line}");
        assert!(line.ends_with("; 35 more"), "{line}");
        assert!(!line.contains("EEEE"));
        assert!(line.chars().count() < 1000, "{}", line.chars().count());

        let failed = json!({"type": "web_search_tool_result", "tool_use_id": "s",
            "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}});
        assert_eq!(
            summary_line(&failed).unwrap(),
            "web search failed: max_uses_exceeded"
        );
        let empty = json!({"type": "web_search_tool_result", "tool_use_id": "s", "content": []});
        assert_eq!(summary_line(&empty).unwrap(), "web search results: none");
        let other = json!({"type": "server_tool_use", "name": "web_fetch", "input": {}});
        assert_eq!(summary_line(&other), None);
        assert_eq!(summary_line(&json!({"type": "thinking"})), None);
    }
}
