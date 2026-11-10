//! One connection to Discord's Gateway (v10, JSON, no compression), kept
//! alive across disconnects. Developer docs, "Gateway":
//!
//! - After Hello (op 10) the first Heartbeat (op 1) goes out after
//!   `heartbeat_interval * jitter` (jitter random in 0..1), then one every
//!   interval, carrying the last sequence number. Without a Heartbeat ACK
//!   (op 11) since the last one the connection is a zombie: close it with a
//!   code other than 1000/1001 and resume. A Heartbeat from Discord asks
//!   for one at once.
//! - Identify (op 2) starts a session; Ready gives `session_id` and
//!   `resume_gateway_url`. Resume (op 6, `session_id` and `seq`) on that URL
//!   after a Reconnect (op 7), a close code that allows it, a drop without
//!   a code, or Invalid Session (op 9) with `d: true`. With `d: false` the
//!   session is gone: wait 1 to 5 seconds and identify again.
//! - Close codes 4004 and 4010 to 4014 must not be retried; 4007 and 4009
//!   need a new session.
//! - Reconnecting waits longer after every failed try (exponential, capped)
//!   so a broken network or token does not burn the 1000 identifies a day.

use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::Log;
use super::config::{Token, check_url};
use crate::cancel::Cancel;

/// GUILDS (thread events), GUILD_MESSAGES, DIRECT_MESSAGES and the
/// privileged MESSAGE_CONTENT, which has to be switched on in the
/// Developer Portal or Discord closes the connection with 4014.
pub const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

/// The Gateway version and encoding every connection asks for.
const QUERY: &str = "v=10&encoding=json";

/// Close code for leaving while keeping the session resumable: anything
/// but 1000 and 1001, which end the session.
const CLOSE_RESUMABLE: u16 = 4000;

/// Waits of the connection, so tests can make them short.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The wait before the first reconnect; doubles on every failed try.
    pub backoff_start: Duration,
    pub backoff_max: Duration,
    /// After Invalid Session, a random wait in this range (docs: 1 to 5 s).
    pub invalid_session: (Duration, Duration),
    /// How long to wait for the connection and for Hello.
    pub connect_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_start: Duration::from_secs(1),
            backoff_max: Duration::from_secs(120),
            invalid_session: (Duration::from_secs(1), Duration::from_secs(5)),
            connect_timeout: Duration::from_secs(30),
        }
    }
}

/// An event Discord dispatched (op 0): its `t` and `d`.
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub name: String,
    pub data: Value,
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The Gateway connection and the session it may resume.
pub struct Shard {
    token: Token,
    /// From Get Gateway Bot; used to identify.
    url: String,
    timing: Timing,
    log: Log,
    /// `session_id` and `resume_gateway_url` of the last Ready.
    session: Option<(String, String)>,
    /// The last sequence number received.
    seq: Option<u64>,
    /// A Ready or Resumed came on the current connection.
    connected: bool,
}

/// How one connection ended.
enum End {
    Shutdown,
    Fatal(anyhow::Error),
    /// Connect again; resume when the session is still there.
    Reconnect,
    /// Invalid Session: wait the docs' 1 to 5 seconds first.
    InvalidSession,
}

impl Shard {
    pub fn new(token: Token, url: String, timing: Timing, log: Log) -> Self {
        Self {
            token,
            url,
            timing,
            log,
            session: None,
            seq: None,
            connected: false,
        }
    }

