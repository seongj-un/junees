//! `junees gateway slack`: Junees as a Slack app.
//!
//! Events come over Socket Mode ([`socket`]): an outgoing WebSocket, so
//! the machine needs no public address. Replies go through the Web API
//! ([`api`]). The conversations run on the same channel engine as the
//! Discord and Telegram gateways ([`crate::gateway::channels`]): each DM
//! and each channel thread has its own conversation, saved as an
//! ordinary session and continued after a restart (index
//! `gateway/slack-sessions.json`); messages of one conversation run in
//! order, conversations at the same time up to `slack_parallel`. Only the
//! users in `slack_allowed_users` are listened to. Approval questions
//! come with Block Kit buttons, pressed over the same socket
//! (`interactive` envelopes, [`bot`]).

pub mod api;
pub mod bot;
pub mod config;
pub mod format;
pub mod platform;
pub mod socket;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{Semaphore, mpsc};

use crate::cancel::Cancel;
use crate::gateway::channels::Shared;
use crate::gateway::engine::Engines;
use crate::gateway::sessions::SessionIndex;
use crate::gateway::{Log, stderr_log};
use crate::memory::Memory;
use crate::session;
use api::Api;
use bot::Router;
use config::SlackConfig;
use platform::Slack;
use socket::Socket;

pub use socket::Timing;

/// The file a running Slack gateway keeps locked, inside the state folder
/// (the other gateways have their own).
pub const LOCK_FILE: &str = "gateway/slack.lock";

/// Conversation key to session id, inside the state folder.
pub const INDEX_FILE: &str = "gateway/slack-sessions.json";

/// Added to the system prompt of every Slack conversation.
pub const SLACK_NOTE: &str = "\n\nYou are talking with the user through Slack, not a \
    terminal: each reply is posted as Slack messages. Bold, italics, strikethrough, inline \
    code, code blocks, links and simple lists show formatted; tables and nested formatting do \
    not, so prefer plain sentences and short lists. Long replies are cut into several \
    messages. Keep replies short.";

/// How long a conversation may sit unused before its worker ends (its
/// session stays saved and opens again with the next message).
pub const CLOSE_IDLE: Duration = Duration::from_secs(30 * 60);

/// How long the conversations get to stop when the gateway does.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Everything the Slack gateway runs with.
pub struct SlackGateway {
    pub config: SlackConfig,
    pub engines: Arc<dyn Engines>,
    /// The state folder: sessions and the conversation index. `None`
    /// saves nothing.
    pub home: Option<PathBuf>,
    /// The process's current folder: the tools run there.
    pub cwd: PathBuf,
    /// What `!memory` shows.
    pub memory: Option<Memory>,
    pub timing: Timing,
    /// See [`CLOSE_IDLE`].
    pub close_idle: Duration,
    pub log: Log,
}

impl SlackGateway {
    /// A gateway with the default timing, logging to stderr.
    pub fn new(
        config: SlackConfig,
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
            close_idle: CLOSE_IDLE,
            log: stderr_log(),
        }
    }

    /// Refuses to start while another Slack gateway runs on the same
    /// state folder ([`LOCK_FILE`]).
    ///
    /// Answers until `shutdown` is set (then running turns are cancelled
    /// and recorded) or Slack refuses a token for good (an error). The
    /// conversations run on the calling thread
    /// ([`tokio::task::LocalSet`]), so the future is not `Send`:
    /// `block_on` it rather than spawning it.
    pub async fn run(self, shutdown: Cancel) -> Result<()> {
        // Held until the gateway returns.
        let _lock = self
            .home
            .as_deref()
            .map(|home| crate::gateway::lock_file(home, LOCK_FILE))
            .transpose()?;
        let index = SessionIndex::load_for(self.home.as_deref(), INDEX_FILE, session::SLACK)?;
        let api = Arc::new(Api::new(&self.config, Arc::clone(&self.log))?);
        let me = tokio::select! {
            me = api.auth_test() => me.context("cannot reach Slack (auth.test)")?,
            _ = shutdown.cancelled() => return Ok(()),
        };
        (self.log)(&format!(
            "slack gateway: {} ({}){}; {}; {} allowed users, {} turns at once",
            crate::prompt_text::escape_hidden(&me.name),
            me.user_id,
            if me.team.is_empty() {
                String::new()
            } else {
                format!(" in {}", crate::prompt_text::escape_hidden(&me.team))
            },
            self.engines.describe(),
            self.config.allowed_users.len(),
            self.config.parallel
        ));
        let shared = Arc::new(Shared {
            platform: Slack::new(Arc::clone(&api)),
            engines: self.engines,
            parallel: self.config.parallel,
            approval_timeout: self.config.approval_timeout,
            home: self.home,
            cwd: self.cwd,
            memory: self.memory,
            index: std::sync::Mutex::new(index),
            permits: Arc::new(Semaphore::new(self.config.parallel)),
            // Slack has no typing indicator for bots: no reason to wake.
            typing_every: Duration::from_secs(3600),
            log: Arc::clone(&self.log),
            closing: AtomicBool::new(false),
        });
        let socket = Socket::new(
            api,
            self.timing,
            Arc::clone(&self.log),
            !self.config.is_slack(),
        );
        let log = Arc::clone(&self.log);
        let allowed = self.config.allowed_users.clone();
        // Every Slack thread is a conversation of its own: close the ones
        // nobody uses, or their workers and histories pile up.
        let close_idle = self.close_idle;
        let mut sweep = tokio::time::interval(
            (close_idle / 4).clamp(Duration::from_millis(10), Duration::from_secs(60)),
        );
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The conversations run here, on this thread.
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let mut router = Router::new(shared, allowed, me);
                let (events, mut received) = mpsc::unbounded_channel();
                let connection = socket.run(events, shutdown.clone());
                tokio::pin!(connection);
                let ended = loop {
                    tokio::select! {
                        ended = &mut connection => break ended,
                        Some(payload) = received.recv() => router.handle(&payload),
                        _ = sweep.tick() => router.close_idle(close_idle),
                    }
                };
                log("slack gateway: stopping");
                router.shutdown(SHUTDOWN_GRACE).await;
                ended
            })
            .await
    }
}
