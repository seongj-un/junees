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

pub mod cli;
pub mod command;
pub mod daemon;
pub mod deliver;
pub mod output;
pub mod run;
pub mod schedule;
pub mod store;
pub mod tz;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::prompt_text::escape_hidden;
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

/// Characters of a prompt shown in a listing.
const PROMPT_PREVIEW: usize = 80;

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

/// The jobs as text: id, name, schedule, next and last run, and with
/// `detail` the prompt (shortened), folder and destination too.
pub fn overview(jobs: &[Job], tz: &TimeZone, detail: bool) -> String {
    if jobs.is_empty() {
        return "(no scheduled jobs; add one with `junees cron add \"<schedule>\" \"<prompt>\"`)\n"
            .to_string();
    }
    let mut out = String::new();
    for job in jobs {
        let mut line = job.id.clone();
        if let Some(name) = &job.name {
            line.push_str(&format!(" {}", escape_hidden(name)));
        }
        line.push_str(&format!(": {}", job.schedule.describe(tz)));
        if job.paused {
            line.push_str(", paused");
        } else if let Some(next) = job.next_run {
            line.push_str(&format!(", next {}", tz.format(next)));
        } else {
            line.push_str(", done");
        }
        out.push_str(&line);
        out.push('\n');
        if let Some(last) = &job.last {
            let mut line = format!(
                "  last: {} at {}",
                last.status.word(),
                tz.format(last.started_at)
            );
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
            out.push_str(&format!(
                "  prompt: {}\n  folder: {}, deliver: {}\n",
                crate::cli::preview(&escape_hidden(&job.prompt), PROMPT_PREVIEW),
                escape_hidden(&job.dir.display().to_string()),
                job.deliver
            ));
        }
    }
    out
}

/// The listing of `/cron` and `!cron`: the jobs of `home`, and whether a
/// daemon runs them.
pub fn listing(
    home: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<String>,
    detail: bool,
) -> String {
    let Some(home) = home else {
        return "no scheduled jobs: neither JUNEES_HOME nor HOME is set\n".to_string();
    };
    let tz = match timezone(lookup) {
        Ok(tz) => tz,
        Err(err) => return format!("cannot read the time zone: {err:#}\n"),
    };
    let jobs = match Store::new(home).jobs() {
        Ok(jobs) => jobs,
        Err(err) => return format!("cannot read the scheduled jobs: {err:#}\n"),
    };
    let mut out = overview(&jobs, &tz, detail);
    if !jobs.is_empty() && !daemon::is_running(home) {
        out.push_str("(no `junees cron daemon` is running: the jobs wait until one does)\n");
    }
    out
}

/// [`listing`] with the settings of the environment and the settings
/// file (for the gateway, which keeps no settings of its own).
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
    use crate::cron::schedule::Schedule;
    use crate::cron::store::RunStatus;
    use crate::cron::store::tests::job;

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

    #[test]
    fn the_overview_shows_guild_channels_less() {
        let tz = TimeZone::utc();
        let mut daily = job(
            "aaaaaaaa",
            Schedule::Daily { hour: 9, minute: 0 },
            Some(1_794_301_200),
        );
        daily.name = Some("news".into());
        daily.prompt = "summarise\nthe news".into();
        daily.last = Some(LastRun {
            run_id: "r".into(),
            due: 0,
            started_at: 0,
            finished_at: Some(1),
            status: RunStatus::Failed,
            note: "time limit reached (30m)".into(),
            output: None,
            session: None,
            tokens: 0,
            delivery_error: Some("no webhook".into()),
        });
        let mut paused = job(
            "bbbbbbbb",
            Schedule::Every {
                secs: 3600,
                anchor: 0,
            },
            Some(5),
        );
        paused.paused = true;
        let done = job("cccccccc", Schedule::Once { at: 5 }, None);
        let jobs = [daily, paused, done];
        let full = overview(&jobs, &tz, true);
        assert_eq!(
            full,
            "aaaaaaaa news: daily 09:00 (UTC), next 2026-11-10 09:00 +00:00\n\
             \x20 last: failed at 1970-01-01 00:00 +00:00 (time limit reached (30m)); not delivered: no webhook\n\
             \x20 prompt: summarise the news\n\
             \x20 folder: /, deliver: local\n\
             bbbbbbbb: every 1h, paused\n\
             \x20 prompt: say hi\n\
             \x20 folder: /, deliver: local\n\
             cccccccc: once at 1970-01-01 00:00 +00:00, done\n\
             \x20 prompt: say hi\n\
             \x20 folder: /, deliver: local\n"
        );
        let short = overview(&jobs, &tz, false);
        assert!(
            !short.contains("prompt") && !short.contains("folder") && !short.contains("webhook")
        );
        assert!(overview(&[], &tz, true).contains("no scheduled jobs"));
    }
}
