//! The Telegram Bot API, as far as the gateway needs it (Bot API docs,
//! https://core.telegram.org/bots/api):
//!
//! - "Making requests": every call is `POST <base>/bot<token>/METHOD_NAME`
//!   with a JSON body; the answer is `{ok, result}` or `{ok: false,
//!   description, error_code, parameters}`, where `parameters.retry_after`
//!   (ResponseParameters) says how long flood control wants us to wait.
//! - `getMe` checks the token and names the bot; `getUpdates` is the long
//!   poll (`offset`, `timeout`, `allowed_updates`); `sendMessage` posts
//!   (`text` of 1-4096 characters, `message_thread_id` for forum topics,
//!   `link_preview_options`); `sendChatAction` shows "typing" ("for 5
//!   seconds or less"); `getFile` gives a `file_path` that downloads from
//!   `<base>/file/bot<token>/<file_path>` (up to 20 MB for bots).
//!
//! The token sits in the URL path of every request, so no error, log line
//! or answer ever holds a request URL: transport errors are described
//! from their kind and causes ([`describe`]) and anything the server says
//! is cleaned of the token ([`Api::clean`]). Redirects are not followed.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::redirect::Policy;
use serde_json::{Value, json};

use super::config::{TelegramConfig, Token};
use crate::gateway::Log;

/// 429 answers waited out before a request gives up.
const MAX_RATE_LIMITED: usize = 5;
/// Tries of a request that could not connect (so was never sent).
const MAX_TRIES: usize = 3;
/// The pause before trying again after a connect error or a 5xx.
const RETRY_PAUSE: Duration = Duration::from_millis(500);
/// The longest flood-control wait taken before the request fails instead.
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
/// Time an ordinary call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest answer read: a full `getUpdates` batch (100 updates) fits.
const MAX_ANSWER_BYTES: usize = 16 * 1024 * 1024;
/// What replaces the token in text from the server.
const HIDDEN: &str = "[hidden]";

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The Bot API answered with an error.
    Refused {
        method: String,
        /// `error_code`, else the HTTP status.
        code: u16,
        /// `description`, cleaned of the token and shortened.
        description: String,
    },
    /// No (usable) answer: connect error, time-out, broken body.
    Transport { method: String, detail: String },
}

impl ApiError {
    /// The token is wrong (401 Unauthorized; 404 Not Found when the path
    /// names no bot), or another program takes this bot's updates (409
    /// Conflict: another `getUpdates` or a webhook). Polling again would
    /// not help.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ApiError::Refused {
                code: 401 | 404 | 409,
                ..
            }
        )
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Refused {
                method,
                code,
                description,
            } => {
                write!(f, "Telegram {method}: error {code}")?;
                if !description.is_empty() {
                    write!(f, " ({description})")?;
                }
                if *code == 409 {
                    f.write_str(
                        "; another program takes this bot's updates (a second gateway, or a \
                         webhook set with setWebhook: remove it with deleteWebhook)",
                    )?;
                } else if matches!(code, 401 | 404) {
                    f.write_str("; check the bot token (telegram_token)")?;
                }
                Ok(())
            }
            ApiError::Transport { method, detail } => write!(f, "Telegram {method}: {detail}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// The bot itself, from `getMe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
    pub id: i64,
    /// Without the `@`.
    pub username: String,
}

/// A file ready to download, from `getFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: String,
    pub size: Option<u64>,
}

/// Where a message goes: a chat, and in a forum supergroup its topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub chat: i64,
    pub thread: Option<i64>,
}

impl Target {
    /// The gateway's channel key: `<chat>` or `<chat>:<topic>`.
    pub fn key(&self) -> String {
        match self.thread {
            Some(thread) => format!("{}:{thread}", self.chat),
            None => self.chat.to_string(),
        }
    }

    /// The target a channel key names.
    pub fn parse(key: &str) -> Option<Self> {
        let (chat, thread) = match key.split_once(':') {
            Some((chat, thread)) => (chat, Some(thread.parse().ok()?)),
            None => (key, None),
        };
        Some(Self {
            chat: chat.parse().ok()?,
            thread,
        })
    }

    fn fields(&self, body: &mut Value) {
        body["chat_id"] = json!(self.chat);
        if let Some(thread) = self.thread {
            body["message_thread_id"] = json!(thread);
        }
    }
}

/// How a call may be tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// Asking again changes nothing: connect errors, 5xx answers and time
    /// limits are tried again.
    Safe,
    /// Sending again could post twice (`sendMessage` has no idempotency
    /// key): only a connect error (nothing was sent) and flood control
    /// (refused before it ran) are tried again.
    Once,
}

/// A client of the Bot API.
pub struct Api {
    client: reqwest::Client,
    base: String,
    token: Token,
    log: Log,
}

