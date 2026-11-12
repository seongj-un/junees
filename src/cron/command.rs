//! The `junees cron` commands.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::signal::unix::{SignalKind, signal};

use super::cli::{CronCommand, USAGE};
use super::daemon::{self, Daemon, ProcessLauncher, SystemClock};
use super::deliver::{Delivery, Lookup};
use super::run::{ApiEngine, ClaudeEngine, Engine, Outcome};
use super::schedule::Schedule;
use super::store::{Job, LastRun, RunStatus, Store, new_id, random_hex};
use super::tz::TimeZone;
use super::unix_now;
use crate::cancel::Cancel;
use crate::claude_code::{Backend, ClaudeCode};
use crate::config::Config;
use crate::config_file::{self, Settings};
use crate::gateway::config as discord;
use crate::prompt_text::{escape_hidden, is_hidden};

/// Longest job name.
const NAME_CHARS: usize = 64;
/// Longest prompt.
const PROMPT_CHARS: usize = 50_000;

/// Runs `command` with the settings of `settings`.
pub async fn main(command: CronCommand, settings: Settings) -> Result<ExitCode> {
    let settings = Arc::new(settings);
    let lookup: Lookup = {
        let settings = Arc::clone(&settings);
        Arc::new(move |key: &str| settings.get(key))
    };
    if command == CronCommand::Help {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let home = config_file::home_dir(|key| lookup(key))
        .context("scheduled jobs live in the state folder: set JUNEES_HOME or HOME")?;
    let tz = super::timezone(&*lookup)?;
    let store = Store::new(&home);
    match command {
        CronCommand::Add {
            schedule,
            prompt,
            name,
            dir,
            deliver,
        } => {
            let job = new_job(&store, &tz, &lookup, schedule, prompt, name, dir, deliver)?;
            println!(
                "added job {} ({}), next run {}",
                job.id,
                job.schedule.describe(&tz),
                job.next_run
                    .map_or_else(|| "-".to_string(), |next| tz.format(next))
            );
            if !daemon::is_running(&home) {
                eprintln!(
                    "no `junees cron daemon` is running: start it (see README, \"예약 작업\") \
                     or the job waits"
                );
            }
        }
        CronCommand::List => print!("{}", super::listing(Some(&home), &*lookup, true)),
        CronCommand::Remove(key) => {
            let mut locked = store.lock()?;
            let index = locked.find(&key)?;
            let job = locked.jobs.remove(index);
            locked.save()?;
            println!(
                "removed job {} ({})",
                job.id,
                job.schedule.describe(&job.zone(&tz))
            );
        }
        CronCommand::Pause(key) => {
            let mut locked = store.lock()?;
            let index = locked.find(&key)?;
            locked.jobs[index].paused = true;
            locked.save()?;
            println!("paused job {}", locked.jobs[index].id);
        }
        CronCommand::Resume(key) => {
            let mut locked = store.lock()?;
            let index = locked.find(&key)?;
            let job = &mut locked.jobs[index];
            let tz = job.zone(&tz);
            let next = job.schedule.next_after(unix_now(), &tz);
            if next.is_none() {
                bail!(
                    "job {} was to run once, at a time that has passed; add it again",
                    job.id
                );
            }
            job.paused = false;
            job.next_run = next;
            let id = job.id.clone();
            locked.save()?;
            println!(
                "resumed job {id}, next run {}",
                next.map_or_else(String::new, |next| tz.format(next))
            );
        }
        CronCommand::Run { job, run_id, due } => {
            return run_now(&home, &tz, &lookup, &job, run_id, due).await;
        }
        CronCommand::Daemon => {
            let _lock = daemon::lock(&home)?;
            let limits = super::limits(&*lookup)?;
            let program = std::env::current_exe().context("cannot find the junees program")?;
            let cancel = Cancel::new();
            cancel_on_signals(cancel.clone())?;
            eprintln!(
                "junees cron daemon for {}, times in {}; SIGINT or SIGTERM stops it",
                home.display(),
                tz.name()
            );
            Daemon::new(
                &home,
                tz,
                Arc::new(SystemClock),
                Box::new(ProcessLauncher::new(program, limits)),
                lookup,
                crate::gateway::stderr_log(),
            )
            .run(cancel)
            .await?;
            eprintln!("cron daemon stopped");
        }
        CronCommand::Help => unreachable!("handled above"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Checks and saves a new job.
#[allow(clippy::too_many_arguments)]
fn new_job(
    store: &Store,
    tz: &TimeZone,
    lookup: &Lookup,
    schedule_text: String,
    prompt: String,
    name: Option<String>,
    dir: Option<std::path::PathBuf>,
    deliver: Option<String>,
) -> Result<Job> {
    let now = unix_now();
    let schedule = Schedule::parse(&schedule_text, now, tz)?;
    check_prompt(&prompt)?;
    if let Some(name) = &name {
        check_name(name)?;
    }
    let dir = match dir {
        Some(dir) => dir,
        None => std::env::current_dir().context("cannot read the current folder")?,
    };
    let dir = std::fs::canonicalize(&dir)
        .ok()
        .filter(|dir| dir.is_dir())
        .with_context(|| format!("{} is not a folder", dir.display()))?;
    let deliver = match deliver {
        Some(text) => Delivery::parse(&text)?,
        None => Delivery::Local,
    };
    match &deliver {
        Delivery::Discord(_)
            if lookup(discord::TOKEN_ENV).is_none_or(|token| token.trim().is_empty()) =>
        {
            eprintln!(
                "  !! no discord_token is set: delivery to the channel will fail until it is"
            );
        }
        Delivery::Webhook
            if lookup(crate::notify::WEBHOOK_ENV).is_none_or(|url| url.trim().is_empty()) =>
        {
            eprintln!("  !! no notify_webhook is set: delivery will fail until it is");
        }
        _ => {}
    }
    let mut locked = store.lock()?;
    if let Some(name) = &name
        && locked
            .jobs
            .iter()
            .any(|job| job.name.as_ref() == Some(name))
    {
        bail!("a job named `{}` exists already", escape_hidden(name));
    }
    let job = Job {
        id: new_id(&locked.jobs),
        name,
        next_run: schedule.next_after(now, tz),
        schedule,
        // Kept if the zone can be found by its name again.
        timezone: TimeZone::named(tz.name())
            .is_ok_and(|named| named == *tz)
            .then(|| tz.name().to_string()),
        text: schedule_text.trim().to_string(),
        prompt,
        dir,
        deliver,
        paused: false,
        created_at: now,
        last: None,
    };
    locked.jobs.push(job.clone());
    locked.save()?;
    Ok(job)
}

fn check_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    if prompt.chars().count() > PROMPT_CHARS {
        bail!("the prompt is longer than {PROMPT_CHARS} characters");
    }
    if let Some(c) = prompt
        .chars()
        .find(|&c| is_hidden(c) && c != '\n' && c != '\t')
    {
        bail!(
            "the prompt contains U+{:04X}, a control or bidirectional formatting character",
            c as u32
        );
    }
    Ok(())
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.trim().is_empty()
        && name.chars().count() <= NAME_CHARS
        && !name.chars().any(|c| is_hidden(c) || c == '`')
        && !name.starts_with('-');
    if !ok {
        bail!(
            "a job name is 1 to {NAME_CHARS} characters, without backticks, control characters \
             or a leading `-`"
        );
    }
    Ok(())
}

/// `junees cron run <id>`: runs the job now in its folder, records and
/// delivers the result, and prints the answer. The daemon starts runs this
/// way too, with their id and time.
async fn run_now(
    home: &Path,
    tz: &TimeZone,
    lookup: &Lookup,
    key: &str,
    run_id: Option<String>,
    due: Option<i64>,
) -> Result<ExitCode> {
    let started_at = unix_now();
    let run_id = run_id.unwrap_or_else(|| random_hex(8));
    let due = due.unwrap_or(started_at);
    let job = {
        let store = Store::new(home);
        let mut locked = store.lock()?;
        let index = locked.find(key)?;
        locked.jobs[index].last = Some(LastRun {
            run_id: run_id.clone(),
            due,
            started_at,
            finished_at: None,
            status: RunStatus::Running,
            note: String::new(),
            output: None,
            session: None,
            tokens: 0,
            delivery_error: None,
        });
        locked.save()?;
        locked.jobs[index].clone()
    };
    let cancel = Cancel::new();
    cancel_on_signals(cancel.clone())?;
    eprintln!(
        "running job {} in {}",
        job.id,
        escape_hidden(&job.dir.display().to_string())
    );
    let outcome = match prepare(lookup, &job).await {
        Ok((engine, limits)) => engine.run(&job.spec(), &cancel, limits).await,
        Err(err) => Outcome::failed(format!("{err:#}")),
    };
    let last = super::finish(home, tz, lookup, &job, &run_id, due, started_at, &outcome).await;
    if !outcome.answer.is_empty() {
        println!("{}", outcome.answer);
    }
    let mut status = format!("job {}: {}", job.id, outcome.status.word());
    if !outcome.note.is_empty() {
        status.push_str(&format!(" ({})", outcome.note));
    }
    if let Some(session) = &outcome.session {
        status.push_str(&format!("; session {session}"));
    }
    status.push_str(&format!("; {} tokens", outcome.tokens));
    if let Some(output) = &last.output {
        status.push_str(&format!("; output {}", output.display()));
    }
    eprintln!("{status}");
    Ok(if outcome.status == RunStatus::Ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The engine of the `backend` setting and the limits, in the job's
/// folder.
async fn prepare(lookup: &Lookup, job: &Job) -> Result<(Box<dyn Engine>, super::run::Limits)> {
    let limits = super::limits(&**lookup)?;
    Ok((engine_in(lookup, &job.dir).await?, limits))
}

/// Moves to `dir` (the tools work in the current folder) and makes the
/// engine of the `backend` setting there. The claude-code backend runs only
/// when [`crate::claude_code::check_unattended`] allows it.
pub async fn engine_in(lookup: &Lookup, dir: &Path) -> Result<Box<dyn Engine>> {
    std::env::set_current_dir(dir).with_context(|| format!("cannot work in {}", dir.display()))?;
    let home = lookup("HOME")
        .filter(|home| !home.trim().is_empty())
        .map(|home| std::path::PathBuf::from(home.trim()));
    let state = config_file::home_dir(|key| lookup(key));
    if let Some(warning) =
        crate::tools::unattended_folder_warning(dir, home.as_deref(), state.as_deref())
    {
        eprintln!("  !! {warning}");
    }
    let engine: Box<dyn Engine> = match Backend::from_lookup(|key| lookup(key))? {
        Backend::Api => Box::new(ApiEngine::new(Config::from_lookup(|key| lookup(key))?)),
        Backend::ClaudeCode => {
            let config = Config::from_lookup_without_key(|key| lookup(key))?;
            crate::claude_code::check_unattended(|key| lookup(key), config.tools.sandbox.as_ref())?;
            eprintln!(
                "  !! the claude-code backend runs this unattended run's commands without \
                 Junees's sandbox ({})",
                crate::claude_code::UNATTENDED_ENV
            );
            let claude = ClaudeCode::from_lookup(|key| lookup(key));
            claude.check(dir, &config.tools).await?;
            for notice in claude.notices() {
                eprintln!("  !! {notice}");
            }
            Box::new(ClaudeEngine::new(claude, config))
        }
    };
    Ok(engine)
}

/// Sets `cancel` on SIGINT or SIGTERM.
pub fn cancel_on_signals(cancel: Cancel) -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt()).context("cannot catch SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("cannot catch SIGTERM")?;
    tokio::spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        cancel.cancel();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::deliver::tests::lookup;

    #[test]
    fn adding_checks_the_job_and_names_are_unique() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let tz = TimeZone::utc();
        let settings = lookup(&[]);
        let add = |schedule: &str, prompt: &str, name: Option<&str>, deliver: Option<&str>| {
            new_job(
                &store,
                &tz,
                &settings,
                schedule.into(),
                prompt.into(),
                name.map(str::to_string),
                Some(home.path().to_path_buf()),
                deliver.map(str::to_string),
            )
        };
        let job = add(
            "every 2h",
            "check the logs\nand say",
            Some("logs"),
            Some("webhook"),
        )
        .unwrap();
        assert_eq!(job.next_run, Some(job.created_at + 7200));
        assert_eq!(job.dir, std::fs::canonicalize(home.path()).unwrap());
        assert_eq!(job.deliver, Delivery::Webhook);
        assert_eq!(job.timezone.as_deref(), Some("UTC"));
        assert_eq!(store.jobs().unwrap(), [job]);
        for (schedule, prompt, name, deliver, wanted) in [
            ("every 2h", "x", Some("logs"), None, "exists already"),
            ("every 2h", " ", None, None, "prompt is empty"),
            ("every 2h", "a\u{202e}b", None, None, "U+202E"),
            ("every 2h", "x", Some("a`b"), None, "job name"),
            ("every 2h", "x", Some("-x"), None, "job name"),
            ("every 2h", "x", None, Some("slack"), "--deliver"),
            ("whenever", "x", None, None, "cannot read the schedule"),
        ] {
            let err = add(schedule, prompt, name, deliver)
                .unwrap_err()
                .to_string();
            assert!(err.contains(wanted), "{schedule} {prompt}: {err}");
        }
        let err = new_job(
            &store,
            &tz,
            &settings,
            "every 2h".into(),
            "x".into(),
            None,
            Some(home.path().join("missing")),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("is not a folder"), "{err}");
        assert_eq!(store.jobs().unwrap().len(), 1);
    }
}
