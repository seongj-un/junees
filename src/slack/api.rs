//! The Slack Web API, as far as the gateway needs it (docs.slack.dev):
//!
//! - Every method is `POST <base>/<method>`; the token goes in the
//!   `Authorization: Bearer` header ("Tokens should be passed as an HTTP
//!   Authorization header"), here always marked sensitive, never in a
//!   URL. Bodies are JSON (`application/json; charset=utf-8`, accepted by
//!   the write methods since 2017). An answer is `{ok: true, ...}` or
//!   `{ok: false, error}`.
//! - `auth.test` names the bot (`user_id`, `bot_id`); `apps.connections.open`
//!   takes the app-level token and gives a Socket Mode WebSocket URL;
//!   `chat.postMessage` posts (`channel`, `text`, `thread_ts`).
//! - Rate limits ("Rate limits"): "HTTP 429 Too Many Requests ... and a
//!   `Retry-After` HTTP header containing the number of seconds until you
//!   can retry". A 429 was not run, so it is waited out and sent again.
//!   `chat.postMessage` has no idempotency key: besides a 429 only a
//!   connect error (nothing sent) is tried again.
//! - Files ("Downloading files"): `url_private(_download)` with the bot
//!   token, only to `https://files.slack.com` (the parsed host, exactly),
//!   with redirects followed by hand and checked by the same rule.

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, LOCATION, RETRY_AFTER};
use reqwest::redirect::Policy;
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};

use super::config::{FILES_HOST, SlackConfig, Token};
use crate::gateway::Log;

/// 429 answers waited out before a request gives up.
const MAX_RATE_LIMITED: usize = 5;
/// Tries of a request that could not connect (so was never sent).
const MAX_TRIES: usize = 3;
/// The pause before trying again after a connect error or a 5xx.
const RETRY_PAUSE: Duration = Duration::from_millis(500);
/// The longest `Retry-After` taken before the request fails instead.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
/// Time an ordinary call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest answer read.
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
/// Redirects a file download follows.
pub const MAX_REDIRECTS: usize = 3;
/// What replaces a token in text from the server.
const HIDDEN: &str = "[hidden]";

/// `error` codes that say a token is no good: trying again cannot help.
const FATAL_ERRORS: &[&str] = &[
    "invalid_auth",
    "not_authed",
    "account_inactive",
    "token_revoked",
    "token_expired",
    "not_allowed_token_type",
    "missing_scope",
    "no_permission",
    "team_access_not_granted",
];

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// Slack answered `ok: false` (or an HTTP error).
    Refused {
        method: String,
        /// The `error` code (`invalid_auth`), else the HTTP status.
        error: String,
    },
    /// No (usable) answer: connect error, time-out, broken body.
    Transport { method: String, detail: String },
}

impl ApiError {
    /// A token is wrong or lacks a scope: running on would not help.
    pub fn is_fatal(&self) -> bool {
        matches!(self, ApiError::Refused { error, .. } if FATAL_ERRORS.contains(&error.as_str()))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Refused { method, error } => {
                write!(f, "Slack {method}: {error}")?;
                if self.is_fatal() {
                    let token = if method == "apps.connections.open" {
                        "the app-level token (slack_app_token, scope connections:write)"
                    } else {
                        "the bot token (slack_bot_token) and its scopes"
                    };
                    write!(f, "; check {token}")?;
                }
                Ok(())
            }
            ApiError::Transport { method, detail } => write!(f, "Slack {method}: {detail}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// The bot itself, from `auth.test`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
    /// The bot user's id: how mentions of the bot are written.
    pub user_id: String,
    /// The bot's id: its own messages carry it.
    pub bot_id: Option<String>,
    /// The bot user's name and the workspace, for the log.
    pub name: String,
    pub team: String,
}

/// How a call may be tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Asking again changes nothing: connect errors, 5xx answers and time
    /// limits are tried again.
    Safe,
    /// Sending again could post twice: only a connect error and a 429.
    Once,
}

/// Which token a call sends.
#[derive(Debug, Clone, Copy)]
enum Auth {
    Bot,
    App,
}

/// A client of the Web API.
pub struct Api {
    client: reqwest::Client,
    base: String,
    bot: Token,
    app: Token,
    files: Option<Url>,
    log: Log,
}

impl Api {
    pub fn new(config: &SlackConfig, log: Log) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .build()
            .map_err(|err| anyhow::anyhow!("cannot set up the HTTP client: {}", describe(err)))?;
        Ok(Self {
            client,
            base: config.api_url.clone(),
            bot: config.bot_token.clone(),
            app: config.app_token.clone(),
            files: config.files_url.clone(),
            log,
        })
    }

