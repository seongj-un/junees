//! Notifications about task runs, posted to a Discord webhook.
//!
//! With `JUNEES_NOTIFY_WEBHOOK` set, `task run` posts one message when it
//! stops, and with `JUNEES_NOTIFY_EACH_SESSION=1` a short line after every
//! session too. A failed post is only a warning. The webhook URL carries the
//! webhook's token, so messages about it name only its host.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use serde_json::json;

use crate::config::parse_flag;
use crate::task::state::{State, Stop};

pub const WEBHOOK_ENV: &str = "JUNEES_NOTIFY_WEBHOOK";
pub const EACH_SESSION_ENV: &str = "JUNEES_NOTIFY_EACH_SESSION";

/// Discord's limit on the length of a message.
pub const CONTENT_LIMIT: usize = 2000;

/// How long one post may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Where notifications go; [`Notifier::default`] sends none.
#[derive(Default)]
pub struct Notifier {
    webhook: Option<Webhook>,
    each_session: bool,
}

struct Webhook {
    /// Never shown: it holds the token.
    url: Url,
    /// What messages show instead of the URL.
    host: String,
    client: reqwest::Client,
    timeout: Duration,
}

/// A post that did not go through. Shows the webhook's host, never its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyError {
    host: String,
    problem: String,
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "notification to {} failed: {}", self.host, self.problem)
    }
}

impl std::error::Error for NotifyError {}

/// Shows the host only.
impl fmt::Debug for Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notifier")
            .field("host", &self.host())
            .field("each_session", &self.each_session)
            .finish()
    }
}

impl Notifier {
    /// Reads [`WEBHOOK_ENV`] and [`EACH_SESSION_ENV`].
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Like [`Notifier::from_env`], with the variables from `lookup`. An
    /// empty webhook variable counts as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let each_session = parse_flag(lookup(EACH_SESSION_ENV), EACH_SESSION_ENV)?;
        let Some(raw) = lookup(WEBHOOK_ENV).filter(|raw| !raw.trim().is_empty()) else {
            return Ok(Self::default());
        };
        // The URL is left out of every message here.
        let url = Url::parse(raw.trim())
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .with_context(|| format!("{WEBHOOK_ENV} is not an http(s) URL"))?;
        let Some(host) = url.host_str().map(str::to_string) else {
            bail!("{WEBHOOK_ENV} has no host");
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("cannot set up the notification client")?;
        Ok(Self {
            webhook: Some(Webhook {
                url,
                host,
                client,
                timeout: TIMEOUT,
            }),
            each_session,
        })
    }

    /// The host notifications go to, if they are on.
    pub fn host(&self) -> Option<&str> {
        self.webhook.as_ref().map(|webhook| webhook.host.as_str())
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        if let Some(webhook) = &mut self.webhook {
            webhook.timeout = timeout;
        }
        self
    }

    /// Posts `content`, cut to [`CONTENT_LIMIT`], with every mention
    /// turned off. Does nothing without a webhook.
    pub async fn send(&self, content: &str) -> Result<(), NotifyError> {
        let Some(webhook) = &self.webhook else {
            return Ok(());
        };
        let fail = |problem: String| NotifyError {
            host: webhook.host.clone(),
            problem,
        };
        let body = json!({
            "content": cut(content, CONTENT_LIMIT),
            "allowed_mentions": {"parse": []},
        });
        let response = webhook
            .client
            .post(webhook.url.clone())
            .timeout(webhook.timeout)
            .json(&body)
            .send()
            .await
            .map_err(|err| fail(describe(err, webhook.timeout)))?;
        let status = response.status();
        if !status.is_success() {
            return Err(fail(format!("HTTP {status}")));
        }
        Ok(())
    }

