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
