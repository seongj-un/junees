//! Discord's REST API, as far as the gateway needs it: where the gateway
//! is, sending messages and the typing indicator.
//!
//! Rate limits (developer docs, "Rate Limits"): every answer may carry
//! `X-RateLimit-Remaining` and `X-RateLimit-Reset-After`; when a route has
//! no requests left, the next one on it waits for the reset. A 429 answer
//! says how long to wait in `retry_after` (seconds, with decimals; the
//! `Retry-After` header too), and `global: true` holds back every route.
//!
//! A message post that fails after sending (no answer, a 5xx) may have
//! created the message anyway, so posts carry a `nonce` with
//! `enforce_nonce: true` (developer docs, "Message", Create Message: "If
//! true and nonce is present, it will be checked for uniqueness in the past
//! few minutes. If another message was created by the same author with the
//! same nonce, that message will be returned and no new message will be
//! created."); every try of one post sends the same nonce.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::Log;
use super::config::{DiscordConfig, Token};

/// The User-Agent Discord asks bots for: `DiscordBot ($url, $versionNumber)`.
pub const AGENT: &str = concat!(
    "DiscordBot (https://github.com/seongj-un/junees, ",
    env!("CARGO_PKG_VERSION"),
    ")"
);

/// 429 answers taken (and waited out) before a request gives up.
const MAX_RATE_LIMITED: usize = 5;
/// Tries of a request that failed on the network or with a 5xx.
const MAX_TRIES: usize = 3;
/// The pause before trying again after a network error or a 5xx.
const RETRY_PAUSE: Duration = Duration::from_millis(500);
/// The longest wait a rate limit may impose before the request fails
/// instead: a reply should not sit for hours.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// A client of the REST API.
pub struct Rest {
    client: reqwest::Client,
    base: String,
    token: Token,
    limits: tokio::sync::Mutex<Limits>,
    log: Log,
}

#[derive(Default)]
struct Limits {
    /// Routes that have no requests left until the instant.
    routes: HashMap<String, Instant>,
    /// Every route waits until then (a global 429).
    global: Option<Instant>,
}

