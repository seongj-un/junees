//! The Socket Mode connection ("Using Socket Mode",
//! https://docs.slack.dev/apis/events-api/using-socket-mode):
//!
//! 1. `apps.connections.open` with the app-level token gives a WebSocket
//!    URL ("not static ... it refreshes regularly"), opened right away.
//! 2. Slack sends `hello` first; then envelopes `{envelope_id, type,
//!    payload, ...}`. "Your app still needs to acknowledge receiving
//!    each event so that Slack knows whether to retry": every envelope is
//!    answered with `{"envelope_id": ...}` at once, before it is handled.
//! 3. "Expect disconnects": a `disconnect` message (`warning`,
//!    `refresh_requested`, `link_disabled`), a close or a dead socket
//!    end the connection, and a new URL is asked for. Pings keep the
//!    socket alive; a socket silent for too long is dropped too.
//!
//! An event Slack did not see acknowledged in time comes again
//! (`retry_attempt`); the router drops repeats by `event_id`.
//!
//! Button presses come the same way ("Using interactive features"): an
//! envelope of type `interactive` whose payload is a `block_actions`
//! payload. It is acknowledged with its `envelope_id` alone, at once like
//! any other, which is the acknowledgment response Slack wants "within 3
//! seconds" ("Handling user interaction"); the router answers the press
//! afterwards through the Web API.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

use super::api::{Api, ApiError};
use crate::cancel::Cancel;
use crate::gateway::Log;

/// How the connection keeps itself alive and tries again.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How often a ping is sent.
    pub ping_every: Duration,
    /// A connection that sends nothing (not even a pong) for this long is
    /// dropped and opened again.
    pub idle: Duration,
    /// How long connecting and the `hello` may take.
    pub connect_timeout: Duration,
    /// The first pause after a failed connection; it doubles up to
    /// `backoff_max`.
    pub backoff_start: Duration,
    pub backoff_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(30),
            idle: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(30),
            backoff_start: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }
}

/// How one connection ended.
enum End {
    /// Slack asked for a new connection, or the socket ended: open the
    /// next one now (`fast`) or after a pause.
    Again {
        fast: bool,
    },
    Shutdown,
}

/// Keeps a Socket Mode connection open and hands the payload of every
/// event and interaction to `events`.
pub struct Socket {
    api: Arc<Api>,
    timing: Timing,
    log: Log,
    /// Whether the WebSocket URL may be on this machine (a test Web API).
    local: bool,
}

impl Socket {
    pub fn new(api: Arc<Api>, timing: Timing, log: Log, local: bool) -> Self {
        Self {
            api,
            timing,
            log,
            local,
        }
    }

