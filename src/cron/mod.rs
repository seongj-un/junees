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
pub mod output;
pub mod run;
pub mod schedule;
pub mod store;
pub mod tz;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use deliver::{Lookup, Report};
use run::{Limits, Outcome};
use store::{Job, LastRun, Store};
use tz::TimeZone;

/// The zone of schedules (an IANA name such as `Asia/Seoul`); the
/// system's zone when unset.
pub const TIMEZONE_ENV: &str = "JUNEES_TIMEZONE";
/// The time one run may take.
pub const MAX_TIME_ENV: &str = "JUNEES_CRON_MAX_TIME";
/// The tokens one run may use (`0` for no limit).
pub const MAX_TOKENS_ENV: &str = "JUNEES_CRON_MAX_TOKENS";

pub const DEFAULT_MAX_TIME: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_MAX_TOKENS: u64 = 1_000_000;

/// The time now, in unix seconds.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs() as i64)
}

/// The cron folder of the state folder `home`.
pub fn dir(home: &Path) -> PathBuf {
    home.join("cron")
}

/// The zone of the `timezone` setting, else the system's (`TZ`, then
/// `/etc/localtime`).
pub fn timezone(lookup: &dyn Fn(&str) -> Option<String>) -> Result<TimeZone> {
    match lookup(TIMEZONE_ENV).filter(|name| !name.trim().is_empty()) {
        Some(name) => {
            TimeZone::named_in(&name, &tz::zoneinfo_dir(lookup("TZDIR"))).context(TIMEZONE_ENV)
        }
        None => Ok(TimeZone::system(lookup("TZ"))),
    }
}

/// The limits of one run, from the settings.
pub fn limits(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Limits> {
    let get = |key: &str| lookup(key).filter(|value| !value.trim().is_empty());
    let max_time = match get(MAX_TIME_ENV) {
        None => DEFAULT_MAX_TIME,
        Some(raw) => crate::task::budget::parse_duration(&raw).context(MAX_TIME_ENV)?,
    };
    if max_time.is_zero() {
        bail!("{MAX_TIME_ENV} must be longer than 0s");
    }
    let max_tokens = match get(MAX_TOKENS_ENV) {
        None => DEFAULT_MAX_TOKENS,
        Some(raw) => raw
            .trim()
            .parse()
            .ok()
            .with_context(|| format!("{MAX_TOKENS_ENV} must be a whole number, got `{raw}`"))?,
    };
    Ok(Limits {
        max_time,
        max_tokens,
    })
}

/// Ends run `run_id` of `job`: writes the output file, delivers the
/// result, and records it as the job's last run (unless the job was
/// removed meanwhile). Problems with any of these are printed, not fatal.
#[allow(clippy::too_many_arguments)]
pub async fn finish(
    home: &Path,
    tz: &TimeZone,
    lookup: &Lookup,
    job: &Job,
    run_id: &str,
    due: i64,
    started_at: i64,
    outcome: &Outcome,
) -> LastRun {
    let finished_at = unix_now();
    let record = output::Record {
        job,
        status: outcome.status,
        due,
        started_at,
        finished_at,
        note: &outcome.note,
        answer: &outcome.answer,
        session: outcome.session.as_deref(),
        tokens: outcome.tokens,
    };
    let output = match output::write(home, tz, &record) {
        Ok(path) => Some(path),
        Err(err) => {
            eprintln!("  !! job {}: {err:#}", job.id);
            None
        }
    };
    let report = Report {
        job,
        status: outcome.status,
        answer: &outcome.answer,
        note: &outcome.note,
        output: output.as_deref(),
    };
    let delivery_error = match deliver::deliver(lookup, &report).await {
        Ok(()) => None,
        Err(err) => {
            let err = format!("{err:#}");
            eprintln!(
                "  !! the result of job {} did not reach {}: {err}",
                job.id, job.deliver
            );
            Some(err)
        }
    };
    let last = LastRun {
        run_id: run_id.to_string(),
        due,
        started_at,
        finished_at: Some(finished_at),
        status: outcome.status,
        note: outcome.note.clone(),
        output,
        session: outcome.session.clone(),
        tokens: outcome.tokens,
        delivery_error,
    };
    let saved = Store::new(home).lock().and_then(|mut locked| {
        if let Some(stored) = locked.jobs.iter_mut().find(|stored| stored.id == job.id) {
            stored.last = Some(last.clone());
            locked.save()?;
        }
        Ok(())
    });
    if let Err(err) = saved {
        eprintln!("  !! cannot record the run of job {}: {err:#}", job.id);
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_have_defaults_and_are_checked() {
        let none = |_: &str| None;
        assert_eq!(
            limits(&none).unwrap(),
            Limits {
                max_time: DEFAULT_MAX_TIME,
                max_tokens: DEFAULT_MAX_TOKENS
            }
        );
        let set = |key: &str| match key {
            MAX_TIME_ENV => Some("1h30m".to_string()),
            MAX_TOKENS_ENV => Some("0".to_string()),
            TIMEZONE_ENV => Some("KST-9".to_string()),
            _ => None,
        };
        assert_eq!(
            limits(&set).unwrap(),
            Limits {
                max_time: Duration::from_secs(5400),
                max_tokens: 0
            }
        );
        assert_eq!(timezone(&set).unwrap().name(), "KST-9");
        let bad = |key: &str| match key {
            MAX_TIME_ENV => Some("0s".to_string()),
            MAX_TOKENS_ENV => Some("many".to_string()),
            TIMEZONE_ENV => Some("Mars/Base".to_string()),
            _ => None,
        };
        assert!(limits(&bad).is_err());
        let err = format!("{:#}", timezone(&bad).unwrap_err());
        assert!(
            err.contains("JUNEES_TIMEZONE") && err.contains("unknown time zone"),
            "{err}"
        );
        let tokens = |key: &str| (key == MAX_TOKENS_ENV).then(|| "many".to_string());
        assert!(
            limits(&tokens)
                .unwrap_err()
                .to_string()
                .contains("whole number")
        );
    }
}
