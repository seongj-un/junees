//! Where a run's result goes: a Discord channel (the bot's REST client,
//! `discord_token`), the task webhook (`notify_webhook`, [`Notifier`]), or
//! only the output file (`local`). Failures and missed runs are delivered
//! too; a delivery that fails is recorded with the run.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::store::RunStatus;
use crate::gateway::config::{self as discord, DiscordConfig, Token};
use crate::gateway::rest::Rest;
use crate::gateway::split::{MESSAGE_LIMIT, split_message};
use crate::notify::Notifier;
use crate::prompt_text::escape_hidden;

/// Messages one result may take in a Discord channel; a longer answer is
/// cut and points at the output file.
pub const MAX_POSTS: usize = 5;

/// Where a job's results go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Delivery {
    /// The output file only.
    Local,
    /// The `notify_webhook` Discord webhook.
    Webhook,
    /// A Discord channel (id), posted by the bot.
    Discord(String),
}

impl Delivery {
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        match text {
            "local" => Ok(Self::Local),
            "webhook" => Ok(Self::Webhook),
            _ => {
                let Some(channel) = text.strip_prefix("discord:") else {
                    bail!("--deliver takes discord:<channel id>, webhook or local (got `{text}`)");
                };
                if channel.is_empty()
                    || channel.len() > 20
                    || !channel.bytes().all(|b| b.is_ascii_digit())
                {
                    bail!("`{channel}` is not a Discord channel id (all digits)");
                }
                Ok(Self::Discord(channel.to_string()))
            }
        }
    }
}

impl TryFrom<String> for Delivery {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        Self::parse(&text)
    }
}

impl From<Delivery> for String {
    fn from(delivery: Delivery) -> Self {
        delivery.to_string()
    }
}

impl std::fmt::Display for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Webhook => f.write_str("webhook"),
            Self::Discord(channel) => write!(f, "discord:{channel}"),
        }
    }
}

/// Reads a setting by its environment name (see [`crate::config_file::Settings::get`]).
pub type Lookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What a run produced, for delivery.
pub struct Report<'a> {
    /// What ran: `cron` or `webhook`.
    pub source: &'a str,
    /// The job or route.
    pub label: &'a str,
    pub deliver: &'a Delivery,
    pub status: RunStatus,
    /// The final answer (may be empty).
    pub answer: &'a str,
    /// Why it failed, or other notes.
    pub note: &'a str,
    pub output: Option<&'a Path>,
}

impl Report<'_> {
    /// The first line of every delivery: which job and how it went.
    pub fn headline(&self) -> String {
        let label = escape_hidden(self.label).replace('`', "'");
        let mut line = format!(
            "**junees {}** `{label}`: {}",
            self.source,
            self.status.word()
        );
        if !self.note.is_empty() {
            let note = escape_hidden(self.note).replace('`', "'");
            let note = crate::cli::preview(&note, 300);
            line.push_str(&format!(" (`{note}`)"));
        }
        line
    }

    /// The whole text: headline, then the answer.
    pub fn text(&self) -> String {
        if self.answer.trim().is_empty() {
            self.headline()
        } else {
            format!("{}\n{}", self.headline(), self.answer.trim())
        }
    }
}

/// Delivers `report` as its job asks. `Err` says why it did not arrive.
pub async fn deliver(lookup: &Lookup, report: &Report<'_>) -> Result<()> {
    match report.deliver {
        Delivery::Local => Ok(()),
        Delivery::Webhook => {
            let notifier = Notifier::from_lookup(|key| lookup(key))?;
            if notifier.host().is_none() {
                bail!("no webhook: set notify_webhook (JUNEES_NOTIFY_WEBHOOK)");
            }
            // One message, cut at Discord's limit by the notifier.
            notifier.send(&report.text()).await?;
            Ok(())
        }
        Delivery::Discord(channel) => {
            let rest = Rest::new(&discord_config(lookup)?, crate::gateway::stderr_log())?;
            for post in posts(report) {
                rest.create_message(channel, &post).await?;
            }
            Ok(())
        }
    }
}

