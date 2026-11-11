//! `junees cron daemon`: wakes at the start of every minute, starts the
//! jobs that are due, each as a process of its own (`junees cron run <id>`
//! in the job's folder), and records runs that never reported back.
//!
//! A repeating job the daemon was not running for runs once when it comes
//! back (not once per missed time); a one-time job more than
//! [`MISSED_AFTER`] late is skipped and recorded as missed. Only one daemon
//! runs per state folder (a lock on `cron/daemon.lock`).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc::{self, UnboundedSender};

use super::deliver::Lookup;
use super::run::{Limits, Outcome};
use super::store::{Job, RunStatus, Store, create_private_dir, random_hex};
use super::tz::TimeZone;
use crate::cancel::Cancel;
use crate::gateway::Log;

/// A one-time job found later than this is not run.
pub const MISSED_AFTER: i64 = 10 * 60;

/// A run that starts later than this after its time is a catch-up.
pub const LATE_AFTER: i64 = 2 * 60;

/// How long past its time limit a run may go before it is stopped from
/// outside (SIGINT to its process group, then SIGKILL after [`GRACE`]).
pub const KILL_AFTER: Duration = Duration::from_secs(60);

/// How long stopped runs get to finish before SIGKILL.
pub const GRACE: Duration = Duration::from_secs(30);

/// The lock file of a running daemon, in the cron folder.
pub const LOCK_FILE: &str = "daemon.lock";

/// The time, for the daemon; tests move a fake one.
#[async_trait]
pub trait Clock: Send + Sync {
    /// Unix time in seconds.
    fn now(&self) -> i64;
    /// Returns at unix time `t` (at once if it has passed).
    async fn sleep_until(&self, t: i64);
}

pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        super::unix_now()
    }

    async fn sleep_until(&self, t: i64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let target = Duration::from_secs(t.max(0) as u64);
        tokio::time::sleep(target.saturating_sub(now)).await;
    }
}

/// A run the daemon started has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub job_id: String,
    pub run_id: String,
    pub due: i64,
    /// How the process ended (`exit status: 0`).
    pub exit: String,
}

/// Starts runs.
pub trait Launcher: Send {
    /// Starts run `run_id` of `job`, due at `due`; reports its end on `done`.
    fn launch(
        &mut self,
        job: &Job,
        run_id: &str,
        due: i64,
        done: UnboundedSender<Finished>,
    ) -> Result<()>;
    /// Sends `signal` to every run still going.
    fn signal_all(&mut self, signal: i32);
}

/// What a tick found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// Start it now; `late` when it is a catch-up.
    Run { job: Job, due: i64, late: bool },
    /// A one-time job whose time passed long ago.
    Missed { job: Job, due: i64 },
    /// A repeating job still running its previous run: this time is skipped.
    Busy { job: Job, due: i64 },
}

/// The daemon of one state folder.
pub struct Daemon {
    home: PathBuf,
    store: Store,
    tz: TimeZone,
    clock: Arc<dyn Clock>,
    launcher: Box<dyn Launcher>,
    lookup: Lookup,
    log: Log,
    /// Job id -> run id of the runs going on.
    running: HashMap<String, String>,
}

impl Daemon {
    pub fn new(
        home: &Path,
        tz: TimeZone,
        clock: Arc<dyn Clock>,
        launcher: Box<dyn Launcher>,
        lookup: Lookup,
        log: Log,
    ) -> Self {
        Self {
            home: home.to_path_buf(),
            store: Store::new(home),
            tz,
            clock,
            launcher,
            lookup,
            log,
            running: HashMap::new(),
        }
    }

