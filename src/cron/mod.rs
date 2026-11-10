//! Scheduled jobs (`junees cron`): prompts that run on a schedule, each in
//! a new session on the unattended path of `junees -p`, with the result
//! written to `$JUNEES_HOME/cron/output/<id>/` and delivered to a Discord
//! channel, the task webhook, or nowhere else.
//!
//! - [`schedule`]: the schedule grammar and next run times, in the zone of
//!   the `timezone` setting ([`tz`], read from the system's zone files).
//! - [`store`]: `cron/jobs.json`, locked and written atomically.
//! - [`daemon`]: `junees cron daemon`, which starts due jobs as
//!   `junees cron run <id>` processes ([`run`]) and records what they left.
//! - [`deliver`], [`output`]: where results go.
//! - [`cli`], [`command`]: the `junees cron` commands.

pub mod deliver;
pub mod schedule;
pub mod store;
pub mod tz;

use std::path::{Path, PathBuf};

/// The cron folder of the state folder `home`.
pub fn dir(home: &Path) -> PathBuf {
    home.join("cron")
}
