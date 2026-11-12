//! The HTTP requests of the login and of a token refresh: metadata
//! documents, client registration and the token endpoint.
//!
//! Like the MCP transport: https only (http to this computer), redirects
//! are not followed, answers are read up to a size limit, and errors
//! never carry the URL or a header value.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Client, RequestBuilder, StatusCode, Url};
use serde_json::Value;

use crate::mcp::http::target::is_loopback;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one request may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// A larger answer is refused.
const MAX_BYTES: usize = 1024 * 1024;

/// `text` as the URL of `what`, if it is https (or http to this computer)
/// without a user name, password or fragment. MCP 2026-07-28,
/// security-considerations.md, "Communication Security": every
/// authorization server endpoint MUST be served over HTTPS.
pub fn endpoint(text: &str, what: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|_| format!("{what} is not a valid absolute URL"))?;
    let secure = url.scheme() == "https" || url.scheme() == "http" && is_loopback(&url);
    if !secure {
        return Err(format!(
            "{what} `{}` is not https (http is allowed only to this computer)",
            origin(&url)
        ));
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{what} has no host, or a user name or password"));
    }
    if url.fragment().is_some() {
        return Err(format!("{what} `{}` has a fragment", origin(&url)));
    }
    Ok(url)
}

/// `https://host:port`: what is shown of a URL.
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// An answer, read whole.
pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Answer {
    /// The body as a JSON object, if it is one.
    pub fn object(&self) -> Option<serde_json::Map<String, Value>> {
        match serde_json::from_slice(&self.body) {
            Ok(Value::Object(map)) => Some(map),
            _ => None,
        }
    }
}

/// Sends the request `build` makes for `url` and reads the answer.
async fn send(url: &Url, build: impl FnOnce(&Client) -> RequestBuilder) -> Result<Answer, String> {
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TIMEOUT);
    if is_loopback(url) {
        builder = builder.no_proxy();
    }
    let client = builder.build().map_err(|err| {
        format!(
            "cannot set up the HTTP client: {}",
            crate::mcp::http::describe(&err)
        )
    })?;
    let mut response = build(&client)
        .send()
        .await
        .map_err(|err| crate::mcp::http::describe(&err))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(format!(
            "it answered with a redirect (HTTP {}), which junees does not follow",
            status.as_u16()
        ));
    }
    let headers = response.headers().clone();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| crate::mcp::http::describe(&err))?
    {
        if body.len() + chunk.len() > MAX_BYTES {
            return Err("its answer is larger than 1 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Answer {
        status,
        headers,
        body,
    })
}

pub async fn get(url: &Url) -> Result<Answer, String> {
    send(url, |client| client.get(url.clone()).header(ACCEPT, JSON)).await
}

/// A POST of `body` with `headers`, which set `Accept` themselves.
pub async fn post_with(url: &Url, headers: HeaderMap, body: String) -> Result<Answer, String> {
    send(url, |client| {
        client.post(url.clone()).headers(headers).body(body)
    })
    .await
}

const JSON: HeaderValue = HeaderValue::from_static("application/json");

/// A form POST (`application/x-www-form-urlencoded`), with HTTP Basic
/// client authentication when `basic` is given (RFC 6749 section 2.3.1:
/// both parts form-encoded first).
pub async fn post_form(
    url: &Url,
    form: &[(&str, &str)],
    basic: Option<(&str, &str)>,
) -> Result<Answer, String> {
    let body = encode_form(form);
    send(url, |client| {
        let mut request = client
            .post(url.clone())
            .header(ACCEPT, JSON)
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            )
            .body(body);
        if let Some((id, secret)) = basic {
            use base64::Engine;
            let pair = format!("{}:{}", encode_component(id), encode_component(secret));
            let value = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            );
            if let Ok(mut value) = HeaderValue::from_str(&value) {
                value.set_sensitive(true);
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
        }
        request
    })
    .await
}

pub async fn post_json(url: &Url, body: &Value) -> Result<Answer, String> {
    let text = body.to_string();
    send(url, |client| {
        client
            .post(url.clone())
            .header(ACCEPT, JSON)
            .header(CONTENT_TYPE, JSON)
            .body(text)
    })
    .await
}

/// `a=1&b=2`, form-encoded.
pub fn encode_form(pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse("http://form.invalid/").expect("valid");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_string()
}

fn encode_component(text: &str) -> String {
    let encoded = encode_form(&[("", text)]);
    encoded.trim_start_matches('=').to_string()
}

/// The `error` and `error_description` of an OAuth error answer (RFC 6749
/// section 5.2), escaped and cut short, or its status.
pub fn oauth_error(answer: &Answer) -> String {
    let fields = answer.object().unwrap_or_default();
    let text = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .map(|text| short(text, 300))
    };
    match (text("error"), text("error_description")) {
        (Some(error), Some(description)) => {
            format!("HTTP {}, `{error}`: {description}", answer.status.as_u16())
        }
        (Some(error), None) => format!("HTTP {}, `{error}`", answer.status.as_u16()),
        _ => format!("HTTP {}", answer.status.as_u16()),
    }
}

/// `text` with hidden characters escaped, cut to `chars` characters.
pub fn short(text: &str, chars: usize) -> String {
    let escaped = crate::prompt_text::escape_hidden(text);
    match escaped.char_indices().nth(chars) {
        Some((at, _)) => format!("{}...", &escaped[..at]),
        None => escaped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_https_or_loopback() {
        assert!(endpoint("https://auth.example.com/token", "x").is_ok());
        assert!(endpoint("http://127.0.0.1:9/token", "x").is_ok());
        assert!(endpoint("http://localhost/token", "x").is_ok());
        let message =
            endpoint("http://auth.example.com/t0ken-path", "the token endpoint").unwrap_err();
        assert!(message.contains("not https"), "{message}");
        assert!(!message.contains("t0ken-path"), "{message}");
        assert!(endpoint("https://u:p@auth.example.com/", "x").is_err());
        assert!(endpoint("https://auth.example.com/#f", "x").is_err());
        assert!(endpoint("auth.example.com", "x").is_err());
    }

    #[test]
    fn forms_are_encoded() {
        assert_eq!(
            encode_form(&[
                ("a", "b c"),
                ("redirect_uri", "http://127.0.0.1:1/cb?x=1&y")
            ]),
            "a=b+c&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcb%3Fx%3D1%26y"
        );
        assert_eq!(encode_component("a:b c"), "a%3Ab+c");
    }

    #[test]
    fn oauth_errors_are_escaped_and_short() {
        let answer = |body: &str| Answer {
            status: StatusCode::BAD_REQUEST,
            headers: HeaderMap::new(),
            body: body.as_bytes().to_vec(),
        };
        assert_eq!(
            oauth_error(&answer(
                r#"{"error": "invalid_grant", "error_description": "code\u001b[2J used"}"#
            )),
            "HTTP 400, `invalid_grant`: code\\u{1b}[2J used"
        );
        assert_eq!(oauth_error(&answer("<html>")), "HTTP 400");
        let long = format!(
            r#"{{"error": "x", "error_description": "{}"}}"#,
            "a".repeat(500)
        );
        assert!(oauth_error(&answer(&long)).ends_with("..."));
    }
}
