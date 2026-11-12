//! `junees gateway telegram`: Junees as a Telegram bot.
//!
//! Hand-rolled on the Bot API ([`api`]) with long polling (`getUpdates`),
//! on the same channel engine as the Discord gateway
//! ([`crate::gateway::channels`]): each chat (or forum topic) has its own
//! conversation, saved as an ordinary session and continued after a
//! restart (index `gateway/telegram-sessions.json`); messages of one chat
//! run in order, chats at the same time up to `telegram_parallel`. Only
//! the users in `telegram_allowed_users` are listened to.
//!
//! Updates are confirmed by asking for the next ones with a higher
//! `offset` (Bot API, getUpdates: "An update is considered confirmed as
//! soon as getUpdates is called with an offset higher than its
//! update_id"), after the batch was handed to the chats; a stopping
//! gateway confirms its last batch, so a restart does not run it again.

pub mod api;
pub mod bot;
pub mod config;
pub mod format;
pub mod platform;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::cancel::Cancel;
use crate::gateway::channels::Shared;
use crate::gateway::engine::Engines;
use crate::gateway::sessions::SessionIndex;
use crate::gateway::{Log, stderr_log};
use crate::memory::Memory;
use crate::session;
use api::{Api, ApiError};
use bot::Router;
use config::TelegramConfig;
use platform::Telegram;

/// The file a running Telegram gateway keeps locked, inside the state
/// folder (the Discord gateway has its own).
pub const LOCK_FILE: &str = "gateway/telegram.lock";

/// Chat key to session id, inside the state folder.
pub const INDEX_FILE: &str = "gateway/telegram-sessions.json";

/// Added to the system prompt of every Telegram conversation
/// (`telegram_format = html`, the default).
pub const TELEGRAM_NOTE: &str = "\n\nYou are talking with the user through Telegram, not a \
    terminal: each reply is posted as Telegram messages (Markdown code blocks, inline code, \
    bold, italic, strikethrough, links and > quotes are shown formatted; headings and tables \
    are not, so prefer plain sentences and short lists; long replies are cut into messages of \
    4096 characters). Keep replies short.";

/// [`TELEGRAM_NOTE`] for `telegram_format = plain`.
pub const TELEGRAM_PLAIN_NOTE: &str = "\n\nYou are talking with the user through Telegram, \
    not a terminal: each reply is posted as Telegram messages in plain text (Markdown is not \
    rendered, so prefer plain sentences and short lists; long replies are cut into messages of \
    4096 characters). Keep replies short.";

/// The system prompt note for replies sent as `format`.
pub fn telegram_note(format: config::Format) -> &'static str {
    match format {
        config::Format::Html => TELEGRAM_NOTE,
        config::Format::Plain => TELEGRAM_PLAIN_NOTE,
    }
}

/// How long the chats get to stop when the gateway does.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Updates asked for at once (`getUpdates` `limit`, 1-100).
const BATCH: u32 = 100;

/// How the gateway polls.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The long poll's `timeout`: how long Telegram holds a `getUpdates`
    /// with nothing new.
    pub poll: Duration,
    /// The first pause after a failed poll; it doubles up to
    /// `backoff_max`.
    pub backoff_start: Duration,
    pub backoff_max: Duration,
    /// How long the parts of an album received so far wait for the next
    /// one before they go on as one input.
    pub album_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(30),
            backoff_start: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            album_wait: Duration::from_secs(1),
        }
    }
}

/// Everything the Telegram gateway runs with.
pub struct TelegramGateway {
    pub config: TelegramConfig,
    pub engines: Arc<dyn Engines>,
    /// The state folder: sessions and the chat index. `None` saves nothing.
    pub home: Option<PathBuf>,
    /// The process's current folder: the tools run there.
    pub cwd: PathBuf,
    /// What `/memory` shows.
    pub memory: Option<Memory>,
    pub timing: Timing,
    /// How often typing is shown during a turn (it lasts 5 seconds).
    pub typing_every: Duration,
    pub log: Log,
}

impl TelegramGateway {
    /// A gateway with the default timing, logging to stderr.
    pub fn new(
        config: TelegramConfig,
        engines: Arc<dyn Engines>,
        home: Option<PathBuf>,
        cwd: PathBuf,
        memory: Option<Memory>,
    ) -> Self {
        Self {
            config,
            engines,
            home,
            cwd,
            memory,
            timing: Timing::default(),
            typing_every: Duration::from_secs(4),
            log: stderr_log(),
        }
    }