    /// Finds the jobs due at `now` and moves each one's next run on, before
    /// anything starts: a crash while starting cannot run a job twice.
    pub fn tick(&mut self, now: i64) -> Result<Vec<Due>> {
        let mut locked = self.store.lock()?;
        let mut found = Vec::new();
        for job in &mut locked.jobs {
            let Some(due) = job.next_run.filter(|&due| due <= now && !job.paused) else {
                continue;
            };
            job.next_run = job.schedule.next_after(now, &job.zone(&self.tz));
            if !job.schedule.repeats() {
                job.next_run = None;
                if now - due > MISSED_AFTER {
                    found.push(Due::Missed {
                        job: job.clone(),
                        due,
                    });
                    continue;
                }
            }
            if self.running.contains_key(&job.id) {
                found.push(Due::Busy {
                    job: job.clone(),
                    due,
                });
                continue;
            }
            found.push(Due::Run {
                job: job.clone(),
                due,
                late: now - due > LATE_AFTER,
            });
        }
        if !found.is_empty() {
            locked.save()?;
        }
        Ok(found)
    }

    /// Acts on what a tick found.
    async fn start(&mut self, due: Due, done: &UnboundedSender<Finished>) {
        match due {
            Due::Run { job, due, late } => {
                let run_id = random_hex(8);
                let when = job.zone(&self.tz).format(due);
                let catch_up = if late { ", catching up" } else { "" };
                match self.launcher.launch(&job, &run_id, due, done.clone()) {
                    Ok(()) => {
                        (self.log)(&format!("job {} started (due {when}{catch_up})", job.id));
                        self.running.insert(job.id.clone(), run_id);
                    }
                    Err(err) => {
                        let note = format!("cannot start the run: {err:#}");
                        (self.log)(&format!("job {}: {note}", job.id));
                        self.conclude(&job, &run_id, due, RunStatus::Failed, note)
                            .await;
                    }
                }
            }
            Due::Missed { job, due } => {
                let note = format!(
                    "missed: due at {}, when no daemon was running; not run",
                    job.zone(&self.tz).format(due)
                );
                (self.log)(&format!("job {} {note}", job.id));
                self.conclude(&job, &random_hex(8), due, RunStatus::Missed, note)
                    .await;
            }
            Due::Busy { job, due } => {
                (self.log)(&format!(
                    "job {}: the run due at {} is skipped, the previous one is still going",
                    job.id,
                    job.zone(&self.tz).format(due)
                ));
            }
        }
    }

    /// A run ended: if it did not record its end (it crashed, was killed,
    /// or failed before it started), record a failure for it.
    async fn finished(&mut self, finished: Finished) {
        if self.running.get(&finished.job_id) == Some(&finished.run_id) {
            self.running.remove(&finished.job_id);
        }
        let job = match self.store.jobs() {
            Ok(jobs) => jobs.into_iter().find(|job| job.id == finished.job_id),
            Err(err) => {
                (self.log)(&format!("cannot read the jobs: {err:#}"));
                return;
            }
        };
        let Some(job) = job else {
            (self.log)(&format!(
                "job {} ended ({}); it was removed",
                finished.job_id, finished.exit
            ));
            return;
        };
        let last = job
            .last
            .as_ref()
            .filter(|last| last.run_id == finished.run_id);
        match last {
            Some(last) if last.status != RunStatus::Running => {
                (self.log)(&format!("job {} ended: {}", job.id, last.status.word()));
            }
            _ => {
                let note = format!("the run ended without a result ({})", finished.exit);
                (self.log)(&format!("job {}: {note}", job.id));
                let session = last.and_then(|last| last.session.clone());
                let outcome = Outcome {
                    session,
                    ..Outcome::failed(note)
                };
                let started = last.map_or(finished.due, |last| last.started_at);
                super::finish(
                    &self.home,
                    &self.tz,
                    &self.lookup,
                    &job,
                    &finished.run_id,
                    finished.due,
                    started,
                    &outcome,
                )
                .await;
            }
        }
    }

    async fn conclude(&self, job: &Job, run_id: &str, due: i64, status: RunStatus, note: String) {
        let now = self.clock.now();
        let outcome = Outcome {
            status,
            ..Outcome::failed(note)
        };
        super::finish(
            &self.home,
            &self.tz,
            &self.lookup,
            job,
            run_id,
            due,
            now,
            &outcome,
        )
        .await;
    }