impl Rest {
    pub fn new(config: &DiscordConfig, log: Log) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("cannot set up the HTTP client")?;
        Ok(Self {
            client,
            base: config.api_url.clone(),
            token: config.token.clone(),
            limits: tokio::sync::Mutex::new(Limits::default()),
            log,
        })
    }

    /// Get Gateway Bot: the WebSocket URL to connect to. It also checks
    /// the token (401 when it is wrong).
    pub async fn gateway_url(&self) -> Result<String> {
        let body = self
            .request(Method::GET, "GET /gateway/bot", "/gateway/bot", None)
            .await?;
        body.get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("Discord's Get Gateway Bot answer has no url")
    }

    /// Create Message with `content` (at most 2000 characters). Mentions
    /// in it do not notify anyone (`allowed_mentions: {parse: []}`): the
    /// text comes from the model. A retried post cannot show twice: its
    /// tries share one enforced `nonce` (module docs).
    pub async fn create_message(&self, channel: &str, content: &str) -> Result<()> {
        let body = json!({
            "content": content,
            "allowed_mentions": {"parse": []},
            "nonce": nonce(),
            "enforce_nonce": true,
        });
        self.request(
            Method::POST,
            &format!("POST /channels/{channel}/messages"),
            &format!("/channels/{channel}/messages"),
            Some(body),
        )
        .await
        .map(drop)
    }

    /// Trigger Typing Indicator: shows "typing" for 10 seconds.
    pub async fn typing(&self, channel: &str) -> Result<()> {
        self.request(
            Method::POST,
            &format!("POST /channels/{channel}/typing"),
            &format!("/channels/{channel}/typing"),
            None,
        )
        .await
        .map(drop)
    }

    /// Sends one request, waiting out rate limits and retrying network
    /// errors and 5xx answers a few times. `route` names the rate limit
    /// (method, path with its top-level id) and shows in errors; the
    /// token never does.
    async fn request(
        &self,
        method: Method,
        route: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut auth = HeaderValue::from_str(&format!("Bot {}", self.token.expose()))
            .map_err(|_| anyhow!("the Discord bot token has characters a header cannot hold"))?;
        auth.set_sensitive(true);
        let mut limited = 0;
        let mut tries = 0;
        loop {
            self.wait_for(route).await;
            tries += 1;
            let mut request = self
                .client
                .request(method.clone(), format!("{}{path}", self.base))
                .header(AUTHORIZATION, auth.clone())
                .header(USER_AGENT, AGENT);
            request = match &body {
                Some(body) => request.json(body),
                // Discord wants a Content-Type, even on an empty POST.
                None if method == Method::POST => request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body("{}"),
                None => request,
            };
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) if tries < MAX_TRIES => {
                    (self.log)(&format!("Discord {route}: {}; trying again", describe(err)));
                    tokio::time::sleep(RETRY_PAUSE).await;
                    continue;
                }
                Err(err) => bail!("Discord {route}: {}", describe(err)),
            };
            let status = response.status();
            let headers = response.headers().clone();
            self.note_headers(route, &headers).await;
            let text = match response.text().await {
                Ok(text) => text,
                Err(err) if tries < MAX_TRIES => {
                    (self.log)(&format!("Discord {route}: {}; trying again", describe(err)));
                    tokio::time::sleep(RETRY_PAUSE).await;
                    continue;
                }
                Err(err) => bail!("Discord {route}: {}", describe(err)),
            };
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if status == StatusCode::TOO_MANY_REQUESTS {
                limited += 1;
                let wait = retry_after(&value, &headers);
                let global = value.get("global").and_then(Value::as_bool) == Some(true)
                    || header(&headers, "x-ratelimit-global").is_some_and(|v| v == "true");
                if limited > MAX_RATE_LIMITED || wait > MAX_WAIT {
                    bail!(
                        "Discord {route}: still rate limited after {limited} tries (asked to \
                         wait {:.1}s)",
                        wait.as_secs_f64()
                    );
                }
                (self.log)(&format!(
                    "Discord {route}: rate limited{}, waiting {:.2}s",
                    if global { " (global)" } else { "" },
                    wait.as_secs_f64()
                ));
                let until = Instant::now() + wait;
                let mut limits = self.limits.lock().await;
                if global {
                    limits.global = Some(until);
                } else {
                    limits.routes.insert(route.to_string(), until);
                }
                continue;
            }
            if status.is_server_error() && tries < MAX_TRIES {
                (self.log)(&format!("Discord {route}: {status}; trying again"));
                tokio::time::sleep(RETRY_PAUSE).await;
                continue;
            }
            if !status.is_success() {
                bail!("Discord {route}: {status}{}", detail(&value));
            }
            return Ok(value);
        }
    }

    /// Sleeps until `route` (and every route, after a global limit) may be
    /// used again.
    async fn wait_for(&self, route: &str) {
        let until = {
            let limits = self.limits.lock().await;
            [limits.global, limits.routes.get(route).copied()]
                .into_iter()
                .flatten()
                .max()
        };
        if let Some(until) = until
            && until > Instant::now()
        {
            tokio::time::sleep_until(until).await;
        }
    }

    /// Remembers when `route` runs out of requests.
    async fn note_headers(&self, route: &str, headers: &HeaderMap) {
        let remaining = header(headers, "x-ratelimit-remaining");
        let reset_after = header(headers, "x-ratelimit-reset-after")
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|secs| secs.is_finite() && *secs >= 0.0);
        let mut limits = self.limits.lock().await;
        match (remaining.as_deref(), reset_after) {
            (Some("0"), Some(secs)) => {
                let wait = Duration::from_secs_f64(secs).min(MAX_WAIT);
                limits
                    .routes
                    .insert(route.to_string(), Instant::now() + wait);
            }
            (Some(_), _) => {
                limits.routes.remove(route);
            }
            _ => {}
        }
    }
}

/// A Create Message `nonce` (docs: up to 25 characters): 16 hex digits,
/// random and different for every call.
fn nonce() -> String {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(CALLS.fetch_add(1, Ordering::Relaxed));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    hasher.write_u128(now);
    format!("{:016x}", hasher.finish())
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
}

/// How long a 429 asks to wait: `retry_after` in the body, else the
/// `Retry-After` header, else a second.
fn retry_after(body: &Value, headers: &HeaderMap) -> Duration {
    body.get("retry_after")
        .and_then(Value::as_f64)
        .or_else(|| header(headers, "retry-after").and_then(|v| v.parse::<f64>().ok()))
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map_or(Duration::from_secs(1), Duration::from_secs_f64)
}

/// Discord's error message and code, shortened, for an error line.
fn detail(body: &Value) -> String {
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .map(|message| message.chars().take(200).collect::<String>());
    let code = body.get("code").and_then(Value::as_i64);
    match (message, code) {
        (Some(message), Some(code)) => format!(" ({message}, code {code})"),
        (Some(message), None) => format!(" ({message})"),
        _ => String::new(),
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

/// A shared client, as the gateway hands it to every channel.
pub type SharedRest = Arc<Rest>;