impl Api {
    pub fn new(config: &TelegramConfig, log: Log) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(Policy::none())
            .build()
            .map_err(|err| anyhow::anyhow!("cannot set up the HTTP client: {}", describe(err)))?;
        Ok(Self {
            client,
            base: config.api_url.clone(),
            token: config.token.clone(),
            log,
        })
    }

    /// `text` without the token (or its secret part after the colon).
    pub fn clean(&self, text: &str) -> String {
        let token = self.token.expose();
        let mut text = text.replace(token, HIDDEN);
        if let Some((_, secret)) = token.split_once(':')
            && secret.len() >= 8
        {
            text = text.replace(secret, HIDDEN);
        }
        text
    }

    /// `getMe`: the bot's id and username; fails on a wrong token.
    pub async fn get_me(&self) -> Result<Me, ApiError> {
        let result = self
            .call("getMe", json!({}), CALL_TIMEOUT, Retry::Safe)
            .await?;
        let id = result.get("id").and_then(Value::as_i64);
        let username = result.get("username").and_then(Value::as_str);
        match (id, username) {
            (Some(id), Some(username)) => Ok(Me {
                id,
                username: username.to_string(),
            }),
            _ => Err(self.transport("getMe", "the answer names no bot")),
        }
    }

    /// `getUpdates` from `offset` (confirming every update below it),
    /// waiting up to `wait` for one (long polling). Only messages are
    /// asked for (`allowed_updates`).
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        wait: Duration,
        limit: u32,
    ) -> Result<Vec<Value>, ApiError> {
        let mut body = json!({
            "timeout": wait.as_secs(),
            "limit": limit,
            "allowed_updates": ["message"],
        });
        if let Some(offset) = offset {
            body["offset"] = json!(offset);
        }
        // The server holds the request up to `timeout` seconds.
        let result = self
            .call("getUpdates", body, wait + CALL_TIMEOUT, Retry::Safe)
            .await?;
        match result {
            Value::Array(updates) => Ok(updates),
            _ => Err(self.transport("getUpdates", "the answer is not a list of updates")),
        }
    }

    /// `sendMessage` of plain `text` (no `parse_mode`: the model's
    /// Markdown shows as written, and nothing in it can fail to parse),
    /// without link previews.
    pub async fn send_message(&self, target: Target, text: &str) -> Result<(), ApiError> {
        let mut body = json!({
            "text": text,
            "link_preview_options": {"is_disabled": true},
        });
        target.fields(&mut body);
        self.call("sendMessage", body, CALL_TIMEOUT, Retry::Once)
            .await
            .map(drop)
    }

    /// `sendChatAction` `typing`.
    pub async fn typing(&self, target: Target) -> Result<(), ApiError> {
        let mut body = json!({"action": "typing"});
        target.fields(&mut body);
        self.call("sendChatAction", body, CALL_TIMEOUT, Retry::Safe)
            .await
            .map(drop)
    }

    /// `getFile`: where the file can be downloaded.
    pub async fn get_file(&self, file_id: &str) -> Result<FileInfo, ApiError> {
        let result = self
            .call(
                "getFile",
                json!({"file_id": file_id}),
                CALL_TIMEOUT,
                Retry::Safe,
            )
            .await?;
        let path = result
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| self.transport("getFile", "the answer has no file_path"))?;
        Ok(FileInfo {
            path: path.to_string(),
            size: result.get("file_size").and_then(Value::as_u64),
        })
    }

    /// Downloads `file_path` from the Bot API's file URL, at most `limit`
    /// bytes; `Err(Ok(len))` when it is larger (`len` as far as known),
    /// `Err(Err(why))` when it could not be read.
    pub async fn download(
        &self,
        file_path: &str,
        limit: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, Result<u64, String>> {
        if !safe_file_path(file_path) {
            return Err(Err("the server gave an unusable file path".to_string()));
        }
        let url = format!("{}/file/bot{}/{file_path}", self.base, self.token.expose());
        let mut response = self
            .client
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| Err(self.clean(&describe(err))))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Err(format!("HTTP {status}")));
        }
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

    /// Calls `method` with `body`, waiting out flood control and trying
    /// again as `retry` allows; the `result` of a successful answer.
    async fn call(
        &self,
        method: &str,
        body: Value,
        timeout: Duration,
        retry: Retry,
    ) -> Result<Value, ApiError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token.expose());
        let mut limited = 0;
        let mut tries = 0;
        loop {
            tries += 1;
            let sent = self
                .client
                .post(&url)
                .json(&body)
                .timeout(timeout)
                .send()
                .await;
            let response = match sent {
                Ok(response) => response,
                Err(err) => {
                    let again = tries < MAX_TRIES
                        && (err.is_connect() || (retry == Retry::Safe && err.is_timeout()));
                    let detail = self.clean(&describe(err));
                    if again {
                        (self.log)(&format!("Telegram {method}: {detail}; trying again"));
                        tokio::time::sleep(RETRY_PAUSE).await;
                        continue;
                    }
                    return Err(self.transport(method, &detail));
                }
            };
            let status = response.status();
            let text = match read_capped(response, MAX_ANSWER_BYTES).await {
                Ok(text) => text,
                Err(detail) if retry == Retry::Safe && tries < MAX_TRIES => {
                    (self.log)(&format!(
                        "Telegram {method}: {}; trying again",
                        self.clean(&detail)
                    ));
                    tokio::time::sleep(RETRY_PAUSE).await;
                    continue;
                }
                Err(detail) => return Err(self.transport(method, &detail)),
            };
            let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if answer.get("ok").and_then(Value::as_bool) == Some(true) && status.is_success() {
                return Ok(answer.get("result").cloned().unwrap_or(Value::Null));
            }
            let code = answer
                .get("error_code")
                .and_then(Value::as_u64)
                .and_then(|code| u16::try_from(code).ok())
                .unwrap_or(status.as_u16());
            if code == StatusCode::TOO_MANY_REQUESTS.as_u16() {
                limited += 1;
                let wait = answer
                    .pointer("/parameters/retry_after")
                    .and_then(Value::as_u64)
                    .map_or(Duration::from_secs(1), Duration::from_secs);
                if limited > MAX_RATE_LIMITED || wait > MAX_WAIT {
                    return Err(ApiError::Refused {
                        method: method.to_string(),
                        code,
                        description: format!(
                            "still rate limited after {limited} tries (asked to wait {}s)",
                            wait.as_secs()
                        ),
                    });
                }
                (self.log)(&format!(
                    "Telegram {method}: rate limited, waiting {}s",
                    wait.as_secs()
                ));
                tokio::time::sleep(wait).await;
                continue;
            }
            if status.is_server_error() && retry == Retry::Safe && tries < MAX_TRIES {
                (self.log)(&format!("Telegram {method}: {status}; trying again"));
                tokio::time::sleep(RETRY_PAUSE).await;
                continue;
            }
            let description = answer
                .get("description")
                .and_then(Value::as_str)
                .map(|text| self.clean(text).chars().take(200).collect::<String>())
                .unwrap_or_else(|| {
                    status
                        .canonical_reason()
                        .unwrap_or("no description")
                        .to_string()
                });
            return Err(ApiError::Refused {
                method: method.to_string(),
                code,
                description: crate::prompt_text::escape_hidden(&description),
            });
        }
    }
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