    /// Runs until `cancel`: a tick at the start of every minute. Then
    /// stops the runs going on (SIGINT, which they record as interrupted)
    /// and waits for them up to [`GRACE`].
    pub async fn run(mut self, cancel: Cancel) -> Result<()> {
        let (done, mut ended) = mpsc::unbounded_channel();
        loop {
            let now = self.clock.now();
            match self.tick(now) {
                Ok(found) => {
                    for due in found {
                        self.start(due, &done).await;
                    }
                }
                // A broken jobs.json is fixed by hand; keep going meanwhile.
                Err(err) => (self.log)(&format!("cannot check the jobs: {err:#}")),
            }
            let next = (now.div_euclid(60) + 1) * 60;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.stop(&mut ended).await;
                        return Ok(());
                    }
                    Some(finished) = ended.recv() => self.finished(finished).await,
                    _ = self.clock.sleep_until(next) => break,
                }
            }
        }
    }

    async fn stop(&mut self, ended: &mut mpsc::UnboundedReceiver<Finished>) {
        if self.running.is_empty() {
            return;
        }
        (self.log)(&format!("stopping {} running job(s)", self.running.len()));
        self.launcher.signal_all(libc::SIGINT);
        let deadline = tokio::time::Instant::now() + GRACE;
        while !self.running.is_empty() {
            match tokio::time::timeout_at(deadline, ended.recv()).await {
                Ok(Some(finished)) => self.finished(finished).await,
                _ => break,
            }
        }
        if !self.running.is_empty() {
            self.launcher.signal_all(libc::SIGKILL);
        }
    }
}

/// Takes the daemon lock of `home` (held while the file stays open).
pub fn lock(home: &Path) -> Result<File> {
    let dir = super::dir(home);
    create_private_dir(&dir)?;
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    if !crate::flock::try_exclusive(&file)
        .with_context(|| format!("cannot lock {}", path.display()))?
    {
        bail!(
            "another `junees cron daemon` is running for {}",
            home.display()
        );
    }
    Ok(file)
}

/// Whether a daemon holds the lock of `home` now.
pub fn is_running(home: &Path) -> bool {
    let path = super::dir(home).join(LOCK_FILE);
    let Ok(file) = File::open(path) else {
        return false;
    };
    // A lock let go a moment ago can still be held by a child forked
    // in between (until it execs), so look with the usual retries; a
    // lock taken here is released when `file` is dropped.
    !crate::flock::try_exclusive(&file).unwrap_or(false)
}

/// Starts each run as `<program> cron run <id> --run-id <run> --due <t>`
/// in a process group of its own, stopping it from outside when it goes
/// past its time limit by [`KILL_AFTER`].
pub struct ProcessLauncher {
    program: PathBuf,
    limits: Limits,
    /// Run id -> process group of the runs going on.
    groups: Arc<Mutex<HashMap<String, i32>>>,
}

impl ProcessLauncher {
    pub fn new(program: PathBuf, limits: Limits) -> Self {
        Self {
            program,
            limits,
            groups: Arc::default(),
        }
    }
}

/// Sends `signal` to the process group `pid` leads.
pub fn signal_group(pid: i32, signal: i32) {
    // SAFETY: kill only sends a signal; a negative pid names the group the
    // run's process leads.
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Waits for `child`, the leader of process group `pid`; after `limit`
/// stops the group (SIGINT, then SIGKILL after [`GRACE`]). Returns how it
/// ended (`exit status: 0`).
pub async fn wait_limited(child: &mut tokio::process::Child, pid: i32, limit: Duration) -> String {
    let status = match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            signal_group(pid, libc::SIGINT);
            match tokio::time::timeout(GRACE, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    signal_group(pid, libc::SIGKILL);
                    child.wait().await
                }
            }
        }
    };
    match status {
        Ok(status) => status.to_string(),
        Err(err) => format!("cannot wait for it: {err}"),
    }
}

