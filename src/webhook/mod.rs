//! Webhooks (`junees webhook`): HTTP requests that start an unattended
//! run, like a scheduled job's ([`crate::cron`]), with a prompt made from
//! the request.
//!
//! - [`store`]: `webhooks/routes.json`, the routes (name, kind, prompt
//!   template, where the secret is, event filter, folder, destination).
//! - [`server`]: `junees webhook serve`, a small HTTP/1.1 receiver
//!   ([`http`]) that checks signatures ([`signature`]), drops repeated
//!   deliveries and filtered events, answers 202 at once and runs each
//!   route's requests one at a time as `junees webhook run` processes.
//! - [`run`]: one run: the prompt ([`template`]), the cron engine, the
//!   output file, the delivery.
//! - [`cli`], [`command`]: the `junees webhook` commands.

pub mod cli;
pub mod command;
pub mod http;
pub mod run;
pub mod server;
pub mod signature;
pub mod store;
pub mod template;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cron::run::Limits;
use crate::cron::tz::TimeZone;
use crate::prompt_text::escape_hidden;
use store::{Route, Store};

/// The time one run may take.
pub const MAX_TIME_ENV: &str = "JUNEES_WEBHOOK_MAX_TIME";
/// The tokens one run may use (`0` for no limit).
pub const MAX_TOKENS_ENV: &str = "JUNEES_WEBHOOK_MAX_TOKENS";

/// The address `junees webhook serve` listens on by default.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8650";

/// The lock file of a running server, in the webhooks folder.
pub const LOCK_FILE: &str = "server.lock";

/// Characters of a prompt shown in a listing.
const PROMPT_PREVIEW: usize = 80;

/// The webhooks folder of the state folder `home`.
pub fn dir(home: &Path) -> PathBuf {
    home.join("webhooks")
}

/// The output folder of route `name`.
pub fn output_dir(home: &Path, name: &str) -> PathBuf {
    dir(home).join("output").join(name)
}

/// The limits of one run (`webhook_max_time`, `webhook_max_tokens`; the
/// cron defaults when unset).
pub fn limits(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Limits> {
    crate::cron::limits_from(lookup, MAX_TIME_ENV, MAX_TOKENS_ENV)
}

/// Whether a server holds the lock of `home` now.
pub fn is_serving(home: &Path) -> bool {
    let Ok(file) = std::fs::File::open(dir(home).join(LOCK_FILE)) else {
        return false;
    };
    // SAFETY: `file` owns the descriptor and outlives the call; a lock
    // taken here is released when it is dropped.
    unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        ) != 0
    }
}

/// The routes as text: name, kind, events, and with `detail` the secret's
/// place, prompt (shortened), folder and destination too.
pub fn overview(routes: &[Route], tz: &TimeZone, detail: bool) -> String {
    if routes.is_empty() {
        return "(no webhook routes; add one with `junees webhook add <name> --prompt \"...\"`)\n"
            .to_string();
    }
    let mut out = String::new();
    for route in routes {
        let mut line = format!("{} ({})", route.name, route.kind.word());
        if route.events.is_empty() {
            line.push_str(": every event");
        } else {
            line.push_str(&format!(": {}", route.events.join(", ")));
        }
        if route.secret.is_none() {
            line.push_str(", no secret (loopback only)");
        }
        if route.paused {
            line.push_str(", paused");
        }
        out.push_str(&line);
        out.push('\n');
        if let Some(last) = &route.last {
            let mut line = format!("  last: {}", last.status.word());
            if let Some(event) = &route.last_event {
                line.push_str(&format!(" ({})", escape_hidden(event)));
            }
            line.push_str(&format!(" at {}", tz.format(last.started_at)));
            if !last.note.is_empty() {
                line.push_str(&format!(
                    " ({})",
                    crate::cli::preview(&escape_hidden(&last.note), 120)
                ));
            }
            if let Some(err) = &last.delivery_error
                && detail
            {
                line.push_str(&format!("; not delivered: {}", escape_hidden(err)));
            }
            out.push_str(&line);
            out.push('\n');
        }
        if detail {
            let secret = route
                .secret
                .as_ref()
                .map_or_else(|| "none".to_string(), |secret| secret.describe());
            out.push_str(&format!(
                "  prompt: {}\n  folder: {}, deliver: {}, secret: {}\n",
                crate::cli::preview(&escape_hidden(&route.prompt), PROMPT_PREVIEW),
                escape_hidden(&route.dir.display().to_string()),
                route.deliver,
                escape_hidden(&secret)
            ));
        }
    }
    out
}