/// Whether a `file_path` from `getFile` can go into the download URL as
/// it is: relative, without `..`, of plain characters only (so it cannot
/// leave `/file/bot<token>/` or add a query).
fn safe_file_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && path.split('/').all(|part| !part.is_empty() && part != "..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
}

/// A transport error without the request URL (which holds the token), and
/// with what caused it.
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

    use super::*;

    const TOKEN: &str = "4242:secret-token-for-tests";

    fn config(base: &str) -> TelegramConfig {
        let map = [
            (super::super::config::TOKEN_ENV, TOKEN.to_string()),
            (super::super::config::ALLOWED_USERS_ENV, "1".to_string()),
            (super::super::config::API_URL_ENV, base.to_string()),
        ];
        TelegramConfig::from_lookup(|key| {
            map.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap()
    }

    fn quiet() -> Log {
        Arc::new(|_: &str| {})
    }

    /// A server answering consecutive requests with `answers` (status,
    /// body), recording each request's first line and body.
    async fn serve(answers: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            for (status, body) in answers {
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
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().to_string())
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if rest.len() >= length {
                            let first = head.lines().next().unwrap_or_default().to_string();
                            log.lock().unwrap().push((first, rest.to_string()));
                            break;
                        }
                    }
                }
                if status == 0 {
                    continue;
                }
                // `NOLEN:` leaves the length out: the body ends when the
                // connection closes.
                let (length, body) = match body.strip_prefix("NOLEN:") {
                    Some(body) => (String::new(), body.to_string()),
                    None => (format!("content-length: {}\r\n", body.len()), body),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{length}connection: close\r\n\r\n{body}"
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (base, seen)
    }

    #[test]
    fn targets_round_trip() {
        for target in [
            Target {
                chat: 5,
                thread: None,
            },
            Target {
                chat: -1001234,
                thread: Some(7),
            },
        ] {
            assert_eq!(Target::parse(&target.key()), Some(target));
        }
        assert_eq!(Target::parse("x"), None);
        assert_eq!(Target::parse("5:x"), None);
    }

    #[test]
    fn file_paths_stay_under_the_file_url() {
        for good in ["photos/file_1.jpg", "documents/a-b.png"] {
            assert!(safe_file_path(good), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../bot1:x/getMe",
            "photos/../../x",
            "photos//x",
            "a?x=1",
            "a#b",
            "a b",
            "a%2e%2e",
        ] {
            assert!(!safe_file_path(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn answers_and_errors_never_show_the_token() {
        let (base, seen) = serve(vec![
            (200, r#"{"ok":true,"result":{"id":4242,"is_bot":true,"username":"junees_bot"}}"#.into()),
            (
                401,
                format!(r#"{{"ok":false,"error_code":401,"description":"Unauthorized: {TOKEN}"}}"#),
            ),
            (
                409,
                r#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}"#.into(),
            ),
        ])
        .await;
        let api = Api::new(&config(&base), quiet()).unwrap();
        let me = api.get_me().await.unwrap();
        assert_eq!(
            me,
            Me {
                id: 4242,
                username: "junees_bot".into()
            }
        );
        let err = api
            .get_updates(Some(3), Duration::ZERO, 100)
            .await
            .unwrap_err();
        assert!(err.is_fatal());
        let text = err.to_string();
        assert!(!text.contains("secret-token"), "{text}");
        assert!(text.contains("Unauthorized: [hidden]"), "{text}");
        let err = api.get_updates(None, Duration::ZERO, 1).await.unwrap_err();
        assert!(
            err.is_fatal() && err.to_string().contains("deleteWebhook"),
            "{err}"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0].0, format!("POST /bot{TOKEN}/getMe HTTP/1.1"));
        let body: Value = serde_json::from_str(&seen[1].1).unwrap();
        assert_eq!(body["offset"], 3);
        assert_eq!(body["allowed_updates"], json!(["message"]));

        // Nobody listening: the error names no URL.
        drop(api);
        let api = Api::new(&config("http://127.0.0.1:9"), quiet()).unwrap();
        let err = api.get_me().await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("cannot connect"), "{text}");
        assert!(
            !text.contains("secret-token") && !text.contains("127.0.0.1"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn flood_control_is_waited_out_and_a_lost_post_is_not_sent_twice() {
        let (base, seen) = serve(vec![
            (
                429,
                r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 1","parameters":{"retry_after":1}}"#.into(),
            ),
            (200, r#"{"ok":true,"result":{"message_id":1}}"#.into()),
            // A 5xx may come after the message was posted.
            (
                502,
                r#"{"ok":false,"error_code":502,"description":"Bad Gateway"}"#.into(),
            ),
            // Read, then the connection closes without an answer.
            (0, String::new()),
            // Never asked for: nothing is sent twice.
            (200, r#"{"ok":true,"result":{"message_id":2}}"#.into()),
        ])
        .await;
        let api = Api::new(&config(&base), quiet()).unwrap();
        let target = Target {
            chat: 5,
            thread: Some(9),
        };
        let started = std::time::Instant::now();
        api.send_message(target, "hello").await.unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
        let err = api.send_message(target, "maybe").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Telegram sendMessage: error 502 (Bad Gateway)"
        );
        assert!(api.send_message(target, "lost").await.is_err());
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4, "{seen:?}");
        let body: Value = serde_json::from_str(&seen[1].1).unwrap();
        assert_eq!(
            body,
            json!({"chat_id": 5, "message_thread_id": 9, "text": "hello",
                   "link_preview_options": {"is_disabled": true}})
        );
    }

    #[tokio::test]
    async fn downloads_stop_at_the_limit() {
        let big = "x".repeat(2000);
        let (base, seen) = serve(vec![
            (200, "small".into()),
            (200, big.clone()),
            (200, format!("NOLEN:{big}")),
            (302, String::new()),
        ])
        .await;
        let api = Api::new(&config(&base), quiet()).unwrap();
        let timeout = Duration::from_secs(5);
        assert_eq!(
            api.download("photos/a.jpg", 1000, timeout).await.unwrap(),
            b"small"
        );
        assert_eq!(
            api.download("photos/b.jpg", 1000, timeout).await,
            Err(Ok(2000))
        );
        // Without a length the reading stops past the limit too.
        let no_length = api.download("photos/u.jpg", 1000, timeout).await;
        assert!(
            matches!(no_length, Err(Ok(read)) if read > 1000),
            "{no_length:?}"
        );
        let err = api.download("photos/c.jpg", 1000, timeout).await;
        assert_eq!(err, Err(Err("HTTP 302 Found".to_string())));
        assert_eq!(
            api.download("../x", 1000, timeout).await,
            Err(Err("the server gave an unusable file path".to_string()))
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        assert_eq!(
            seen[0].0,
            format!("GET /file/bot{TOKEN}/photos/a.jpg HTTP/1.1")
        );
    }
}
