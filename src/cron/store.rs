//! `$JUNEES_HOME/cron/jobs.json`: the jobs and how their last run went.
//!
//! Every change happens under an exclusive `flock` on `cron/jobs.lock`
//! (the CLI, the daemon and a running job all write here), and lands with
//! a write to a temporary file and a rename, so a reader never sees half a
//! file. The file is private (0600) in a private folder (0700): it holds
//! prompts and folders. A file another user owns or may change is refused,
//! since its prompts would run with this user's tools.

use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::deliver::Delivery;
use super::schedule::Schedule;
use super::tz::TimeZone;

/// The format of jobs.json this build writes; newer ones are refused.
pub const FORMAT: u32 = 1;

/// How long a command waits for another holder of the lock.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// One scheduled job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// 8 hex digits.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub schedule: Schedule,
    /// The zone its local times are in: the `timezone` setting (or the
    /// system's zone) when it was added. `None` when that zone has no name
    /// to find it by again; the zone of the moment is used then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// The schedule as it was typed.
    pub text: String,
    pub prompt: String,
    /// The folder the job works in (absolute).
    pub dir: PathBuf,
    pub deliver: Delivery,
    #[serde(default)]
    pub paused: bool,
    /// Unix time.
    pub created_at: i64,
    /// When it runs next (unix time); `None` once a one-time job is over.
    pub next_run: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<LastRun>,
}

impl Job {
    /// The name, or the id.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// The zone of the job's schedule, else `fallback` (the zone of the
    /// setting now).
    pub fn zone(&self, fallback: &TimeZone) -> TimeZone {
        match &self.timezone {
            Some(name) if name != fallback.name() => {
                TimeZone::named(name).unwrap_or_else(|_| fallback.clone())
            }
            _ => fallback.clone(),
        }
    }
}

/// How the last run went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    /// Tells one run from another (the daemon checks a run it started
    /// recorded its end).
    pub run_id: String,
    /// The time it was due (unix), or when `cron run` started it.
    pub due: i64,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    pub status: RunStatus,
    /// One line: why it failed, or what else to know.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default)]
    pub tokens: u64,
    /// Why the result did not reach its destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
    /// A one-time job whose time passed while no daemon ran.
    Missed,
}