/// The listing of `/webhooks` and `!webhooks`.
pub fn listing(
    home: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<String>,
    detail: bool,
) -> String {
    let Some(home) = home else {
        return "no webhook routes: neither JUNEES_HOME nor HOME is set\n".to_string();
    };
    let tz = crate::cron::timezone(lookup).unwrap_or_else(|_| TimeZone::utc());
    let routes = match Store::new(home).routes() {
        Ok(routes) => routes,
        Err(err) => return format!("cannot read the webhook routes: {err:#}\n"),
    };
    let mut out = overview(&routes, &tz, detail);
    if !routes.is_empty() && !is_serving(home) {
        out.push_str("(no `junees webhook serve` is running: requests are not received)\n");
    }
    out
}

/// [`listing`] with the settings of the environment and the settings file
/// (for the gateway).
pub fn listing_from_env(home: Option<&Path>, detail: bool) -> String {
    let settings = crate::config_file::Settings::from_env().ok();
    let lookup = |key: &str| match &settings {
        Some(settings) => settings.get(key),
        None => std::env::var(key).ok(),
    };
    listing(home, &lookup, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::deliver::Delivery;
    use crate::cron::store::{LastRun, RunStatus};
    use store::tests::route;

    #[test]
    fn the_overview_hides_prompts_and_folders_unless_asked() {
        let tz = TimeZone::utc();
        let mut pr = route("pr");
        pr.events = vec!["pull_request.opened".into()];
        pr.deliver = Delivery::Discord("42".into());
        pr.last = Some(LastRun {
            run_id: "r".into(),
            due: 0,
            started_at: 60,
            finished_at: Some(61),
            status: RunStatus::Failed,
            note: "time limit reached (30m)".into(),
            output: None,
            session: None,
            tokens: 0,
            delivery_error: Some("no Discord bot token".into()),
        });
        pr.last_event = Some("pull_request.opened".into());
        let mut open = route("open");
        open.kind = store::Kind::Generic;
        open.secret = None;
        open.paused = true;
        let routes = [pr, open];
        assert_eq!(
            overview(&routes, &tz, true),
            "pr (github): pull_request.opened\n\
             \x20 last: failed (pull_request.opened) at 1970-01-01 00:01 +00:00 (time limit \
             reached (30m)); not delivered: no Discord bot token\n\
             \x20 prompt: review {{payload.number}}\n\
             \x20 folder: /, deliver: discord:42, secret: env TEST_HOOK_SECRET\n\
             open (generic): every event, no secret (loopback only), paused\n\
             \x20 prompt: review {{payload.number}}\n\
             \x20 folder: /, deliver: local, secret: none\n"
        );
        let short = overview(&routes, &tz, false);
        for hidden in ["prompt", "folder", "TEST_HOOK_SECRET", "not delivered"] {
            assert!(!short.contains(hidden), "{hidden}: {short}");
        }
        assert!(overview(&[], &tz, false).contains("no webhook routes"));
    }

    #[test]
    fn limits_have_their_own_settings() {
        let set = |key: &str| match key {
            MAX_TIME_ENV => Some("5m".to_string()),
            MAX_TOKENS_ENV => Some("100".to_string()),
            crate::cron::MAX_TIME_ENV => Some("1h".to_string()),
            _ => None,
        };
        let limits = limits(&set).unwrap();
        assert_eq!(limits.max_time, std::time::Duration::from_secs(300));
        assert_eq!(limits.max_tokens, 100);
        let bad = |key: &str| (key == MAX_TOKENS_ENV).then(|| "lots".to_string());
        let err = super::limits(&bad).unwrap_err().to_string();
        assert!(err.contains(MAX_TOKENS_ENV), "{err}");
    }
}