    /// Refuses to start while another Telegram gateway runs on the same
    /// state folder ([`LOCK_FILE`]).
    ///
    /// Polls and answers until `shutdown` is set (then running turns are
    /// cancelled and recorded, and the last updates confirmed) or Telegram
    /// refuses the bot for good (an error: wrong token, or another program
    /// takes its updates). The chats run on the calling thread
    /// ([`tokio::task::LocalSet`]), so the future is not `Send`:
    /// `block_on` it rather than spawning it.
    pub async fn run(self, shutdown: Cancel) -> Result<()> {
        // Held until the gateway returns.
        let _lock = self
            .home
            .as_deref()
            .map(|home| crate::gateway::lock_file(home, LOCK_FILE))
            .transpose()?;
        let index = SessionIndex::load_for(self.home.as_deref(), INDEX_FILE, session::TELEGRAM)?;
        let api = Arc::new(Api::new(&self.config, Arc::clone(&self.log))?);
        let me = tokio::select! {
            me = api.get_me() => me.context("cannot reach Telegram (getMe)")?,
            _ = shutdown.cancelled() => return Ok(()),
        };
        (self.log)(&format!(
            "telegram gateway: @{}; {}; {} allowed users, {} turns at once",
            me.username,
            self.engines.describe(),
            self.config.allowed_users.len(),
            self.config.parallel
        ));
        let shared = Arc::new(Shared {
            platform: Telegram::new(Arc::clone(&api), me.username.clone(), self.config.format),
            engines: self.engines,
            parallel: self.config.parallel,
            approval_timeout: self.config.approval_timeout,
            home: self.home,
            cwd: self.cwd,
            memory: self.memory,
            index: std::sync::Mutex::new(index),
            permits: Arc::new(Semaphore::new(self.config.parallel)),
            typing_every: self.typing_every,
            log: Arc::clone(&self.log),
            closing: AtomicBool::new(false),
        });
        let mut poller = Poller::new(Arc::clone(&api), self.timing, Arc::clone(&self.log));
        let log = Arc::clone(&self.log);
        let allowed = self.config.allowed_users.clone();
        let album_wait = self.timing.album_wait;
        // The chats' conversations run here, on this thread.
        let chats = tokio::task::LocalSet::new();
        chats
            .run_until(async move {
                let mut router = Router::new(shared, allowed, me, album_wait);
                let ended = loop {
                    let due = router.next_due();
                    tokio::select! {
                        _ = shutdown.cancelled() => break Ok(()),
                        // The poll is cancel-safe: updates it did not hand
                        // out come again.
                        _ = async {
                            match due {
                                Some(due) => tokio::time::sleep_until(due.into()).await,
                                None => std::future::pending().await,
                            }
                        } => router.flush_due(),
                        batch = poller.next() => match batch {
                            Ok(updates) => router.handle(&updates),
                            Err(err) => break Err(err),
                        },
                    }
                };
                log("telegram gateway: stopping");
                router.shutdown(SHUTDOWN_GRACE).await;
                if ended.is_ok() {
                    poller.confirm().await;
                }
                ended.map_err(anyhow::Error::from)
            })
            .await
    }
}

/// The long poll: asks for the updates after the last one handed out.
pub struct Poller {
    api: Arc<Api>,
    timing: Timing,
    log: Log,
    /// The `offset` of the next `getUpdates`: one more than the highest
    /// update id received. `None` until the first update.
    offset: Option<i64>,
    /// The ids handed out lately, oldest first ([`RECENT`] at most): an
    /// update the server sends again is not run twice.
    recent: VecDeque<i64>,
    /// When polls were refused with a conflict lately, oldest first.
    conflicts: VecDeque<Instant>,
}

/// Update ids remembered against duplicates.
const RECENT: usize = 4096;

/// Conflicts (409: another program polls the same bot) waited out within
/// [`CONFLICT_WINDOW`]; the next one ends the gateway. A one-off
/// `getUpdates` passes, a second gateway on the same token does not.
const CONFLICT_TRIES: usize = 5;
const CONFLICT_WINDOW: Duration = Duration::from_secs(600);

impl Poller {
    pub fn new(api: Arc<Api>, timing: Timing, log: Log) -> Self {
        Self {
            api,
            timing,
            log,
            offset: None,
            recent: VecDeque::new(),
            conflicts: VecDeque::new(),
        }
    }