    /// Connects, and connects again, until `shutdown` (`Ok`) or Slack
    /// refuses the app-level token (an error).
    pub async fn run(self, events: UnboundedSender<Value>, shutdown: Cancel) -> Result<()> {
        let mut backoff = self.timing.backoff_start;
        loop {
            let opened = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                opened = self.api.connections_open() => opened,
            };
            let end = match opened {
                Err(err) if err.is_fatal() => return Err(err.into()),
                Err(err) => {
                    (self.log)(&format!("{err}"));
                    End::Again { fast: false }
                }
                Ok(url) => match self.check(&url) {
                    Err(err) => return Err(err),
                    Ok(url) => {
                        self.connect_once(url, &events, &shutdown, &mut backoff)
                            .await
                    }
                },
            };
            match end {
                End::Shutdown => return Ok(()),
                End::Again { fast: true } => {}
                End::Again { fast: false } => {
                    (self.log)(&format!(
                        "slack: connecting again in {:.1}s",
                        backoff.as_secs_f64()
                    ));
                    tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                }
            }
        }
    }

    /// The URL `apps.connections.open` gave, if it is Slack's: `wss` on a
    /// `slack.com` host (or on this machine, with a test Web API).
    fn check(&self, url: &str) -> Result<Url> {
        let parsed = Url::parse(url).map_err(|_| anyhow!("Slack gave no usable WebSocket URL"))?;
        let host = parsed.host_str().unwrap_or_default();
        let slack = parsed.scheme() == "wss"
            && (host == "slack.com" || host.ends_with(".slack.com"))
            && parsed.username().is_empty();
        let ip = host.trim_start_matches('[').trim_end_matches(']');
        let local = self.local
            && matches!(parsed.scheme(), "ws" | "wss")
            && (host == "localhost"
                || ip
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()));
        if slack || local {
            Ok(parsed)
        } else {
            Err(anyhow!(
                "Slack gave a WebSocket URL on `{host}`, not on slack.com: not connecting"
            ))
        }
    }

    /// One connection, from connecting to its end.
    async fn connect_once(
        &self,
        url: Url,
        events: &UnboundedSender<Value>,
        shutdown: &Cancel,
        backoff: &mut Duration,
    ) -> End {
        let connect = tokio::time::timeout(
            self.timing.connect_timeout,
            tokio_tungstenite::connect_async(url.as_str()),
        );
        let mut socket = tokio::select! {
            _ = shutdown.cancelled() => return End::Shutdown,
            result = connect => match result {
                Ok(Ok((socket, _))) => socket,
                // The error text never holds the URL (its ticket).
                Ok(Err(err)) => {
                    (self.log)(&format!("slack: cannot connect: {}", short(&err.to_string())));
                    return End::Again { fast: false };
                }
                Err(_) => {
                    (self.log)("slack: connecting timed out");
                    return End::Again { fast: false };
                }
            },
        };
        let mut ping = tokio::time::interval(self.timing.ping_every);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await;
        let mut said_hello = false;
        // When the last frame came. Only received frames move it: the
        // pings sent in between must not restart the silence timer, or a
        // dead connection whose writes still succeed is never noticed.
        let mut last_frame = tokio::time::Instant::now();
        loop {
            let wait = if said_hello {
                self.timing.idle
            } else {
                self.timing.connect_timeout
            };
            let frame = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
                    return End::Shutdown;
                }
                _ = tokio::time::sleep_until(last_frame + wait) => {
                    (self.log)(&format!(
                        "slack: nothing from the connection for {}s; connecting again",
                        wait.as_secs_f64()
                    ));
                    return End::Again { fast: said_hello };
                }
                _ = ping.tick() => {
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                        (self.log)("slack: the connection broke");
                        return End::Again { fast: false };
                    }
                    continue;
                }
                frame = socket.next() => frame,
            };
            let text = match frame {
                None | Some(Err(_)) => {
                    (self.log)("slack: the connection broke");
                    return End::Again { fast: false };
                }
                Some(Ok(frame)) => {
                    last_frame = tokio::time::Instant::now();
                    match frame {
                        Message::Close(_) => {
                            (self.log)("slack: the connection was closed");
                            return End::Again { fast: said_hello };
                        }
                        Message::Text(text) => text,
                        // Pings are answered by the library; pongs and the
                        // rest only show the connection lives.
                        _ => continue,
                    }
                }
            };
            let Ok(envelope) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            match envelope.get("type").and_then(Value::as_str) {
                Some("hello") => {
                    if !said_hello {
                        said_hello = true;
                        *backoff = self.timing.backoff_start;
                        (self.log)("slack: connected (Socket Mode)");
                    }
                }
                Some("disconnect") => {
                    let reason = envelope
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("no reason");
                    (self.log)(&format!(
                        "slack: Slack asks to reconnect ({}); connecting again",
                        crate::prompt_text::escape_hidden(&short(reason))
                    ));
                    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
                    // `link_disabled`: Socket Mode was switched off in the
                    // app settings; no hurry to ask again.
                    return End::Again {
                        fast: said_hello && reason != "link_disabled",
                    };
                }
                _ => {
                    if let Some(id) = envelope.get("envelope_id").and_then(Value::as_str) {
                        let ack = json!({"envelope_id": id}).to_string();
                        if socket.send(Message::Text(ack.into())).await.is_err() {
                            // Not acknowledged: Slack sends it again.
                            (self.log)("slack: the connection broke");
                            return End::Again { fast: false };
                        }
                    }
                    let kind = envelope.get("type").and_then(Value::as_str);
                    if matches!(kind, Some("events_api" | "interactive"))
                        && let Some(payload) = envelope.get("payload")
                    {
                        let _ = events.send(payload.clone());
                    }
                }
            }
        }
    }
}

/// At most 200 characters of `text`.
fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// Whether `err` (from [`Socket::run`]) is Slack refusing the app.
pub fn is_refused(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(ApiError::is_fatal)
}

#[cfg(test)]
mod tests {
    use super::super::config::tests::{app_token, bot_token};
    use super::super::config::{ALLOWED_USERS_ENV, APP_TOKEN_ENV, BOT_TOKEN_ENV, SlackConfig};
    use super::*;

    fn socket(local: bool) -> Socket {
        let map = [
            (BOT_TOKEN_ENV, bot_token()),
            (APP_TOKEN_ENV, app_token()),
            (ALLOWED_USERS_ENV, "U1A".to_string()),
        ];
        let config = SlackConfig::from_lookup(|key| {
            map.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let log: Log = Arc::new(|_: &str| {});
        let api = Arc::new(Api::new(&config, Arc::clone(&log)).unwrap());
        Socket::new(api, Timing::default(), log, local)
    }

    #[test]
    fn connects_only_to_slack() {
        let slack = socket(false);
        for good in [
            "wss://wss-primary.slack.com/link/?ticket=1&app_id=2",
            "wss://slack.com/link",
        ] {
            slack.check(good).unwrap();
        }
        for bad in [
            "ws://wss-primary.slack.com/link",
            "wss://slack.com.evil.example/link",
            "wss://evilslack.com/link",
            "wss://127.0.0.1:9/link",
            "https://wss.slack.com/link",
            "wss://user@wss.slack.com/link",
            "not a url",
        ] {
            assert!(slack.check(bad).is_err(), "{bad}");
        }
        let local = socket(true);
        local.check("ws://127.0.0.1:9/link?ticket=1").unwrap();
        assert!(local.check("ws://10.0.0.1:9/link").is_err());
    }
}