/// The REST settings delivery needs: the bot token and the API URL (the
/// rest of the gateway's settings do not matter here).
fn discord_config(lookup: &Lookup) -> Result<DiscordConfig> {
    let get = |key: &str| {
        lookup(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let token = get(discord::TOKEN_ENV).context(
        "no Discord bot token: set discord_token (JUNEES_DISCORD_TOKEN) to deliver to a channel",
    )?;
    let api_url = get(discord::API_URL_ENV).unwrap_or_else(|| discord::DEFAULT_API_URL.to_string());
    discord::check_url(&api_url, "https", "http").context(discord::API_URL_ENV)?;
    Ok(DiscordConfig {
        token: Token::new(&token)?,
        allowed_users: Default::default(),
        channels: Default::default(),
        parallel: discord::DEFAULT_PARALLEL,
        approval_timeout: discord::DEFAULT_APPROVAL_TIMEOUT,
        api_url: api_url.trim_end_matches('/').to_string(),
    })
}

/// The Discord messages of a report: at most [`MAX_POSTS`], the last one
/// saying where the rest is when the answer was cut.
fn posts(report: &Report<'_>) -> Vec<String> {
    let mut posts = split_message(&report.text(), MESSAGE_LIMIT);
    if posts.len() > MAX_POSTS {
        posts.truncate(MAX_POSTS - 1);
        let rest = match report.output {
            Some(path) => format!(
                "(cut; the whole answer is in `{}`)",
                escape_hidden(&path.display().to_string())
            ),
            None => "(cut)".to_string(),
        };
        posts.push(rest);
    }
    posts
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::cron::schedule::Schedule;
    use crate::cron::store::tests::job;

    /// A fake HTTP server answering every request with `status` and `{}`;
    /// returns its URL and the (request line, JSON body, authorization) of
    /// each request.
    pub async fn fake_http(status: u16) -> (String, Arc<Mutex<Vec<(String, Value, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body) = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break (String::new(), Vec::new());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let length = head
                            .to_lowercase()
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break (head, buf[end + 4..end + 4 + length].to_vec());
                        }
                    }
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                let auth = head
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .starts_with("authorization:")
                            .then(|| line[14..].trim().to_string())
                    })
                    .unwrap_or_default();
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                record.lock().unwrap().push((line, body, auth));
                let response = format!(
                    "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\
                     connection: close\r\n\r\n{{}}"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (url, seen)
    }

    pub fn lookup(pairs: &[(&str, &str)]) -> Lookup {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    }

    #[test]
    fn parses_destinations() {
        assert_eq!(Delivery::parse("local").unwrap(), Delivery::Local);
        assert_eq!(Delivery::parse("webhook").unwrap(), Delivery::Webhook);
        assert_eq!(
            Delivery::parse("discord:123456789012345678").unwrap(),
            Delivery::Discord("123456789012345678".into())
        );
        for bad in ["discord:", "discord:abc", "discord:1/2", "slack", ""] {
            assert!(Delivery::parse(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_string(&Delivery::Discord("42".into())).unwrap();
        assert_eq!(json, "\"discord:42\"");
        assert!(serde_json::from_str::<Delivery>("\"discord:x\"").is_err());
    }

    #[tokio::test]
    async fn posts_to_a_discord_channel_with_the_bot_token() {
        let (url, seen) = fake_http(200).await;
        let mut job = job("0123abcd", Schedule::Once { at: 1 }, None);
        job.name = Some("news`digest".into());
        job.deliver = Delivery::Discord("42".into());
        let settings = lookup(&[
            ("JUNEES_DISCORD_TOKEN", "bot-token"),
            ("JUNEES_DISCORD_API_URL", &url),
        ]);
        let report = Report {
            source: "cron",
            label: job.label(),
            deliver: &job.deliver,
            status: RunStatus::Failed,
            answer: "partial 답",
            note: "time limit reached",
            output: None,
        };
        deliver(&settings, &report).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "POST /channels/42/messages HTTP/1.1");
        assert_eq!(seen[0].2, "Bot bot-token");
        let content = seen[0].1["content"].as_str().unwrap();
        assert_eq!(
            content,
            "**junees cron** `news'digest`: failed (`time limit reached`)\npartial 답"
        );
        assert_eq!(
            seen[0].1["allowed_mentions"]["parse"],
            serde_json::json!([])
        );
    }

    #[tokio::test]
    async fn long_answers_are_cut_and_point_at_the_file() {
        let (url, seen) = fake_http(200).await;
        let mut job = job("0123abcd", Schedule::Once { at: 1 }, None);
        job.deliver = Delivery::Discord("42".into());
        let answer = "줄\n".repeat(6000);
        let report = Report {
            source: "cron",
            label: job.label(),
            deliver: &job.deliver,
            status: RunStatus::Ok,
            answer: &answer,
            note: "",
            output: Some(Path::new("/h/cron/output/0123abcd/x.md")),
        };
        let settings = lookup(&[
            ("JUNEES_DISCORD_TOKEN", "t"),
            ("JUNEES_DISCORD_API_URL", &url),
        ]);
        deliver(&settings, &report).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), MAX_POSTS);
        assert_eq!(
            seen[MAX_POSTS - 1].1["content"],
            "(cut; the whole answer is in `/h/cron/output/0123abcd/x.md`)"
        );
    }

    #[tokio::test]
    async fn the_webhook_gets_one_message_and_failures_are_errors() {
        let (url, seen) = fake_http(204).await;
        let mut job = job("0123abcd", Schedule::Once { at: 1 }, None);
        job.deliver = Delivery::Webhook;
        let report = Report {
            source: "cron",
            label: job.label(),
            deliver: &job.deliver,
            status: RunStatus::Ok,
            answer: "done",
            note: "",
            output: None,
        };
        deliver(
            &lookup(&[("JUNEES_NOTIFY_WEBHOOK", &format!("{url}/hook"))]),
            &report,
        )
        .await
        .unwrap();
        assert_eq!(seen.lock().unwrap()[0].0, "POST /hook HTTP/1.1");
        assert_eq!(
            seen.lock().unwrap()[0].1["content"],
            "**junees cron** `0123abcd`: ok\ndone"
        );
        let err = deliver(&lookup(&[]), &report).await.unwrap_err();
        assert!(err.to_string().contains("no webhook"), "{err}");
        let (url, _) = fake_http(500).await;
        let err = deliver(&lookup(&[("JUNEES_NOTIFY_WEBHOOK", &url)]), &report)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
        let mut job = job.clone();
        job.deliver = Delivery::Discord("42".into());
        let report = Report {
            deliver: &job.deliver,
            ..report
        };
        let err = deliver(&lookup(&[]), &report).await.unwrap_err();
        assert!(err.to_string().contains("no Discord bot token"), "{err}");
    }
}
