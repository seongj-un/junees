//! `junees gateway`: Junees as a Discord bot.
//!
//! Hand-rolled on Discord's Gateway v10 (a WebSocket, [`shard`]) and REST
//! API ([`rest`]) instead of a bot framework, which would cost far more to
//! build than everything else here. Each channel (or thread, or DM) has its
//! own conversation, saved as an ordinary session and continued after a
//! restart ([`sessions`]); messages of one channel run in order, channels
//! at the same time up to `discord_parallel` ([`bot`]). Only the users in
//! `discord_allowed_users` are listened to.

pub mod config;
pub mod rest;
pub mod split;

use std::sync::Arc;

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
