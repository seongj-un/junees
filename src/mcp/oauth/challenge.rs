//! The `WWW-Authenticate` challenges of a 401 or 403 answer (RFC 9110
//! section 11.6.1; the Bearer parameters of RFC 6750 section 3 and
//! `resource_metadata` of RFC 9728 section 5.1).
//!
//! MCP 2026-07-28, authorization-server-discovery.md: "MCP clients MUST be
//! able to parse `WWW-Authenticate` headers" and use `resource_metadata`
//! when present; authorization.md, "Scope Selection Strategy": the
//! challenge's `scope` comes first.

/// What a `Bearer` challenge said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bearer {
    pub resource_metadata: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
}

impl Bearer {
    /// The first `Bearer` challenge of the header values, if any.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Option<Self> {
        headers
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(Self::parse)
    }

    /// The first `Bearer` challenge in one header value.
    pub fn parse(value: &str) -> Option<Self> {
        let challenge = challenges(value)
            .into_iter()
            .find(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))?;
        let param = |name: &str| {
            challenge
                .1
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
                .filter(|value| !value.is_empty())
        };
        Some(Self {
            resource_metadata: param("resource_metadata"),
            scope: param("scope"),
            error: param("error"),
        })
    }
}

/// Every challenge of `value`: its scheme and its `name=value` parameters
/// (quoted values unescaped). A token68 is kept as a parameter without a
/// name.
fn challenges(value: &str) -> Vec<(String, Vec<(String, String)>)> {
    let chars: Vec<char> = value.chars().collect();
    let len = chars.len();
    let blank = |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n';
    let skip = |at: &mut usize, also_commas: bool| {
        while *at < len && (blank(chars[*at]) || also_commas && chars[*at] == ',') {
            *at += 1;
        }
    };
    let token = |at: &mut usize| {
        let start = *at;
        while *at < len && !blank(chars[*at]) && !matches!(chars[*at], ',' | '=' | '"') {
            *at += 1;
        }
        chars[start..*at].iter().collect::<String>()
    };
    let mut at = 0;
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    loop {
        skip(&mut at, true);
        if at >= len {
            break;
        }
        let word = token(&mut at);
        if word.is_empty() {
            // A stray character.
            at += 1;
            continue;
        }
        skip(&mut at, false);
        if at < len && chars[at] == '=' {
            at += 1;
            skip(&mut at, false);
            let value = value_at(&chars, &mut at);
            // A parameter before any scheme belongs to no challenge.
            if let Some((_, params)) = out.last_mut() {
                params.push((word, value));
            }
            continue;
        }
        out.push((word, Vec::new()));
        // A token68 right after the scheme (`Negotiate abc==`): a token,
        // any `=`, then the end or a comma.
        let start = at;
        let candidate = token(&mut at);
        let mut end = at;
        while end < len && chars[end] == '=' {
            end += 1;
        }
        let mut after = end;
        while after < len && blank(chars[after]) {
            after += 1;
        }
        if !candidate.is_empty() && (after >= len || chars[after] == ',') {
            let text: String = chars[start..end].iter().collect();
            out.last_mut().unwrap().1.push((String::new(), text));
            at = end;
        } else {
            at = start;
        }
    }
    out
}

/// A quoted string (backslash escapes undone) or a token.
fn value_at(chars: &[char], at: &mut usize) -> String {
    let mut value = String::new();
    if *at < chars.len() && chars[*at] == '"' {
        *at += 1;
        while *at < chars.len() && chars[*at] != '"' {
            if chars[*at] == '\\' && *at + 1 < chars.len() {
                *at += 1;
            }
            value.push(chars[*at]);
            *at += 1;
        }
        *at += 1;
    } else {
        while *at < chars.len() && !matches!(chars[*at], ',' | ' ' | '\t') {
            value.push(chars[*at]);
            *at += 1;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_bearer_parameters() {
        // The examples of MCP 2026-07-28 authorization.md.
        let bearer = Bearer::parse(
            "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\",\n                         scope=\"files:read\"",
        )
        .unwrap();
        assert_eq!(
            bearer.resource_metadata.as_deref(),
            Some("https://mcp.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(bearer.scope.as_deref(), Some("files:read"));
        assert_eq!(bearer.error, None);

        let bearer = Bearer::parse(
            r#"Bearer error="insufficient_scope", scope="files:write", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource", error_description="File write permission required""#,
        )
        .unwrap();
        assert_eq!(bearer.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(bearer.scope.as_deref(), Some("files:write"));
    }

    #[test]
    fn finds_bearer_among_other_challenges() {
        let bearer = Bearer::parse(
            r#"Basic realm="a, b", Negotiate abc==, bearer realm=x,scope="a \"b\"", Other k=v"#,
        )
        .unwrap();
        assert_eq!(bearer.scope.as_deref(), Some("a \"b\""));
        assert_eq!(bearer.resource_metadata, None);
        assert_eq!(Bearer::parse("Basic realm=\"x\""), None);
        assert_eq!(Bearer::parse(""), None);
        assert_eq!(Bearer::parse("Bearer").unwrap(), Bearer::default());
        // Unquoted values and odd spacing.
        let bearer =
            Bearer::parse("Bearer   resource_metadata = https://r.example/m ,scope=x").unwrap();
        assert_eq!(
            bearer.resource_metadata.as_deref(),
            Some("https://r.example/m")
        );
        assert_eq!(bearer.scope.as_deref(), Some("x"));
        // An unterminated quote ends the value at the end.
        let bearer = Bearer::parse("Bearer scope=\"a b").unwrap();
        assert_eq!(bearer.scope.as_deref(), Some("a b"));
    }

    #[test]
    fn several_header_values_are_searched() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append("www-authenticate", "Basic realm=x".parse().unwrap());
        headers.append(
            "www-authenticate",
            "Bearer resource_metadata=\"https://r.example/m\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            Bearer::from_headers(&headers)
                .unwrap()
                .resource_metadata
                .as_deref(),
            Some("https://r.example/m")
        );
        assert_eq!(
            Bearer::from_headers(&reqwest::header::HeaderMap::new()),
            None
        );
    }
}