impl RunStatus {
    pub fn word(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Missed => "missed",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct File_ {
    format: u32,
    jobs: Vec<Job>,
}

/// The cron folder of a state folder.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// The jobs, read under the lock; [`Locked::save`] writes them back. The
/// lock is held until this is dropped.
pub struct Locked {
    _lock: File,
    path: PathBuf,
    pub jobs: Vec<Job>,
}

impl Store {
    /// The store in `$JUNEES_HOME/cron`.
    pub fn new(home: &Path) -> Self {
        Self {
            dir: super::dir(home),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("jobs.json")
    }

    /// Takes the lock (waiting up to 10 seconds for another holder) and
    /// reads the jobs.
    pub fn lock(&self) -> Result<Locked> {
        create_private_dir(&self.dir)?;
        let lock_path = self.dir.join("jobs.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("cannot open {}", lock_path.display()))?;
        let start = Instant::now();
        while !crate::flock::try_exclusive(&lock)
            .with_context(|| format!("cannot lock {}", lock_path.display()))?
        {
            if start.elapsed() > LOCK_WAIT {
                bail!(
                    "{} stays locked by another junees; try again",
                    lock_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let path = self.path();
        let jobs = read(&path)?;
        Ok(Locked {
            _lock: lock,
            path,
            jobs,
        })
    }

    /// The jobs as they are now (under the lock for a moment).
    pub fn jobs(&self) -> Result<Vec<Job>> {
        Ok(self.lock()?.jobs)
    }
}

impl Locked {
    /// Writes the jobs back (temporary file, then rename).
    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&File_ {
            format: FORMAT,
            jobs: self.jobs.clone(),
        })?;
        let dir = self.path.parent().context("jobs.json has no folder")?;
        let tmp = dir.join(".jobs.json.tmp");
        let write = || -> std::io::Result<()> {
            match fs::remove_file(&tmp) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
                _ => {}
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            File::open(dir)?.sync_all()
        };
        write().with_context(|| format!("cannot write {}", self.path.display()))
    }

    /// The index of the job `key` names: its id, a unique id prefix, or
    /// its name.
    pub fn find(&self, key: &str) -> Result<usize> {
        find(&self.jobs, key)
    }
}

/// See [`Locked::find`].
pub fn find(jobs: &[Job], key: &str) -> Result<usize> {
    let key = key.trim();
    if key.is_empty() {
        bail!("which job? give its id (`junees cron list` shows them)");
    }
    if let Some(index) = jobs.iter().position(|job| job.id == key) {
        return Ok(index);
    }
    let by_prefix: Vec<usize> = (0..jobs.len())
        .filter(|&index| jobs[index].id.starts_with(key))
        .collect();
    let by_name: Vec<usize> = (0..jobs.len())
        .filter(|&index| jobs[index].name.as_deref() == Some(key))
        .collect();
    match (by_prefix.as_slice(), by_name.as_slice()) {
        ([index], _) | ([], [index]) => Ok(*index),
        ([], []) => bail!("no job `{key}`; `junees cron list` shows them"),
        _ => bail!("`{key}` names more than one job; give more of the id"),
    }
}

/// Reads jobs.json; a missing file is no jobs.
fn read(path: &Path) -> Result<Vec<Job>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    check_private(path, &metadata)?;
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let file: File_ =
        serde_json::from_str(&text).with_context(|| format!("{} is broken", path.display()))?;
    if file.format > FORMAT {
        bail!(
            "{} was written by a newer junees (format {}); update junees",
            path.display(),
            file.format
        );
    }
    Ok(file.jobs)
}

/// A regular file of this user that nobody else may write.
fn check_private(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    // SAFETY: getuid only returns this process's user id.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        bail!(
            "{} belongs to another user; its jobs would run as you, so it is not read",
            path.display()
        );
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        bail!(
            "{} can be changed by other users (mode {mode:o}); run `chmod 600 {}`",
            path.display(),
            path.display()
        );
    }
    Ok(())
}

/// Creates `dir` and its parents as 0700 (existing ones stay as they are).
pub fn create_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// A new random id of `digits` hex digits.
pub fn random_hex(digits: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    let random = RandomState::new().hash_one((nanos, std::process::id()));
    format!("{random:016x}")[..digits.min(16)].to_string()
}

/// An id no job in `jobs` has.
pub fn new_id(jobs: &[Job]) -> String {
    loop {
        let id = random_hex(8);
        if !jobs.iter().any(|job| job.id == id) {
            return id;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn job(id: &str, schedule: Schedule, next_run: Option<i64>) -> Job {
        Job {
            id: id.to_string(),
            name: None,
            schedule,
            timezone: None,
            text: "every 1h".into(),
            prompt: "say hi".into(),
            dir: PathBuf::from("/"),
            deliver: Delivery::Local,
            paused: false,
            created_at: 0,
            next_run,
            last: None,
        }
    }

    #[test]
    fn saves_privately_and_reads_back() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        assert!(store.jobs().unwrap().is_empty());
        let mut locked = store.lock().unwrap();
        locked
            .jobs
            .push(job("0123abcd", Schedule::Once { at: 5 }, Some(5)));
        locked.save().unwrap();
        drop(locked);
        assert_eq!(store.jobs().unwrap()[0].id, "0123abcd");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store.path()), 0o600);
        assert_eq!(mode(store.dir()), 0o700);
        assert!(!store.dir().join(".jobs.json.tmp").exists());
    }

    #[test]
    fn the_lock_keeps_a_second_writer_out() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let held = store.lock().unwrap();
        let lock = File::open(store.dir().join("jobs.lock")).unwrap();
        assert!(!crate::flock::try_exclusive(&lock).unwrap());
        drop(held);
        assert!(crate::flock::try_exclusive(&lock).unwrap());
        drop(lock);
        // A writer waits for the holder instead of failing.
        let held = store.lock().unwrap();
        let other = store.clone();
        let waiter = std::thread::spawn(move || {
            let mut locked = other.lock().unwrap();
            locked
                .jobs
                .push(job("bbbbbbbb", Schedule::Once { at: 1 }, None));
            locked.save().unwrap();
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(!waiter.is_finished());
        let mut first = held;
        first
            .jobs
            .push(job("aaaaaaaa", Schedule::Once { at: 1 }, None));
        first.save().unwrap();
        drop(first);
        waiter.join().unwrap();
        let ids: Vec<String> = store
            .jobs()
            .unwrap()
            .into_iter()
            .map(|job| job.id)
            .collect();
        assert_eq!(ids, ["aaaaaaaa", "bbbbbbbb"]);
    }

    #[test]
    fn refuses_files_others_may_change_and_newer_formats() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let mut locked = store.lock().unwrap();
        locked
            .jobs
            .push(job("0123abcd", Schedule::Once { at: 5 }, Some(5)));
        locked.save().unwrap();
        drop(locked);
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o622)).unwrap();
        let err = store.jobs().unwrap_err().to_string();
        assert!(err.contains("mode 622"), "{err}");
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
        store.jobs().unwrap();
        fs::write(store.path(), r#"{"format": 9, "jobs": []}"#).unwrap();
        let err = store.jobs().unwrap_err().to_string();
        assert!(err.contains("newer junees"), "{err}");
        fs::write(store.path(), "{").unwrap();
        let err = store.jobs().unwrap_err().to_string();
        assert!(err.contains("is broken"), "{err}");
        fs::remove_file(store.path()).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", store.path()).unwrap();
        let err = store.jobs().unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn finds_jobs_by_id_prefix_or_name() {
        let mut jobs = vec![
            job("0123abcd", Schedule::Once { at: 1 }, None),
            job("0199ffff", Schedule::Once { at: 1 }, None),
        ];
        jobs[1].name = Some("news".into());
        assert_eq!(find(&jobs, "0123abcd").unwrap(), 0);
        assert_eq!(find(&jobs, "0123").unwrap(), 0);
        assert_eq!(find(&jobs, "news").unwrap(), 1);
        assert!(
            find(&jobs, "01")
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
        assert!(
            find(&jobs, "zz")
                .unwrap_err()
                .to_string()
                .contains("no job")
        );
        assert!(find(&jobs, " ").is_err());
        let id = new_id(&jobs);
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
