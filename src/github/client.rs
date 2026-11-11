//! The REST client: headers, rate-limit waits, pages and error messages
//! that never hold the token or a URL with it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode, Url};
use serde_json::Value;

use super::Token;
use super::format::inline;

pub const API_VERSION: &str = "2022-11-28";
/// Items asked for per page, the most GitHub gives.
const PER_PAGE: usize = 100;
/// Pages read for one list at most.
const MAX_PAGES: usize = 10;
const TIMEOUT: Duration = Duration::from_secs(30);
/// The longest rate-limit wait before a retry; a longer one fails at once.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Retries after a rate-limit answer.
const RETRIES: u32 = 2;
/// Characters of GitHub's error message shown.
const ERROR_CHARS: usize = 500;

/// A REST v3 client for one API root and token.
pub struct Client {
    http: reqwest::Client,
    /// The API root, ending in `/`.
    base: Url,
    token: Token,
    max_wait: Duration,
    retries: u32,
}

/// What a list request returned.
pub struct Listed {
    pub items: Vec<Value>,
    /// Whether GitHub had more than was read.
    pub more: bool,
}

impl Client {
    pub fn new(api_url: &Url, token: Token) -> anyhow::Result<Self> {
        let mut base = api_url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            // A redirect to another host would carry the token there.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| anyhow::anyhow!("cannot set up the GitHub client: {}", describe(err)))?;
        Ok(Self {
            http,
            base,
            token,
            max_wait: MAX_WAIT,
            retries: RETRIES,
        })
    }

    /// Waits at most `max_wait` for a rate limit to reset (tests).
    pub fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    pub(crate) fn token(&self) -> &Token {
        &self.token
    }

    /// The API root, for messages.
    pub fn base(&self) -> &Url {
        &self.base
    }

    pub async fn get(&self, path: &str) -> Result<Value, String> {
        self.request(Method::GET, path, None).await
    }

    /// Sends a request to `path` (relative to the API root, without a
    /// leading `/`) and returns the JSON body (`null` when empty).
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let url = self.url(path)?;
        self.send(method, url, body).await.map(|(body, _)| body)
    }

    /// Reads a list over its pages (`Link: rel="next"`), up to `max` items.
    /// `key` names the array inside an object body (`check_runs`).
    pub async fn list(&self, path: &str, key: Option<&str>, max: usize) -> Result<Listed, String> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let per_page = max.clamp(1, PER_PAGE);
        let mut url = self.url(&format!("{path}{separator}per_page={per_page}"))?;
        let mut items = Vec::new();
        for _ in 0..MAX_PAGES {
            let (body, next) = self.send(Method::GET, url, None).await?;
            let page = match key {
                Some(key) => body.get(key).cloned().unwrap_or(Value::Null),
                None => body,
            };
            let Value::Array(page) = page else {
                return Err("GitHub sent a list reply without a list".into());
            };
            items.extend(page);
            if items.len() >= max {
                let more = items.len() > max || next.is_some();
                items.truncate(max);
                return Ok(Listed { items, more });
            }
            match next {
                Some(next) => url = next,
                None => return Ok(Listed { items, more: false }),
            }
        }
        Ok(Listed { items, more: true })
    }

    fn url(&self, path: &str) -> Result<Url, String> {
        self.base
            .join(path)
            .map_err(|err| format!("bad request path: {err}"))
    }

    /// One request, retried after rate-limit answers; the body and the
    /// next page's URL.
    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<&Value>,
    ) -> Result<(Value, Option<Url>), String> {
        let mut attempt = 0;
        loop {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .headers(self.headers());
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .map_err(|err| format!("cannot reach {}: {}", self.host(), describe(err)))?;
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(wait) = rate_limit_wait(status, &headers) {
                if attempt < self.retries && wait <= self.max_wait {
                    attempt += 1;
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Err(format!(
                    "GitHub's rate limit is used up ({status}); it allows requests again in \
                     {} s. Try later.",
                    wait.as_secs()
                ));
            }
            let text = response
                .text()
                .await
                .map_err(|err| format!("cannot read GitHub's answer: {}", describe(err)))?;
            if !status.is_success() {
                return Err(error_message(status, &text));
            }
            let body = if text.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text)
                    .map_err(|_| format!("GitHub answered {status} with a body that is not JSON"))?
            };
            let next = self.next_page(&headers)?;
            return Ok((body, next));
        }
    }

    fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("junees/", env!("CARGO_PKG_VERSION"))),
        );
        // `Token::new` refused anything a header cannot hold.
        if let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", self.token.value())) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        headers
    }

    fn host(&self) -> String {
        self.base.host_str().unwrap_or_default().to_string()
    }

    /// The `rel="next"` link, which must stay under the API root: the
    /// token goes with the request.
    fn next_page(&self, headers: &HeaderMap) -> Result<Option<Url>, String> {
        let Some(link) = headers.get("link").and_then(|value| value.to_str().ok()) else {
            return Ok(None);
        };
        for part in link.split(',') {
            let mut pieces = part.split(';').map(str::trim);
            let Some(target) = pieces.next() else {
                continue;
            };
            if !pieces.any(|piece| piece == "rel=\"next\"") {
                continue;
            }
            let target = target.trim_start_matches('<').trim_end_matches('>');
            let next = Url::parse(target).map_err(|_| "GitHub sent a bad next-page link")?;
            let same_root = next.scheme() == self.base.scheme()
                && next.host_str() == self.base.host_str()
                && next.port_or_known_default() == self.base.port_or_known_default()
                && next.path().starts_with(self.base.path());
            if !same_root {
                return Err(format!(
                    "GitHub's next-page link leads outside {}; it was not followed",
                    self.base
                ));
            }
            return Ok(Some(next));
        }
        Ok(None)
    }
}