impl Launcher for ProcessLauncher {
    fn launch(
        &mut self,
        job: &Job,
        run_id: &str,
        due: i64,
        done: UnboundedSender<Finished>,
    ) -> Result<()> {
        let mut child = tokio::process::Command::new(&self.program)
            .args([
                "cron",
                "run",
                &job.id,
                "--run-id",
                run_id,
                "--due",
                &due.to_string(),
            ])
            .current_dir("/")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("cannot start {}", self.program.display()))?;
        let pid = child.id().context("the run ended at once")? as i32;
        self.groups.lock().unwrap().insert(run_id.to_string(), pid);
        let groups = Arc::clone(&self.groups);
        let limit = self.limits.max_time + KILL_AFTER;
        let finished = Finished {
            job_id: job.id.clone(),
            run_id: run_id.to_string(),
            due,
            exit: String::new(),
        };
        tokio::spawn(async move {
            let exit = wait_limited(&mut child, pid, limit).await;
            groups.lock().unwrap().remove(&finished.run_id);
            let _ = done.send(Finished { exit, ..finished });
        });
        Ok(())
    }

    fn signal_all(&mut self, signal: i32) {
        for &pid in self.groups.lock().unwrap().values() {
            signal_group(pid, signal);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;
    use crate::cron::deliver::tests::lookup;
    use crate::cron::schedule::Schedule;
    use crate::cron::store::LastRun;
    use crate::cron::store::tests::job;

    struct FakeClock(AtomicI64);

    #[async_trait]
    impl Clock for FakeClock {
        fn now(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }

        async fn sleep_until(&self, t: i64) {
            self.0.fetch_max(t, Ordering::SeqCst);
            tokio::task::yield_now().await;
        }
    }

    /// Records launches; a run "records its end" in the store like
    /// `cron run` does unless `crash`, then reports it ended.
    #[derive(Clone)]
    struct FakeLauncher {
        home: PathBuf,
        clock: Arc<FakeClock>,
        launched: Arc<Mutex<Vec<(String, i64, i64)>>>,
        crash: bool,
        /// Stops the daemon after this many launches.
        stop_after: usize,
        cancel: Cancel,
    }

    impl Launcher for FakeLauncher {
        fn launch(
            &mut self,
            job: &Job,
            run_id: &str,
            due: i64,
            done: UnboundedSender<Finished>,
        ) -> Result<()> {
            let now = self.clock.now();
            let mut launched = self.launched.lock().unwrap();
            launched.push((job.id.clone(), due, now));
            if launched.len() >= self.stop_after {
                self.cancel.cancel();
            }
            let store = Store::new(&self.home);
            let mut locked = store.lock()?;
            let index = locked.find(&job.id)?;
            locked.jobs[index].last = Some(LastRun {
                run_id: run_id.to_string(),
                due,
                started_at: now,
                finished_at: (!self.crash).then_some(now),
                status: if self.crash {
                    RunStatus::Running
                } else {
                    RunStatus::Ok
                },
                note: String::new(),
                output: None,
                session: Some("s1".into()),
                tokens: 0,
                delivery_error: None,
            });
            locked.save()?;
            done.send(Finished {
                job_id: job.id.clone(),
                run_id: run_id.to_string(),
                due,
                exit: if self.crash {
                    "signal: 9 (SIGKILL)"
                } else {
                    "exit status: 0"
                }
                .into(),
            })?;
            Ok(())
        }

        fn signal_all(&mut self, _signal: i32) {}
    }

    const T0: i64 = 1_794_301_200; // 2026-11-10 09:00:00Z, a minute start

    fn daemon(
        home: &Path,
        now: i64,
        crash: bool,
        stop_after: usize,
    ) -> (Daemon, FakeLauncher, Cancel) {
        let clock = Arc::new(FakeClock(AtomicI64::new(now)));
        let cancel = Cancel::new();
        let launcher = FakeLauncher {
            home: home.to_path_buf(),
            clock: Arc::clone(&clock),
            launched: Arc::default(),
            crash,
            stop_after,
            cancel: cancel.clone(),
        };
        let daemon = Daemon::new(
            home,
            TimeZone::utc(),
            clock,
            Box::new(launcher.clone()),
            lookup(&[]),
            Arc::new(|_: &str| {}),
        );
        (daemon, launcher, cancel)
    }

    fn save(home: &Path, jobs: Vec<Job>) {
        let mut locked = Store::new(home).lock().unwrap();
        locked.jobs = jobs;
        locked.save().unwrap();
    }

    fn stored(home: &Path, id: &str) -> Job {
        Store::new(home)
            .jobs()
            .unwrap()
            .into_iter()
            .find(|job| job.id == id)
            .unwrap()
    }

    #[test]
    fn a_repeating_job_catches_up_once_and_a_late_one_time_job_is_missed() {
        let home = tempfile::tempdir().unwrap();
        let hourly = Schedule::Every {
            secs: 3600,
            anchor: T0 - 10 * 3600,
        };
        let mut paused = job("cccccccc", hourly.clone(), Some(T0 - 3600));
        paused.paused = true;
        save(
            home.path(),
            vec![
                // Due 3 hours ago: the daemon was down.
                job("aaaaaaaa", hourly.clone(), Some(T0 - 3 * 3600)),
                job(
                    "bbbbbbbb",
                    Schedule::Once { at: T0 - 3600 },
                    Some(T0 - 3600),
                ),
                paused,
                job("dddddddd", Schedule::Once { at: T0 - 60 }, Some(T0 - 60)),
                job("eeeeeeee", hourly, Some(T0 + 60)),
            ],
        );
        let (mut daemon, _, _) = daemon(home.path(), T0 + 30, false, usize::MAX);
        let found = daemon.tick(T0 + 30).unwrap();
        let summary: Vec<(String, &str, i64)> = found
            .iter()
            .map(|due| match due {
                Due::Run { job, due, late } => {
                    (job.id.clone(), if *late { "late" } else { "run" }, *due)
                }
                Due::Missed { job, due } => (job.id.clone(), "missed", *due),
                Due::Busy { job, due } => (job.id.clone(), "busy", *due),
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("aaaaaaaa".to_string(), "late", T0 - 3 * 3600),
                ("bbbbbbbb".to_string(), "missed", T0 - 3600),
                ("dddddddd".to_string(), "run", T0 - 60),
            ]
        );
        // Moved on before anything ran: only the next time is left.
        assert_eq!(stored(home.path(), "aaaaaaaa").next_run, Some(T0 + 3600));
        assert_eq!(stored(home.path(), "bbbbbbbb").next_run, None);
        assert_eq!(stored(home.path(), "dddddddd").next_run, None);
        assert_eq!(stored(home.path(), "cccccccc").next_run, Some(T0 - 3600));
        assert_eq!(stored(home.path(), "eeeeeeee").next_run, Some(T0 + 60));
        // Nothing is due twice.
        assert!(daemon.tick(T0 + 31).unwrap().is_empty());
        // A job still running skips its time.
        daemon.running.insert("eeeeeeee".into(), "r".into());
        let found = daemon.tick(T0 + 61).unwrap();
        assert!(matches!(&found[..], [Due::Busy { job, .. }] if job.id == "eeeeeeee"));
    }

    #[tokio::test]
    async fn the_loop_ticks_every_minute_on_a_fake_clock() {
        let home = tempfile::tempdir().unwrap();
        let every = Schedule::Every {
            secs: 120,
            anchor: T0,
        };
        let daily = Schedule::Daily { hour: 9, minute: 5 };
        save(
            home.path(),
            vec![
                job("aaaaaaaa", every, Some(T0 + 120)),
                job("bbbbbbbb", daily, Some(T0 + 300)),
                job("cccccccc", Schedule::Once { at: T0 + 90 }, Some(T0 + 90)),
            ],
        );
        let (daemon, launcher, cancel) = daemon(home.path(), T0 + 7, false, 5);
        tokio::time::timeout(Duration::from_secs(10), daemon.run(cancel))
            .await
            .unwrap()
            .unwrap();
        let launched = launcher.launched.lock().unwrap().clone();
        assert_eq!(
            launched,
            [
                ("aaaaaaaa".to_string(), T0 + 120, T0 + 120),
                ("cccccccc".to_string(), T0 + 90, T0 + 120),
                ("aaaaaaaa".to_string(), T0 + 240, T0 + 240),
                ("bbbbbbbb".to_string(), T0 + 300, T0 + 300),
                ("aaaaaaaa".to_string(), T0 + 360, T0 + 360),
            ]
        );
        let last = stored(home.path(), "aaaaaaaa").last.unwrap();
        assert_eq!((last.status, last.due), (RunStatus::Ok, T0 + 360));
    }

    #[tokio::test]
    async fn runs_that_end_without_a_result_and_missed_jobs_are_recorded() {
        let home = tempfile::tempdir().unwrap();
        save(
            home.path(),
            vec![
                job("aaaaaaaa", Schedule::Once { at: T0 }, Some(T0)),
                job(
                    "bbbbbbbb",
                    Schedule::Once { at: T0 - 3600 },
                    Some(T0 - 3600),
                ),
            ],
        );
        let (daemon, _, cancel) = daemon(home.path(), T0, true, 1);
        tokio::time::timeout(Duration::from_secs(10), daemon.run(cancel))
            .await
            .unwrap()
            .unwrap();
        let crashed = stored(home.path(), "aaaaaaaa").last.unwrap();
        assert_eq!(crashed.status, RunStatus::Failed);
        assert_eq!(
            crashed.note,
            "the run ended without a result (signal: 9 (SIGKILL))"
        );
        assert_eq!(crashed.session.as_deref(), Some("s1"));
        let text = std::fs::read_to_string(crashed.output.unwrap()).unwrap();
        assert!(text.contains("- status: failed\n"), "{text}");
        let missed = stored(home.path(), "bbbbbbbb").last.unwrap();
        assert_eq!(missed.status, RunStatus::Missed);
        assert!(
            missed
                .note
                .starts_with("missed: due at 2026-11-10 08:00 +00:00"),
            "{}",
            missed.note
        );
        assert!(missed.output.unwrap().exists());
    }

    #[test]
    fn jobs_keep_the_zone_they_were_added_in() {
        let home = tempfile::tempdir().unwrap();
        let mut nine = job("aaaaaaaa", Schedule::Daily { hour: 9, minute: 0 }, Some(T0));
        nine.timezone = Some("EST5EDT,M3.2.0,M11.1.0".into());
        save(home.path(), vec![nine]);
        // The daemon's own zone is UTC.
        let (mut daemon, _, _) = daemon(home.path(), T0, false, usize::MAX);
        assert_eq!(daemon.tick(T0).unwrap().len(), 1);
        // 09:00 in New York in November is 14:00Z.
        assert_eq!(
            stored(home.path(), "aaaaaaaa").next_run,
            Some(T0 + 5 * 3600)
        );
    }

    #[test]
    fn one_daemon_per_state_folder() {
        let home = tempfile::tempdir().unwrap();
        assert!(!is_running(home.path()));
        let held = lock(home.path()).unwrap();
        assert!(is_running(home.path()));
        let err = lock(home.path()).unwrap_err().to_string();
        assert!(err.contains("another `junees cron daemon`"), "{err}");
        drop(held);
        assert!(!is_running(home.path()));
    }
}