    /// `text` without either token.
    pub fn clean(&self, text: &str) -> String {
        text.replace(self.bot.expose(), HIDDEN)
            .replace(self.app.expose(), HIDDEN)
    }

    /// `auth.test` with the bot token: who the bot is.
    pub async fn auth_test(&self) -> Result<Me, ApiError> {
        let answer = self
            .call("auth.test", Auth::Bot, json!({}), Retry::Safe)
            .await?;
        let text = |key: &str| answer.get(key).and_then(Value::as_str).map(str::to_string);
        let Some(user_id) = text("user_id") else {
            return Err(self.transport("auth.test", "the answer names no bot user"));
        };
        Ok(Me {
            user_id,
            bot_id: text("bot_id"),
            name: text("user").unwrap_or_else(|| "bot".to_string()),
            team: text("team").unwrap_or_default(),
        })
    }

    /// `apps.connections.open` with the app-level token: a fresh Socket
    /// Mode URL.
    pub async fn connections_open(&self) -> Result<String, ApiError> {
        let answer = self
            .call("apps.connections.open", Auth::App, json!({}), Retry::Safe)
            .await?;
        answer
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| self.transport("apps.connections.open", "the answer has no url"))
    }

    /// `chat.postMessage` of mrkdwn `text` to `channel` (in the thread
    /// `thread_ts`), without link and media previews.
    pub async fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<(), ApiError> {
        let mut body = json!({
            "channel": channel,
            "text": text,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts);
        }
        self.call("chat.postMessage", Auth::Bot, body, Retry::Once)
            .await
            .map(drop)
    }

    /// Whether the bot token may go to `url`: https on
    /// [`FILES_HOST`] exactly (default port, no user), or under the test
    /// files URL.
    pub fn allows_file(&self, url: &Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        let slack =
            url.scheme() == "https" && url.port().is_none() && url.host_str() == Some(FILES_HOST);
        slack
            || self.files.as_ref().is_some_and(|base| {
                url.scheme() == base.scheme()
                    && url.host_str() == base.host_str()
                    && url.port_or_known_default() == base.port_or_known_default()
                    && url.path().starts_with(base.path())
            })
    }

    /// Downloads a Slack-hosted file with the bot token, at most `limit`
    /// bytes; `Err(Ok(len))` when it is larger (`len` as far as known),
    /// `Err(Err(why))` when it could not be read. The token never leaves
    /// [`Self::allows_file`]'s hosts: each redirect is checked before it
    /// is followed.
    pub async fn download(
        &self,
        url: &str,
        limit: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, Result<u64, String>> {
        let mut url = Url::parse(url).map_err(|_| Err("the file has no usable URL".to_string()))?;
        let mut response = None;
        for _ in 0..=MAX_REDIRECTS {
            if !self.allows_file(&url) {
                return Err(Err(
                    "it is not on files.slack.com; not downloaded with the bot token".to_string(),
                ));
            }
            let got = self
                .client
                .get(url.clone())
                .header(AUTHORIZATION, bearer(&self.bot))
                .timeout(timeout)
                .send()
                .await
                .map_err(|err| Err(self.clean(&describe(err))))?;
            if !got.status().is_redirection() {
                response = Some(got);
                break;
            }
            let next = got
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| url.join(location).ok())
                .ok_or_else(|| Err("a redirect without a usable Location".to_string()))?;
            url = next;
        }
        let Some(mut response) = response else {
            return Err(Err("too many redirects".to_string()));
        };
        let status = response.status();
        if !status.is_success() {
            return Err(Err(format!("HTTP {status}")));
        }
        // Slack answers a download without a valid token with its HTML
        // sign-in page (200): that is no image.
        if let Some(len) = response.content_length()
            && len > limit
        {
            return Err(Ok(len));
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    bytes.extend_from_slice(&chunk);
                    if bytes.len() as u64 > limit {
                        return Err(Ok(bytes.len() as u64));
                    }
                }
                Ok(None) => return Ok(bytes),
                Err(err) => return Err(Err(self.clean(&describe(err)))),
            }
        }
    }

    fn transport(&self, method: &str, detail: &str) -> ApiError {
        ApiError::Transport {
            method: method.to_string(),
            detail: self.clean(detail),
        }
    }

    /// Calls `method` with `body`, waiting out rate limits and trying
    /// again as `retry` allows; the whole answer when `ok`.
    async fn call(
        &self,
        method: &str,
        auth: Auth,
        body: Value,
        retry: Retry,
    ) -> Result<Value, ApiError> {
        let url = format!("{}/{method}", self.base);
        let token = match auth {
            Auth::Bot => &self.bot,
            Auth::App => &self.app,
        };
        let body = serde_json::to_vec(&body).unwrap_or_default();
        let mut limited = 0;
        let mut tries = 0;
        loop {
            tries += 1;
            let sent = self
                .client
                .post(&url)
                .header(AUTHORIZATION, bearer(token))
                .header(CONTENT_TYPE, "application/json; charset=utf-8")
                .body(body.clone())
                .timeout(CALL_TIMEOUT)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(err) => {
                    let again = tries < MAX_TRIES
                        && (err.is_connect() || (retry == Retry::Safe && err.is_timeout()));
                    let detail = self.clean(&describe(err));
                    if again {
                        (self.log)(&format!("Slack {method}: {detail}; trying again"));
                        tokio::time::sleep(RETRY_PAUSE).await;
                        continue;
                    }
                    return Err(self.transport(method, &detail));
                }
            };
            let status = response.status();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<u64>().ok());
            let text = match read_capped(response, MAX_ANSWER_BYTES).await {
                Ok(text) => text,
                Err(detail) if retry == Retry::Safe && tries < MAX_TRIES => {
                    (self.log)(&format!(
                        "Slack {method}: {}; trying again",
                        self.clean(&detail)
                    ));
                    tokio::time::sleep(RETRY_PAUSE).await;
                    continue;
                }
                Err(detail) => return Err(self.transport(method, &detail)),
            };
            let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let error = answer.get("error").and_then(Value::as_str);
            if status == StatusCode::TOO_MANY_REQUESTS || error == Some("ratelimited") {
                limited += 1;
                let wait = Duration::from_secs(retry_after.unwrap_or(1));
                if limited > MAX_RATE_LIMITED || wait > MAX_WAIT {
                    return Err(ApiError::Refused {
                        method: method.to_string(),
                        error: format!(
                            "ratelimited (still after {limited} tries; asked to wait {}s)",
                            wait.as_secs()
                        ),
                    });
                }
                (self.log)(&format!(
                    "Slack {method}: rate limited, waiting {}s",
                    wait.as_secs()
                ));
                tokio::time::sleep(wait).await;
                continue;
            }
            if status.is_success() && answer.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(answer);
            }
            if status.is_server_error() && retry == Retry::Safe && tries < MAX_TRIES {
                (self.log)(&format!("Slack {method}: {status}; trying again"));
                tokio::time::sleep(RETRY_PAUSE).await;
                continue;
            }
            let error = match error {
                Some(error) => self.clean(error).chars().take(100).collect::<String>(),
                None => format!("HTTP {status}"),
            };
            return Err(ApiError::Refused {
                method: method.to_string(),
                error: crate::prompt_text::escape_hidden(&error),
            });
        }
    }
}

