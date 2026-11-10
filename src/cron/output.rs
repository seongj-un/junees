//! The record of each run: `$JUNEES_HOME/cron/output/<job id>/<time>.md`,
//! a short header and the final answer (or why there is none). Only the
//! newest [`KEEP`] files of a job are kept.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::store::{Job, RunStatus, create_private_dir};
use super::tz::TimeZone;

/// Output files kept per job.
pub const KEEP: usize = 100;

/// What one run left behind.
pub struct Record<'a> {
    pub job: &'a Job,
    pub status: RunStatus,
    pub due: i64,
    pub started_at: i64,
    pub finished_at: i64,
    pub note: &'a str,
    pub answer: &'a str,
    pub session: Option<&'a str>,
    pub tokens: u64,
}

/// The output folder of job `id`.
pub fn job_dir(home: &Path, id: &str) -> PathBuf {
    super::dir(home).join("output").join(id)
}

/// Writes `record` to a new file named after its start time (UTC) and
/// returns its path; then drops the oldest files beyond [`KEEP`].
pub fn write(home: &Path, tz: &TimeZone, record: &Record<'_>) -> Result<PathBuf> {
    let dir = job_dir(home, &record.job.id);
    create_private_dir(&dir)?;
    let stamp = file_stamp(record.started_at);
    let text = render(tz, record);
    for attempt in 1..100 {
        let name = if attempt == 1 {
            format!("{stamp}.md")
        } else {
            format!("{stamp}-{attempt}.md")
        };
        let path = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
                    .with_context(|| format!("cannot write {}", path.display()))?;
                prune(&dir);
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("cannot create {}", path.display()));
            }
        }
    }
    anyhow::bail!("too many runs of {} at {stamp}", record.job.id)
}

/// `20261110-090000Z`.
fn file_stamp(unix: i64) -> String {
    let (days, secs) = super::tz::split(unix);
    let (y, m, d) = super::tz::civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

fn render(tz: &TimeZone, record: &Record<'_>) -> String {
    let job = record.job;
    let mut out = format!("# junees cron: {}\n\n", job.label());
    out.push_str(&format!("- job: {}\n", job.id));
    out.push_str(&format!("- schedule: {}\n", job.schedule.describe(tz)));
    out.push_str(&format!("- folder: {}\n", job.dir.display()));
    out.push_str(&format!("- due: {}\n", tz.format(record.due)));
    out.push_str(&format!(
        "- ran: {} to {}\n",
        tz.format(record.started_at),
        tz.format(record.finished_at)
    ));
    out.push_str(&format!("- status: {}\n", record.status.word()));
    if !record.note.is_empty() {
        out.push_str(&format!("- note: {}\n", record.note));
    }
    if let Some(session) = record.session {
        out.push_str(&format!("- session: {session}\n"));
    }
    out.push_str(&format!("- tokens: {}\n", record.tokens));
    out.push_str(&format!("\n## Prompt\n\n{}\n", job.prompt.trim_end()));
    out.push_str("\n## Answer\n\n");
    if record.answer.trim().is_empty() {
        out.push_str("(none)\n");
    } else {
        out.push_str(record.answer.trim_end());
        out.push('\n');
    }
    out
}

/// Removes the oldest `.md` files of `dir` beyond [`KEEP`]; errors are
/// ignored (the next run tries again).
fn prune(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".md"))
        .collect();
    if names.len() <= KEEP {
        return;
    }
    // Names start with the time, so they sort by it.
    names.sort();
    for name in &names[..names.len() - KEEP] {
        let _ = fs::remove_file(dir.join(name));
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::cron::schedule::Schedule;
    use crate::cron::store::tests::job;

    #[test]
    fn writes_a_private_file_per_run_and_keeps_the_newest() {
        let home = tempfile::tempdir().unwrap();
        let job = job(
            "0123abcd",
            Schedule::Every {
                secs: 3600,
                anchor: 0,
            },
            None,
        );
        let tz = TimeZone::utc();
        let record = |started_at: i64| Record {
            job: &job,
            status: RunStatus::Ok,
            due: started_at,
            started_at,
            finished_at: started_at + 5,
            note: "",
            answer: "모든 것이 괜찮다",
            session: Some("20261110-090000-abcd"),
            tokens: 42,
        };
        let start = 1_794_301_200; // 2026-11-10 09:00:00Z
        let path = write(home.path(), &tz, &record(start)).unwrap();
        assert_eq!(
            path,
            job_dir(home.path(), "0123abcd").join("20261110-090000Z.md")
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# junees cron: 0123abcd\n"), "{text}");
        for line in [
            "- status: ok\n",
            "- session: 20261110-090000-abcd\n",
            "- tokens: 42\n",
            "## Prompt\n\nsay hi\n",
            "## Answer\n\n모든 것이 괜찮다\n",
            "- ran: 2026-11-10 09:00 +00:00 to 2026-11-10 09:00 +00:00\n",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        // The same second again gets a second file.
        let second = write(home.path(), &tz, &record(start)).unwrap();
        assert!(second.ends_with("20261110-090000Z-2.md"));
        for hour in 1..=KEEP as i64 {
            write(home.path(), &tz, &record(start + hour * 3600)).unwrap();
        }
        let count = fs::read_dir(job_dir(home.path(), "0123abcd"))
            .unwrap()
            .count();
        assert_eq!(count, KEEP);
        assert!(!path.exists() && !second.exists());
    }
}
