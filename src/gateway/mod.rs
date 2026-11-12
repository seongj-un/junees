//! `junees gateway`: Junees as a Discord bot.
//!
//! Hand-rolled on Discord's Gateway v10 (a WebSocket, [`shard`]) and REST
//! API ([`rest`]) instead of a bot framework, which would cost far more to
//! build than everything else here. Each channel (or thread, or DM) has its
//! own conversation, saved as an ordinary session and continued after a
//! restart ([`sessions`]); messages of one channel run in order, channels
//! at the same time up to `discord_parallel` ([`bot`]). Only the users in
//! `discord_allowed_users` are listened to.

pub mod ask;
pub mod bot;
pub mod channels;
pub mod config;
pub mod engine;
pub mod platform;
pub mod rest;
pub mod sessions;
pub mod shard;
pub mod split;

use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::{Semaphore, mpsc};

use crate::cancel::Cancel;
use crate::memory::Memory;
use bot::{Bot, Discord};
use channels::Shared;
use config::DiscordConfig;
use engine::Engines;
use rest::Rest;
use sessions::SessionIndex;
use shard::{Shard, Timing};

/// Where the gateway writes what it does (one line each). Lines never hold
/// the bot token or message text.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// A [`Log`] to stderr, each line after the UTC time.
pub fn stderr_log() -> Log {
    Arc::new(|line: &str| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_secs());
        eprintln!("{} {line}", crate::session::format_utc_seconds(now));
    })
}

/// How long the channels get to stop when the gateway does.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// The file a running gateway keeps locked, inside the state folder.
pub const LOCK_FILE: &str = "gateway/lock";

/// Takes the gateway lock of the state folder `home` (an exclusive `flock`
/// on [`LOCK_FILE`], held while the file stays open). Two gateways on one
/// state folder would both answer every message, run the tools twice and
/// take each other's channel sessions, so a second one is refused.
pub fn lock(home: &Path) -> Result<File> {
    lock_file(home, LOCK_FILE)
}

/// [`lock`] on the lock file `name` (relative to `home`): each gateway
/// (Discord, Telegram) has its own, since they answer different chats.
pub fn lock_file(home: &Path, name: &str) -> Result<File> {
    let path = home.join(name);
    let dir = path.parent().context("the lock file has no folder")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let taken = crate::flock::try_exclusive(&file)
        .with_context(|| format!("cannot lock {}", path.display()))?;
    if !taken {
        bail!(
            "another Junees gateway is running on {} (it holds {}); stop it first",
            home.display(),
            path.display()
        );
    }
    Ok(file)
}

/// Everything the gateway runs with.
pub struct Gateway {
    pub config: DiscordConfig,
    pub engines: Arc<dyn Engines>,
    /// The state folder: sessions and the channel index. `None` saves
    /// nothing.
    pub home: Option<PathBuf>,
    /// The process's current folder: the tools run there, and approval
    /// questions and sessions name it.
    pub cwd: PathBuf,
    /// What `!memory` shows.
    pub memory: Option<Memory>,
    pub timing: Timing,
    pub typing_every: Duration,
    pub log: Log,
}

impl Gateway {
    /// A gateway with the default timing, logging to stderr.
    pub fn new(
        config: DiscordConfig,
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
            typing_every: Duration::from_secs(8),
            log: stderr_log(),
        }
    }

    /// Refuses to start while another gateway runs on the same state
    /// folder ([`lock`]).
    ///
    /// Connects and answers until `shutdown` is set (then running turns are
    /// cancelled and recorded, and the connection closed) or Discord
    /// refuses the bot for good (an error). The channels run on the calling
    /// thread ([`tokio::task::LocalSet`]), so the future is not `Send`:
    /// `block_on` it rather than spawning it.
    pub async fn run(self, shutdown: Cancel) -> Result<()> {
        // Held until the gateway returns.
        let _lock = self.home.as_deref().map(lock).transpose()?;
        let index = SessionIndex::load(self.home.as_deref())?;
        let rest = Arc::new(Rest::new(&self.config, Arc::clone(&self.log))?);
        let url = tokio::select! {
            url = rest.gateway_url() => url.context("cannot reach Discord (Get Gateway Bot)")?,
            _ = shutdown.cancelled() => return Ok(()),
        };
        (self.log)(&format!(
            "gateway: {}; {} allowed users, {} listed channels, {} turns at once",
            self.engines.describe(),
            self.config.allowed_users.len(),
            self.config.channels.len(),
            self.config.parallel
        ));
        let fetcher = crate::images::discord::Fetcher::new(self.config.cdn_url.as_deref())?;
        let shared = Arc::new(Shared {
            platform: Discord::new(rest, fetcher),
            engines: self.engines,
            permits: Arc::new(Semaphore::new(self.config.parallel)),
            parallel: self.config.parallel,
            approval_timeout: self.config.approval_timeout,
            home: self.home,
            cwd: self.cwd,
            memory: self.memory,
            index: std::sync::Mutex::new(index),
            typing_every: self.typing_every,
            log: Arc::clone(&self.log),
            closing: AtomicBool::new(false),
        });
        let (events, mut received) = mpsc::unbounded_channel();
        let stop_shard = Cancel::new();
        let shard = Shard::new(
            self.config.token.clone(),
            url,
            self.timing,
            Arc::clone(&self.log),
        );
        let mut connection = tokio::spawn(shard.run(events, stop_shard.clone()));
        let log = Arc::clone(&self.log);
        // The channels' conversations run here, on this thread.
        let channels = tokio::task::LocalSet::new();
        let ended = channels
            .run_until(async move {
                let mut bot = Bot::new(shared, self.config.clone());
                let ended = loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => break None,
                        dispatch = received.recv() => match dispatch {
                            Some(dispatch) => bot.handle(dispatch),
                            None => break Some((&mut connection).await),
                        },
                    }
                };
                log("gateway: stopping");
                bot.shutdown(SHUTDOWN_GRACE).await;
                (ended, connection)
            })
            .await;
        stop_shard.cancel();
        let result = match ended {
            (Some(result), _) => result,
            (None, connection) => connection.await,
        };
        result.context("the gateway connection panicked")?
    }
}