/// How long to wait when GitHub refused a request for its rate limit:
/// 429, or 403 with `retry-after` or no requests left. `retry-after`
/// first, then `x-ratelimit-reset`, else a minute (GitHub's advice for
/// secondary limits without either).
fn rate_limit_wait(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let retry_after = header("retry-after").and_then(|value| value.parse::<u64>().ok());
    let used_up = header("x-ratelimit-remaining") == Some("0");
    let limited = status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (retry_after.is_some() || used_up));
    if !limited {
        return None;
    }
    let secs = retry_after
        .or_else(|| {
            used_up
                .then(|| header("x-ratelimit-reset")?.parse::<u64>().ok())
                .flatten()
                .map(|reset| reset.saturating_sub(now_secs()))
        })
        .unwrap_or(60);
    Some(Duration::from_secs(secs))
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// GitHub's error for the model: the status, its `message` and the
/// `errors` details, shortened and with hidden characters escaped.
fn error_message(status: StatusCode, text: &str) -> String {
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let mut message = body["message"].as_str().unwrap_or_default().to_string();
    if let Some(errors) = body["errors"].as_array() {
        for error in errors {
            let detail = error["message"]
                .as_str()
                .map(str::to_string)
                .or_else(|| error.as_str().map(str::to_string))
                .unwrap_or_else(|| {
                    format!(
                        "{} {}",
                        error["field"].as_str().unwrap_or_default(),
                        error["code"].as_str().unwrap_or_default()
                    )
                });
            message.push_str("; ");
            message.push_str(&detail);
        }
    }
    let hint = match status.as_u16() {
        401 => " (the token was refused: check JUNEES_GITHUB_TOKEN)",
        403 => " (the token may lack the permission for this)",
        404 => " (not found, or the token cannot see it)",
        _ => "",
    };
    let message = inline(&message, ERROR_CHARS);
    if message.is_empty() {
        format!("GitHub answered {status}{hint}")
    } else {
        format!("GitHub answered {status}{hint}: {message}")
    }
}

/// A transport error without the request URL, and with what caused it.
fn describe(err: reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "cannot connect"
    } else {
        "request failed"
    };
    let err = err.without_url();
    let mut text = format!("{kind}: {err}");
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}