    /// Counts a conflict; whether there were too many lately.
    fn too_many_conflicts(&mut self) -> bool {
        let now = Instant::now();
        while self
            .conflicts
            .front()
            .is_some_and(|at| now.duration_since(*at) >= CONFLICT_WINDOW)
        {
            self.conflicts.pop_front();
        }
        self.conflicts.push_back(now);
        self.conflicts.len() > CONFLICT_TRIES
    }

    /// The next updates, in order, none handed out before; polls (and
    /// waits out failures) until there are some. Errors only when
    /// polling cannot work ([`ApiError::is_fatal`]). Cancel-safe: the
    /// offset moves only once a batch is returned.
    pub async fn next(&mut self) -> Result<Vec<Value>, ApiError> {
        let mut backoff = self.timing.backoff_start;
        loop {
            match self
                .api
                .get_updates(self.offset, self.timing.poll, BATCH)
                .await
            {
                Ok(updates) => {
                    backoff = self.timing.backoff_start;
                    let fresh = self.take(updates);
                    if !fresh.is_empty() {
                        return Ok(fresh);
                    }
                }
                Err(err) if err.is_fatal() => return Err(err),
                Err(err) if err.is_conflict() && self.too_many_conflicts() => return Err(err),
                Err(err) => {
                    (self.log)(&format!(
                        "{err}; polling again in {:.1}s",
                        backoff.as_secs_f64()
                    ));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                }
            }
        }
    }

    /// The updates not handed out before, sorted by id, and the offset
    /// moved past every update received. Ids are compared with the recent
    /// ones rather than the offset: after a week without updates Telegram
    /// picks the next id at random (Bot API, Update `update_id`), and it
    /// may be lower.
    fn take(&mut self, updates: Vec<Value>) -> Vec<Value> {
        let mut received: Vec<(i64, Value)> = updates
            .into_iter()
            .filter_map(|update| Some((update.get("update_id")?.as_i64()?, update)))
            .collect();
        received.sort_by_key(|(id, _)| *id);
        if let Some((last, _)) = received.last() {
            self.offset = Some(last + 1);
        }
        let mut fresh = Vec::new();
        for (id, update) in received {
            if self.recent.contains(&id) {
                continue;
            }
            if self.recent.len() == RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back(id);
            fresh.push(update);
        }
        fresh
    }

    /// Confirms every update handed out (a `getUpdates` with the offset
    /// and no wait), so a restarted gateway does not run them again.
    pub async fn confirm(&self) {
        let Some(offset) = self.offset else {
            return;
        };
        let confirm = self.api.get_updates(Some(offset), Duration::ZERO, 1);
        match tokio::time::timeout(Duration::from_secs(10), confirm).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => (self.log)(&format!("cannot confirm the last updates: {err}")),
            Err(_) => (self.log)("cannot confirm the last updates: no answer in 10s"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_offset_moves_past_handed_out_updates_only() {
        let map = [
            (config::TOKEN_ENV, "1:abcdefgh".to_string()),
            (config::ALLOWED_USERS_ENV, "1".to_string()),
            (config::API_URL_ENV, "http://127.0.0.1:9".to_string()),
        ];
        let config = TelegramConfig::from_lookup(|key| {
            map.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
        .unwrap();
        let log: Log = Arc::new(|_: &str| {});
        let api = Arc::new(Api::new(&config, Arc::clone(&log)).unwrap());
        let mut poller = Poller::new(api, Timing::default(), log);
        let update = |id: i64| json!({"update_id": id, "message": {"text": id.to_string()}});
        let ids = |updates: Vec<Value>| -> Vec<i64> {
            updates
                .iter()
                .map(|u| u["update_id"].as_i64().unwrap())
                .collect()
        };
        assert_eq!(ids(poller.take(vec![update(11), update(10)])), [10, 11]);
        assert_eq!(poller.offset, Some(12));
        // Sent again (not confirmed yet), and a duplicate within a batch.
        assert_eq!(
            ids(poller.take(vec![update(11), update(12), update(12), json!({"x": 1})])),
            [12]
        );
        assert_eq!(poller.offset, Some(13));
        // The offset follows the server's answer, as the docs ask
        // ("recalculate offset after each server response").
        assert!(poller.take(vec![update(10)]).is_empty());
        assert_eq!(poller.offset, Some(11));
        // A new sequence after a quiet week.
        assert_eq!(ids(poller.take(vec![update(5)])), [5]);
        assert_eq!(poller.offset, Some(6));
    }
}