    /// Posts why the task run in `root` stopped.
    pub async fn task_stopped(
        &self,
        root: &Path,
        stop: &Stop,
        passing: usize,
        total: usize,
        state: &State,
    ) -> Result<(), NotifyError> {
        if self.webhook.is_none() {
            return Ok(());
        }
        let reason = serde_json::to_value(stop.reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        let usage = state.usage;
        let content = format!(
            "junees: task in {} stopped ({reason}): {stop}\n\
             features: {passing}/{total} passing\n\
             sessions: {}\n\
             tokens: input {}, output {}, cache write {}, cache read {}",
            repo_name(root),
            state.sessions,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
        self.send(&content).await
    }

    /// Posts why the task run in `root` failed before it could record a
    /// stop.
    pub async fn run_failed(&self, root: &Path, err: &anyhow::Error) -> Result<(), NotifyError> {
        if self.webhook.is_none() {
            return Ok(());
        }
        let content = format!("junees: task in {} run failed: {err:#}", repo_name(root));
        self.send(&content).await
    }

    /// Posts one line about a finished session, if notifications for every
    /// session are on.
    pub async fn session_done(
        &self,
        root: &Path,
        session: u64,
        summary: &str,
    ) -> Result<(), NotifyError> {
        if !self.each_session {
            return Ok(());
        }
        let content = format!(
            "junees: task in {} session {session} done: {summary}",
            repo_name(root)
        );
        self.send(&content).await
    }
}

/// The name of the repository folder.
fn repo_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Cuts `text` to at most `limit` UTF-16 units, the strictest way a
/// message length is counted, ending a cut text in `...`.
fn cut(text: &str, limit: usize) -> String {
    let units = |s: &str| s.chars().map(char::len_utf16).sum::<usize>();
    if units(text) <= limit {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 3;
    for c in text.chars() {
        used += c.len_utf16();
        if used > limit {
            break;
        }
        out.push(c);
    }
    out.push_str("...");
    out
}

/// What went wrong with a request, without its URL.
fn describe(err: reqwest::Error, timeout: Duration) -> String {
    if err.is_timeout() {
        return format!("no answer within {} s", timeout.as_secs_f64());
    }
    let err = err.without_url();
    let mut text = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::message::Usage;
    use crate::task::state::StopReason;

    const TOKEN: &str = "s3cret-token";

    fn notifier(pairs: &[(&str, &str)]) -> Result<Notifier> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Notifier::from_lookup(|name| map.get(name).cloned())
    }

    /// A webhook server that answers each connection with the next status
    /// (0: closes without answering, `None`: never answers) and returns the
    /// raw requests.
    async fn server(statuses: Vec<Option<u16>>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/webhooks/42/{TOKEN}",
            listener.local_addr().unwrap()
        );
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut held = Vec::new();
            for status in statuses {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                match status {
                    None => held.push(socket),
                    Some(0) => drop(socket),
                    Some(status) => {
                        let reply = format!(
                            "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        );
                        socket.write_all(reply.as_bytes()).await.unwrap();
                        socket.shutdown().await.ok();
                    }
                }
            }
            // Keeps unanswered connections open until the client gives up.
            if !held.is_empty() {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            requests
        });
        (url, handle)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                if buf.len() >= end + 4 + length {
                    break;
                }
            }
        }
        String::from_utf8(buf).unwrap()
    }

    fn body(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    fn stopped_state() -> State {
        State {
            sessions: 4,
            usage: Usage {
                input_tokens: 1000,
                output_tokens: 200,
                cache_creation_input_tokens: 30,
                cache_read_input_tokens: 400,
            },
            ..State::default()
        }
    }

    #[test]
    fn reads_the_settings() {
        let off = notifier(&[]).unwrap();
        assert_eq!(off.host(), None);
        assert!(!off.each_session);
        assert_eq!(notifier(&[(WEBHOOK_ENV, "  ")]).unwrap().host(), None);

        let on = notifier(&[
            (WEBHOOK_ENV, "https://discord.com/api/webhooks/1/abc"),
            (EACH_SESSION_ENV, "1"),
        ])
        .unwrap();
        assert_eq!(on.host(), Some("discord.com"));
        assert!(on.each_session);
        assert!(!format!("{on:?}").contains("abc"), "{on:?}");

        for bad in [
            "discord.com/api/webhooks/1/abc-token",
            "ftp://discord.com/abc-token",
            "https://",
        ] {
            let err = notifier(&[(WEBHOOK_ENV, bad)]).unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(WEBHOOK_ENV), "{text}");
            assert!(!text.contains("abc-token"), "{text}");
        }
        assert!(notifier(&[(EACH_SESSION_ENV, "maybe")]).is_err());
    }

    #[tokio::test]
    async fn posts_why_the_run_stopped_without_mentions() {
        let (url, server) = server(vec![Some(204)]).await;
        let notifier = notifier(&[(WEBHOOK_ENV, &url)]).unwrap();
        notifier
            .task_stopped(
                Path::new("/work/todo-app"),
                &Stop {
                    reason: StopReason::Stalled,
                    session: 4,
                    detail: None,
                },
                2,
                5,
                &stopped_state(),
            )
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with(&format!("POST /api/webhooks/42/{TOKEN} HTTP/1.1\r\n")),
            "{}",
            requests[0]
        );
        assert!(
            requests[0]
                .to_lowercase()
                .contains("content-type: application/json")
        );
        let body = body(&requests[0]);
        assert_eq!(body["allowed_mentions"], json!({"parse": []}));
        assert_eq!(
            body["content"],
            "junees: task in todo-app stopped (stalled): stalled: no new feature \
             passed in 3 sessions in a row\n\
             features: 2/5 passing\n\
             sessions: 4\n\
             tokens: input 1000, output 200, cache write 30, cache read 400"
        );
    }

    #[tokio::test]
    async fn cuts_long_messages_to_the_discord_limit() {
        let (url, server) = server(vec![Some(200), Some(200)]).await;
        let notifier = notifier(&[(WEBHOOK_ENV, &url)]).unwrap();
        let detail = format!("@everyone {}", "가".repeat(3000));
        notifier
            .task_stopped(
                Path::new("/work/app"),
                &Stop {
                    reason: StopReason::Error,
                    session: 1,
                    detail: Some(detail),
                },
                0,
                3,
                &stopped_state(),
            )
            .await
            .unwrap();
        // Characters outside the BMP count twice.
        notifier.send(&"😀".repeat(1500)).await.unwrap();

        let requests = server.await.unwrap();
        let content = body(&requests[0])["content"].as_str().unwrap().to_string();
        assert_eq!(content.chars().count(), CONTENT_LIMIT);
        assert!(content.ends_with("가가..."), "{content}");
        assert!(content.contains("@everyone"));
        assert_eq!(body(&requests[0])["allowed_mentions"]["parse"], json!([]));
        let emoji = body(&requests[1])["content"].as_str().unwrap().to_string();
        assert_eq!(emoji, format!("{}...", "😀".repeat(998)));
    }

    #[test]
    fn cut_keeps_short_text() {
        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("0123456789", 10), "0123456789");
        assert_eq!(cut("0123456789a", 10), "0123456...");
    }

    #[tokio::test]
    async fn sends_session_lines_only_when_asked() {
        let (url, server) = server(vec![Some(204)]).await;
        let quiet = notifier(&[(WEBHOOK_ENV, &url)]).unwrap();
        quiet
            .session_done(Path::new("/work/app"), 1, "1/3 passing, new: F1")
            .await
            .unwrap();
        let chatty = notifier(&[(WEBHOOK_ENV, &url), (EACH_SESSION_ENV, "1")]).unwrap();
        chatty
            .session_done(Path::new("/work/app"), 2, "2/3 passing, new: F2")
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            body(&requests[0])["content"],
            "junees: task in app session 2 done: 2/3 passing, new: F2"
        );
    }

    #[tokio::test]
    async fn failures_name_the_host_but_never_the_url() {
        let (url, server) = server(vec![Some(500), Some(0), None]).await;
        let notifier = notifier(&[(WEBHOOK_ENV, &url)])
            .unwrap()
            .with_timeout(Duration::from_millis(300));
        let mut errors = Vec::new();
        for _ in 0..3 {
            errors.push(notifier.send("hello").await.unwrap_err().to_string());
        }
        assert_eq!(
            errors[0],
            "notification to 127.0.0.1 failed: HTTP 500 Internal Server Error"
        );
        assert!(
            errors[1].starts_with("notification to 127.0.0.1 failed: "),
            "{}",
            errors[1]
        );
        assert_eq!(
            errors[2],
            "notification to 127.0.0.1 failed: no answer within 0.3 s"
        );
        for error in &errors {
            assert!(!error.contains(TOKEN), "{error}");
            assert!(!error.contains("/api/webhooks"), "{error}");
        }
        server.await.unwrap();

        // Nothing listens on the port any more.
        let refused = notifier.send("hello").await.unwrap_err().to_string();
        assert!(refused.starts_with("notification to 127.0.0.1 failed: "));
        assert!(!refused.contains(TOKEN), "{refused}");
    }

    #[tokio::test]
    async fn without_a_webhook_nothing_is_sent() {
        let notifier = Notifier::default();
        notifier.send("hello").await.unwrap();
        notifier
            .task_stopped(
                Path::new("/work/app"),
                &Stop {
                    reason: StopReason::Complete,
                    session: 1,
                    detail: None,
                },
                1,
                1,
                &State::default(),
            )
            .await
            .unwrap();
        notifier
            .run_failed(Path::new("/work/app"), &anyhow::anyhow!("boom"))
            .await
            .unwrap();
    }
}