/// `Bearer <token>`, marked sensitive (kept out of reqwest's debug output).
fn bearer(token: &Token) -> HeaderValue {
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer invalid"));
    value.set_sensitive(true);
    value
}

/// The body as text, read chunk by chunk up to `limit` bytes.
async fn read_capped(mut response: reqwest::Response, limit: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > limit {
                    return Err(format!("the answer is larger than {limit} bytes"));
                }
            }
            Ok(None) => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(err) => return Err(describe(err)),
        }
    }
}

/// A transport error without the request URL, and with what caused it.
pub fn describe(err: reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "cannot connect"
    } else if err.is_redirect() {
        "redirected"
    } else {
        "request failed"
    };
    let err = err.without_url();
    let mut text = kind.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::super::config::tests::{app_token, bot_token};
    use super::super::config::{
        ALLOWED_USERS_ENV, API_URL_ENV, APP_TOKEN_ENV, BOT_TOKEN_ENV, FILES_URL_ENV,
    };
    use super::*;

    fn config(base: &str, files: Option<&str>) -> SlackConfig {
        let mut map = vec![
            (BOT_TOKEN_ENV, bot_token()),
            (APP_TOKEN_ENV, app_token()),
            (ALLOWED_USERS_ENV, "U1A".to_string()),
            (API_URL_ENV, base.to_string()),
        ];
        if let Some(files) = files {
            map.push((FILES_URL_ENV, files.to_string()));
        }
        SlackConfig::from_lookup(|key| {
            map.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap()
    }

    fn quiet() -> Log {
        Arc::new(|_: &str| {})
    }

    /// A request as the fake server saw it.
    #[derive(Debug, Clone)]
    struct Seen {
        first: String,
        auth: String,
        body: String,
    }

    /// A server answering consecutive requests with `answers` (status,
    /// extra headers, body); status 0 reads the request and closes.
    async fn serve(answers: Vec<(u16, String, String)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            for (status, headers, body) in answers {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let header = |name: &str| {
                            head.lines().find_map(|l| {
                                let (key, value) = l.split_once(':')?;
                                key.eq_ignore_ascii_case(name)
                                    .then(|| value.trim().to_string())
                            })
                        };
                        let length = header("content-length")
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if rest.len() >= length {
                            log.lock().unwrap().push(Seen {
                                first: head.lines().next().unwrap_or_default().to_string(),
                                auth: header("authorization").unwrap_or_default(),
                                body: rest.to_string(),
                            });
                            break;
                        }
                    }
                }
                if status == 0 {
                    continue;
                }
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{headers}\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (base, seen)
    }

    fn ok(body: &str) -> (u16, String, String) {
        (200, String::new(), body.to_string())
    }

    #[tokio::test]
    async fn calls_carry_the_token_in_the_header_and_errors_never_show_it() {
        let (base, seen) = serve(vec![
            ok(r#"{"ok":true,"user_id":"UBOT","bot_id":"BBOT","user":"junees","team":"Team"}"#),
            ok(r#"{"ok":true,"url":"wss://wss-primary.slack.com/link/?ticket=1"}"#),
            ok(&format!(
                r#"{{"ok":false,"error":"invalid_auth {}"}}"#,
                bot_token()
            )),
            ok(r#"{"ok":false,"error":"invalid_auth"}"#),
        ])
        .await;
        let api = Api::new(&config(&base, None), quiet()).unwrap();
        let me = api.auth_test().await.unwrap();
        assert_eq!(
            me,
            Me {
                user_id: "UBOT".into(),
                bot_id: Some("BBOT".into()),
                name: "junees".into(),
                team: "Team".into(),
            }
        );
        assert_eq!(
            api.connections_open().await.unwrap(),
            "wss://wss-primary.slack.com/link/?ticket=1"
        );
        let err = api.post_message("C1", None, "hi").await.unwrap_err();
        assert!(!err.to_string().contains("fake-slack"), "{err}");
        assert!(err.to_string().contains("[hidden]"), "{err}");
        let err = api.connections_open().await.unwrap_err();
        assert!(err.is_fatal());
        assert!(err.to_string().contains("connections:write"), "{err}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0].first, "POST /auth.test HTTP/1.1");
        assert_eq!(seen[0].auth, format!("Bearer {}", bot_token()));
        assert_eq!(seen[1].first, "POST /apps.connections.open HTTP/1.1");
        assert_eq!(seen[1].auth, format!("Bearer {}", app_token()));
        for request in &seen {
            assert!(!request.first.contains("fake-slack") && !request.body.contains("fake-slack"));
        }

        // Nobody listening: the error names no URL and no token.
        drop(api);
        let api = Api::new(&config("http://127.0.0.1:9", None), quiet()).unwrap();
        let err = api.auth_test().await.unwrap_err().to_string();
        assert!(err.contains("cannot connect"), "{err}");
        assert!(
            !err.contains("fake-slack") && !err.contains("127.0.0.1"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn rate_limits_are_waited_out_and_a_lost_post_is_not_sent_twice() {
        let (base, seen) = serve(vec![
            (
                429,
                "retry-after: 1\r\n".into(),
                r#"{"ok":false,"error":"ratelimited"}"#.into(),
            ),
            ok(r#"{"ok":true,"channel":"C1","ts":"1.2"}"#),
            // A 5xx may come after the message was posted.
            (500, String::new(), "oops".into()),
            // Read, then the connection closes without an answer.
            (0, String::new(), String::new()),
            // Never asked for: nothing is sent twice.
            ok(r#"{"ok":true}"#),
        ])
        .await;
        let api = Api::new(&config(&base, None), quiet()).unwrap();
        let started = std::time::Instant::now();
        api.post_message("C1", Some("1700.1"), "hello")
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
        let err = api.post_message("C1", None, "maybe").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Slack chat.postMessage: HTTP 500 Internal Server Error"
        );
        assert!(api.post_message("C1", None, "lost").await.is_err());
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "{seen:?}");
        let body: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert_eq!(
            body,
            json!({"channel": "C1", "thread_ts": "1700.1", "text": "hello",
                   "unfurl_links": false, "unfurl_media": false})
        );
    }

    #[tokio::test]
    async fn files_come_only_from_slack_with_the_token() {
        let png = "\u{89}PNG".to_string();
        let (files, seen) = serve(vec![
            (200, String::new(), png.clone()),
            // A redirect on the same host is followed with the token...
            (302, "location: /files/b.png\r\n".into(), String::new()),
            (200, String::new(), "x".repeat(2000)),
            // ...one away from it is not.
            (
                302,
                "location: https://evil.example/x\r\n".into(),
                String::new(),
            ),
        ])
        .await;
        let base = format!("{files}/files");
        let api = Api::new(&config("http://127.0.0.1:9", Some(&base)), quiet()).unwrap();
        let timeout = Duration::from_secs(5);
        assert_eq!(
            api.download(&format!("{base}/a.png"), 1000, timeout)
                .await
                .unwrap(),
            png.as_bytes()
        );
        assert_eq!(
            api.download(&format!("{base}/r.png"), 1000, timeout).await,
            Err(Ok(2000))
        );
        let err = api
            .download(&format!("{base}/away.png"), 1000, timeout)
            .await
            .unwrap_err()
            .unwrap_err();
        assert!(err.contains("not on files.slack.com"), "{err}");
        // Elsewhere nothing is asked at all.
        for url in [
            "https://files.slack.com.evil.example/a.png",
            "http://files.slack.com/a.png",
            "https://files.slack.com:8443/a.png",
            "https://user@files.slack.com/a.png",
            "https://example.com/files.slack.com/a.png",
            &format!("{files}/other/a.png"),
        ] {
            assert!(api.download(url, 1000, timeout).await.is_err(), "{url}");
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(
            seen.iter()
                .all(|s| s.auth == format!("Bearer {}", bot_token()))
        );
        assert_eq!(seen[2].first, "GET /files/b.png HTTP/1.1");
        assert!(
            api.allows_file(&Url::parse("https://files.slack.com/files-pri/T1-F1/a.png").unwrap())
        );
    }
}