    /// Keeps a connection up and passes every dispatch to `events`, until
    /// `shutdown` is set (then it closes with 1000, which ends the session)
    /// or Discord refuses the bot for good (a close code the docs say not
    /// to retry).
    pub async fn run(
        mut self,
        events: mpsc::UnboundedSender<Dispatch>,
        shutdown: Cancel,
    ) -> Result<()> {
        let mut delay = self.timing.backoff_start;
        loop {
            self.connected = false;
            let wait = match self.connect_once(&events, &shutdown).await {
                End::Shutdown => return Ok(()),
                End::Fatal(err) => return Err(err),
                End::InvalidSession => {
                    let (low, high) = self.timing.invalid_session;
                    low + (high.saturating_sub(low)).mul_f64(random_unit())
                }
                End::Reconnect => {
                    if self.connected {
                        delay = self.timing.backoff_start;
                    }
                    let wait = delay;
                    delay = (delay * 2).min(self.timing.backoff_max);
                    wait
                }
            };
            (self.log)(&format!(
                "gateway: reconnecting in {:.1}s ({})",
                wait.as_secs_f64(),
                if self.session.is_some() {
                    "resume"
                } else {
                    "new session"
                }
            ));
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// One connection, from connecting to its end.
    async fn connect_once(
        &mut self,
        events: &mpsc::UnboundedSender<Dispatch>,
        shutdown: &Cancel,
    ) -> End {
        let base = match &self.session {
            Some((_, resume_url)) => resume_url.clone(),
            None => self.url.clone(),
        };
        if let Err(err) = check_url(&base, "wss", "ws") {
            return End::Fatal(err.context("the Discord gateway URL"));
        }
        let url = with_query(&base);
        let connect = tokio::time::timeout(
            self.timing.connect_timeout,
            tokio_tungstenite::connect_async(url.as_str()),
        );
        let mut socket = tokio::select! {
            _ = shutdown.cancelled() => return End::Shutdown,
            result = connect => match result {
                Ok(Ok((socket, _))) => socket,
                Ok(Err(err)) => {
                    (self.log)(&format!("gateway: cannot connect: {err}"));
                    // Docs: when resuming fails, start over on the first URL.
                    self.forget_session();
                    return End::Reconnect;
                }
                Err(_) => {
                    (self.log)("gateway: connecting timed out");
                    self.forget_session();
                    return End::Reconnect;
                }
            },
        };

        let hello = tokio::select! {
            _ = shutdown.cancelled() => return End::Shutdown,
            hello = tokio::time::timeout(self.timing.connect_timeout, next_payload(&mut socket)) => hello,
        };
        let interval = match hello {
            Ok(Some(payload)) if payload.get("op").and_then(Value::as_u64) == Some(10) => payload
                .pointer("/d/heartbeat_interval")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis),
            _ => None,
        };
        let Some(interval) = interval else {
            (self.log)("gateway: no Hello after connecting");
            close(&mut socket, CLOSE_RESUMABLE).await;
            return End::Reconnect;
        };

        let start = match &self.session {
            Some((session_id, _)) => json!({
                "op": 6,
                "d": {"token": self.token.expose(), "session_id": session_id, "seq": self.seq},
            }),
            None => json!({
                "op": 2,
                "d": {
                    "token": self.token.expose(),
                    "intents": INTENTS,
                    "properties": {
                        "os": std::env::consts::OS,
                        "browser": "junees",
                        "device": "junees",
                    },
                },
            }),
        };
        if let Err(err) = send(&mut socket, &start).await {
            (self.log)(&format!("gateway: {err}"));
            return End::Reconnect;
        }

        let mut next_beat = Instant::now() + interval.mul_f64(random_unit());
        let mut acked = true;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    close(&mut socket, 1000).await;
                    return End::Shutdown;
                }
                _ = tokio::time::sleep_until(next_beat) => {
                    if !acked {
                        (self.log)("gateway: no heartbeat ACK since the last heartbeat; reconnecting");
                        close(&mut socket, CLOSE_RESUMABLE).await;
                        return End::Reconnect;
                    }
                    if let Err(err) = send(&mut socket, &json!({"op": 1, "d": self.seq})).await {
                        (self.log)(&format!("gateway: {err}"));
                        return End::Reconnect;
                    }
                    acked = false;
                    next_beat = Instant::now() + interval;
                }
                message = socket.next() => {
                    let text = match message {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(frame))) => return self.closed(frame),
                        // Pings are answered by the library; nothing else is used.
                        Some(Ok(_)) => continue,
                        Some(Err(err)) => {
                            (self.log)(&format!("gateway: connection lost: {err}"));
                            return End::Reconnect;
                        }
                        None => {
                            (self.log)("gateway: connection closed without a close code");
                            return End::Reconnect;
                        }
                    };
                    let Ok(payload) = serde_json::from_str::<Value>(text.as_str()) else {
                        (self.log)("gateway: a payload that is not JSON; ignored");
                        continue;
                    };
                    match payload.get("op").and_then(Value::as_u64) {
                        Some(0) => self.dispatch(payload, events),
                        Some(1) => {
                            if let Err(err) = send(&mut socket, &json!({"op": 1, "d": self.seq})).await {
                                (self.log)(&format!("gateway: {err}"));
                                return End::Reconnect;
                            }
                        }
                        Some(7) => {
                            (self.log)("gateway: Discord asked to reconnect");
                            close(&mut socket, CLOSE_RESUMABLE).await;
                            return End::Reconnect;
                        }
                        Some(9) => {
                            let resumable = payload.get("d").and_then(Value::as_bool) == Some(true);
                            (self.log)(&format!(
                                "gateway: invalid session ({})",
                                if resumable { "resumable" } else { "starting a new one" }
                            ));
                            if !resumable {
                                self.forget_session();
                            }
                            close(&mut socket, CLOSE_RESUMABLE).await;
                            return End::InvalidSession;
                        }
                        Some(11) => acked = true,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Notes the sequence number and the session of a dispatch and passes
    /// it on.
    fn dispatch(&mut self, payload: Value, events: &mpsc::UnboundedSender<Dispatch>) {
        if let Some(seq) = payload.get("s").and_then(Value::as_u64) {
            self.seq = Some(seq);
        }
        let name = payload
            .get("t")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = payload.get("d").cloned().unwrap_or(Value::Null);
        match name.as_str() {
            "READY" => {
                let session_id = data.get("session_id").and_then(Value::as_str);
                let resume_url = data.get("resume_gateway_url").and_then(Value::as_str);
                if let (Some(session_id), Some(resume_url)) = (session_id, resume_url) {
                    self.session = Some((session_id.to_string(), resume_url.to_string()));
                }
                self.connected = true;
                let name = data
                    .pointer("/user/username")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                (self.log)(&format!("gateway: ready as {name}"));
            }
            "RESUMED" => {
                self.connected = true;
                (self.log)("gateway: resumed");
            }
            _ => {}
        }
        // The receiver is gone only while shutting down.
        let _ = events.send(Dispatch { name, data });
    }

    /// What a close frame from Discord means.
    fn closed(&mut self, frame: Option<CloseFrame>) -> End {
        let Some(frame) = frame else {
            (self.log)("gateway: closed without a code");
            return End::Reconnect;
        };
        let code = u16::from(frame.code);
        let fatal = match code {
            4004 => Some(
                "Discord refused the bot token (4004 Authentication failed); check \
                 discord_token (Developer Portal, Bot, Reset Token)",
            ),
            4010 => Some("Discord: invalid shard (4010)"),
            4011 => Some("Discord: sharding required (4011); Junees runs a single shard"),
            4012 => Some("Discord: invalid API version (4012)"),
            4013 => Some("Discord: invalid intents (4013)"),
            4014 => Some(
                "Discord: disallowed intents (4014). Turn on MESSAGE CONTENT INTENT in the \
                 Developer Portal (your application, Bot, Privileged Gateway Intents)",
            ),
            _ => None,
        };
        if let Some(message) = fatal {
            return End::Fatal(anyhow!(message));
        }
        (self.log)(&format!(
            "gateway: closed by Discord with {code}{}",
            if frame.reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", frame.reason.chars().take(100).collect::<String>())
            }
        ));
        if matches!(code, 4007 | 4009) {
            self.forget_session();
        }
        End::Reconnect
    }

    fn forget_session(&mut self) {
        self.session = None;
        self.seq = None;
    }
}

/// `url` with the version and encoding query.
fn with_query(url: &str) -> String {
    let base = url.split('?').next().unwrap_or(url).trim_end_matches('/');
    format!("{base}/?{QUERY}")
}

/// The next JSON payload, skipping frames that are not text.
async fn next_payload(socket: &mut Socket) -> Option<Value> {
    while let Some(message) = socket.next().await {
        match message.ok()? {
            Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            Message::Close(_) => return None,
            _ => {}
        }
    }
    None
}

async fn send(socket: &mut Socket, payload: &Value) -> Result<()> {
    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        // The error names the transport only, never the payload.
        .map_err(|err| anyhow!("cannot send to the gateway: {err}"))
}

async fn close(socket: &mut Socket, code: u16) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Close(Some(frame))),
    )
    .await;
}

/// A random number in [0, 1), from the standard library's random hash
/// keys: good enough for jitter, no dependency.
pub fn random_unit() -> f64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos()),
    );
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_version_query() {
        assert_eq!(
            with_query("wss://gateway.discord.gg"),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
        assert_eq!(
            with_query("wss://gateway-us-east1-b.discord.gg/?v=9"),
            "wss://gateway-us-east1-b.discord.gg/?v=10&encoding=json"
        );
    }

    #[test]
    fn intents_are_the_documented_bits() {
        assert_eq!(INTENTS, 1 + 512 + 4096 + 32768);
    }

    /// rustls needs exactly one crypto provider compiled in (reqwest's
    /// ring); with none or two, setting up a wss connection panics.
    #[tokio::test]
    async fn wss_sets_up_tls_without_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Accept and hang up: the TLS handshake fails.
            let _ = listener.accept().await;
        });
        let result = tokio_tungstenite::connect_async(format!("wss://127.0.0.1:{port}/")).await;
        assert!(result.is_err());
    }

    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..100 {
            let value = random_unit();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }
}
