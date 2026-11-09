//! `junees task status`: where a task stands, read from `.junees/`.
//!
//! Status only reads files and never writes, so it is safe to run while a
//! `task run` works on the same repository. It does not take the run lock
//! either (a `task run` starting at that moment would be refused): whether
//! a run is going comes from the running mark in state.json and whether
//! the process it names still exists.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::state::{Running, State, StopReason};
use super::{dir, features, progress_path};
use crate::cli::preview;
use crate::message::Usage;
use crate::session::format_utc;

/// Lines of progress.md the status shows, from its end.
pub const PROGRESS_LINES: usize = 5;

/// Characters of a feature description (its first line) in the text output.
const DESCRIPTION_CHARS: usize = 72;

/// Characters of the goal and of a stop's error in the text output.
const LONG_TEXT_CHARS: usize = 200;

/// Width of the text progress bar.
const BAR_WIDTH: usize = 20;

/// Where a task stands. Serialized as is for `task status --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// Name of the repository folder.
    pub repo: String,
    /// The repository root.
    pub root: String,
    /// The goal `task init` planned for; `None` for tasks planned before
    /// the goal was recorded.
    pub goal: Option<String>,
    pub passing: usize,
    pub total: usize,
    pub features: Vec<FeatureStatus>,
    /// Where `features` come from: `features.json`, or `harness` (the copy
    /// in state.json) while a run is marked running. A session can edit
    /// features.json; the harness puts its own copy back when the session
    /// ends.
    pub features_from: &'static str,
    /// Work sessions started so far, over every run.
    pub sessions: u64,
    /// Work sessions in a row without a newly passing feature.
    pub stalled: u32,
    pub tokens: Tokens,
    /// Why the last run stopped; `None` before the first run, while a run
    /// is going, or after a run that ended without recording a stop.
    pub stop: Option<StopStatus>,
    /// Whether a `task run` is working on the task right now.
    pub running: bool,
    /// The run state.json marks as running, if any. With `alive` false its
    /// process is gone: it crashed, and the next `task run` recovers.
    pub run: Option<RunStatus>,
    /// The last [`PROGRESS_LINES`] non-empty lines of progress.md.
    pub progress_tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeatureStatus {
    pub id: String,
    pub passes: bool,
    pub description: String,
}

/// Tokens used by every task session, the init session included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl From<Usage> for Tokens {
    fn from(usage: Usage) -> Self {
        Self {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_write: usage.cache_creation_input_tokens,
            cache_read: usage.cache_read_input_tokens,
        }
    }
}

/// A run marked running in state.json.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RunStatus {
    pub pid: u32,
    /// When it started, in unix seconds.
    pub started: u64,
    /// Whether a process with that pid exists. A reused pid can make a
    /// crashed run look alive, which `task run` itself does not trust: it
    /// goes by the run lock.
    pub alive: bool,
}

impl RunStatus {
    fn of(running: &Running) -> Self {
        Self {
            pid: running.pid,
            started: running.started,
            alive: process_exists(running.pid),
        }
    }
}

/// Whether a process with this pid exists, by sending it signal 0.
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; kill only checks the pid.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // It exists but belongs to someone else.
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StopStatus {
    pub reason: StopReason,
    /// The last session that ran.
    pub session: u64,
    /// The error, for `error` stops; which budget ran out, for `budget`.
    pub detail: Option<String>,
    /// The stop as one sentence.
    pub message: String,
    /// When the run stopped, in unix seconds, if recorded.
    pub at: Option<u64>,
}

/// Reads the status of the task in the repository at `root`.
pub fn load(root: &Path) -> Result<Status> {
    if !dir(root).is_dir() {
        bail!(
            "{} has no task (no .junees/ folder); start one with `junees task init \"<goal>\"`",
            root.display()
        );
    }
    let list = features::load(root)
        .context("cannot read the feature list (did `junees task init` finish?)")?;
    let state = State::load(root)?;
    let progress_tail = progress_tail(root)?;
    let run = state.running.as_ref().map(RunStatus::of);
    // While a run is marked, features.json may hold a session's own edits.
    let (list, features_from) = match (&run, state.features) {
        (Some(_), Some(copy)) => (copy, "harness"),
        _ => (list, "features.json"),
    };

    let features: Vec<FeatureStatus> = list
        .into_iter()
        .map(|feature| FeatureStatus {
            id: feature.id,
            passes: feature.passes,
            description: feature.description,
        })
        .collect();
    let stop = state.stop.map(|stop| StopStatus {
        reason: stop.reason,
        session: stop.session,
        message: stop.to_string(),
        detail: stop.detail,
        at: state.stopped_at,
    });
    Ok(Status {
        repo: root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        root: root.display().to_string(),
        goal: state.goal,
        passing: features.iter().filter(|feature| feature.passes).count(),
        total: features.len(),
        features,
        features_from,
        sessions: state.sessions,
        stalled: state.stalled,
        tokens: state.usage.into(),
        stop,
        running: run.is_some_and(|run| run.alive),
        run,
        progress_tail,
    })
}

/// The last [`PROGRESS_LINES`] non-empty lines of progress.md; none if
/// there is no file.
fn progress_tail(root: &Path) -> Result<Vec<String>> {
    let path = progress_path(root);
    let data = match fs::read(&path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    let start = lines.len().saturating_sub(PROGRESS_LINES);
    Ok(lines[start..].iter().map(|line| line.to_string()).collect())
}

impl Status {
    /// The status for people.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "task: {} ({})", self.repo, self.root);
        if let Some(goal) = &self.goal {
            let _ = writeln!(out, "goal: {}", preview(goal, LONG_TEXT_CHARS));
        }
        let source = if self.features_from == "harness" {
            " (as the harness last saved them; the session in progress may have passed more)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "features: {}/{} passing {}{source}",
            self.passing,
            self.total,
            bar(self.passing, self.total)
        );
        let width = self
            .features
            .iter()
            .map(|feature| feature.id.chars().count())
            .max()
            .unwrap_or(0);
        for feature in &self.features {
            let mark = if feature.passes { 'x' } else { ' ' };
            let first_line = feature.description.lines().next().unwrap_or("");
            let _ = writeln!(
                out,
                "  [{mark}] {:width$}  {}",
                feature.id,
                preview(first_line, DESCRIPTION_CHARS)
            );
        }
        let _ = writeln!(
            out,
            "sessions: {} ({} in a row without a newly passing feature)",
            self.sessions, self.stalled
        );
        let tokens = self.tokens;
        let _ = writeln!(
            out,
            "tokens: input {}, output {}, cache write {}, cache read {}",
            tokens.input, tokens.output, tokens.cache_write, tokens.cache_read
        );
        let _ = writeln!(out, "last stop: {}", self.stop_line());
        let _ = writeln!(out, "running: {}", self.running_line());
        if !self.progress_tail.is_empty() {
            let _ = writeln!(out, "progress.md (last {PROGRESS_LINES} lines):");
            for line in &self.progress_tail {
                let _ = writeln!(out, "  {line}");
            }
        }
        out
    }

    /// The status for scripts and bots.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("status always serializes")
    }

    fn stop_line(&self) -> String {
        match &self.stop {
            Some(stop) => {
                let when = stop
                    .at
                    .map(|at| format!(", {} UTC", format_utc(at)))
                    .unwrap_or_default();
                format!(
                    "{} (session {}{when})",
                    preview(&stop.message, LONG_TEXT_CHARS),
                    stop.session
                )
            }
            None if self.running => "none yet, a run is going".to_string(),
            None if self.run.is_some() => "none: the last run did not stop cleanly".to_string(),
            None if self.sessions == 0 => "none, no run yet".to_string(),
            None => "none recorded: a run is going, or the last one ended \
                     without stopping cleanly"
                .to_string(),
        }
    }

    fn running_line(&self) -> String {
        match self.run {
            None => "no".to_string(),
            Some(run) if run.alive => {
                format!("yes, pid {} since {} UTC", run.pid, format_utc(run.started))
            }
            Some(run) => format!(
                "no; the run with pid {} (started {} UTC) did not stop cleanly, \
                 and the next `junees task run` recovers from it",
                run.pid,
                format_utc(run.started)
            ),
        }
    }
}

/// `[########............] 40%`
fn bar(passing: usize, total: usize) -> String {
    let filled = (passing * BAR_WIDTH).checked_div(total).unwrap_or(0);
    let percent = (passing * 100).checked_div(total).unwrap_or(0);
    format!(
        "[{}{}] {percent}%",
        "#".repeat(filled),
        ".".repeat(BAR_WIDTH - filled)
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use serde_json::{Value, json};

    use super::super::state::Stop;
    use super::super::{features_path, state_path};
    use super::*;
    use crate::task::git::testing::repo;

    /// A repository with a task: two of three features pass, three
    /// sessions ran and the last run stopped at --max-sessions.
    fn task_repo() -> tempfile::TempDir {
        let repo = repo();
        let root = repo.path();
        fs::create_dir_all(dir(root)).unwrap();
        let list = json!([
            {"id": "F1", "description": "서버가 8080에서 응답한다\n자세한 설명", "verify": "true", "passes": true},
            {"id": "F10", "description": "a ".repeat(60), "verify": "true", "passes": true},
            {"id": "F2", "description": "logs requests", "verify": "false", "passes": false, "steps": ["x"]}
        ]);
        fs::write(features_path(root), list.to_string()).unwrap();
        let mut state = State {
            sessions: 3,
            stalled: 1,
            usage: Usage {
                input_tokens: 1200,
                output_tokens: 340,
                cache_creation_input_tokens: 50,
                cache_read_input_tokens: 900,
                ..Default::default()
            },
            goal: Some("웹 서버를 만들어".into()),
            ..State::default()
        };
        state.record_stop(Stop {
            reason: StopReason::MaxSessions,
            session: 3,
            detail: None,
        });
        state.stopped_at = Some(1_794_205_024);
        state.save(root).unwrap();
        let progress = (1..=7).map(|n| format!("- line {n}\n")).collect::<String>();
        fs::write(progress_path(root), format!("# Plan\n{progress}\n\n")).unwrap();
        repo
    }

    /// Every file under `root` (`.git` included) with its content and
    /// modification time.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
        let mut files = BTreeMap::new();
        let mut folders = vec![root.to_path_buf()];
        while let Some(folder) = folders.pop() {
            for entry in fs::read_dir(&folder).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    folders.push(path.clone());
                }
                let content = if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                files.insert(path, (content, meta.modified().unwrap()));
            }
        }
        files
    }

    #[test]
    fn shows_the_task_as_text() {
        let repo = task_repo();
        let root = repo.path();
        let text = load(root).unwrap().to_text();
        let name = root.file_name().unwrap().to_string_lossy();
        let expected = format!(
            "task: {name} ({})\n\
             goal: 웹 서버를 만들어\n\
             features: 2/3 passing [#############.......] 66%\n\
             \x20 [x] F1   서버가 8080에서 응답한다\n\
             \x20 [x] F10  {}...\n\
             \x20 [ ] F2   logs requests\n\
             sessions: 3 (1 in a row without a newly passing feature)\n\
             tokens: input 1200, output 340, cache write 50, cache read 900\n\
             last stop: reached --max-sessions (session 3, 2026-11-09 06:17 UTC)\n\
             running: no\n\
             progress.md (last 5 lines):\n\
             \x20 - line 3\n\
             \x20 - line 4\n\
             \x20 - line 5\n\
             \x20 - line 6\n\
             \x20 - line 7\n",
            root.display(),
            vec!["a"; 35].join(" ")
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn shows_the_task_as_json() {
        let repo = task_repo();
        let root = repo.path();
        let value: Value = serde_json::from_str(&load(root).unwrap().to_json()).unwrap();
        assert_eq!(value["root"], root.display().to_string());
        assert_eq!(value["goal"], "웹 서버를 만들어");
        assert_eq!(
            (value["passing"].clone(), value["total"].clone()),
            (json!(2), json!(3))
        );
        assert_eq!(
            value["features"][0],
            json!({"id": "F1", "passes": true, "description": "서버가 8080에서 응답한다\n자세한 설명"})
        );
        assert_eq!(value["features"][2]["passes"], false);
        assert_eq!(
            (value["sessions"].clone(), value["stalled"].clone()),
            (json!(3), json!(1))
        );
        assert_eq!(
            value["tokens"],
            json!({"input": 1200, "output": 340, "cache_write": 50, "cache_read": 900})
        );
        assert_eq!(
            value["stop"],
            json!({
                "reason": "max_sessions",
                "session": 3,
                "detail": null,
                "message": "reached --max-sessions",
                "at": 1_794_205_024
            })
        );
        assert_eq!(value["running"], false);
        assert_eq!(value["run"], Value::Null);
        assert_eq!(value["features_from"], "features.json");
        assert_eq!(value["progress_tail"][4], "- line 7");
        assert_eq!(
            value["progress_tail"].as_array().unwrap().len(),
            PROGRESS_LINES
        );
    }

    #[test]
    fn writes_nothing() {
        let repo = task_repo();
        let root = repo.path();
        let before = snapshot(root);
        let status = load(root).unwrap();
        status.to_text();
        status.to_json();
        assert_eq!(snapshot(root), before);
    }

    #[test]
    fn a_task_that_never_ran_or_did_not_stop_cleanly() {
        let repo = task_repo();
        let root = repo.path();
        fs::remove_file(state_path(root)).unwrap();
        fs::remove_file(progress_path(root)).unwrap();
        let status = load(root).unwrap();
        assert_eq!(status.goal, None);
        assert_eq!(status.stop, None);
        let text = status.to_text();
        assert!(!text.contains("goal:"), "{text}");
        assert!(text.contains("last stop: none, no run yet\n"), "{text}");
        assert!(!text.contains("progress.md"), "{text}");
        let value: Value = serde_json::from_str(&status.to_json()).unwrap();
        assert_eq!(value["goal"], Value::Null);
        assert_eq!(value["stop"], Value::Null);
        assert_eq!(value["progress_tail"], json!([]));

        // A run is going (or crashed): sessions ran, no stop recorded.
        State {
            sessions: 2,
            ..State::default()
        }
        .save(root)
        .unwrap();
        let text = load(root).unwrap().to_text();
        assert!(
            text.contains("last stop: none recorded: a run is going"),
            "{text}"
        );
    }

    #[test]
    fn shows_an_error_stop_with_its_detail() {
        let repo = task_repo();
        let root = repo.path();
        let mut state = State::load(root).unwrap();
        state.stopped_at = None;
        state.stop = Some(Stop {
            reason: StopReason::Error,
            session: 3,
            detail: Some("network down".into()),
        });
        state.save(root).unwrap();
        let status = load(root).unwrap();
        assert!(
            status
                .to_text()
                .contains("last stop: session 3 failed: network down (session 3)\n")
        );
        let value: Value = serde_json::from_str(&status.to_json()).unwrap();
        assert_eq!(value["stop"]["reason"], "error");
        assert_eq!(value["stop"]["detail"], "network down");
        assert_eq!(value["stop"]["at"], Value::Null);
    }

    #[test]
    fn without_a_task_it_says_to_run_init() {
        let repo = repo();
        let err = load(repo.path()).unwrap_err();
        assert!(
            err.to_string().contains("start one with `junees task init"),
            "{err}"
        );
        assert!(!dir(repo.path()).exists());

        // An init that never wrote a list.
        fs::create_dir_all(dir(repo.path())).unwrap();
        let err = load(repo.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("did `junees task init` finish?"),
            "{err:#}"
        );
    }

    /// The state of a run that is going: marked running by `pid`, no stop,
    /// and a harness copy where only F1 passes.
    fn mark_running(root: &Path, pid: u32) {
        let mut state = State::load(root).unwrap();
        state.stop = None;
        state.stopped_at = None;
        state.running = Some(Running {
            pid,
            started: 1_794_205_024,
        });
        let mut copy = features::load(root).unwrap();
        copy[1].passes = false;
        state.features = Some(copy);
        state.save(root).unwrap();
    }

    #[test]
    fn a_running_run_shows_the_harness_copy_of_the_features() {
        let repo = task_repo();
        let root = repo.path();
        let pid = std::process::id();
        mark_running(root, pid);
        // The session marks F2 passing itself, without the harness.
        let mut list = features::load(root).unwrap();
        list[2].passes = true;
        fs::write(features_path(root), serde_json::to_string(&list).unwrap()).unwrap();

        let status = load(root).unwrap();
        assert!(status.running);
        assert_eq!((status.passing, status.total), (1, 3));
        assert_eq!(status.features_from, "harness");
        let text = status.to_text();
        assert!(
            text.contains(
                "features: 1/3 passing [######..............] 33% (as the harness last saved them"
            ),
            "{text}"
        );
        assert!(text.contains("  [ ] F2 "), "{text}");
        assert!(
            text.contains("last stop: none yet, a run is going\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "running: yes, pid {pid} since 2026-11-09 06:17 UTC\n"
            )),
            "{text}"
        );
        let value: Value = serde_json::from_str(&status.to_json()).unwrap();
        assert_eq!(value["running"], true);
        assert_eq!(
            value["run"],
            json!({"pid": pid, "started": 1_794_205_024, "alive": true})
        );
        assert_eq!(value["features_from"], "harness");
        assert_eq!(value["features"][2]["passes"], false);
    }

    #[test]
    fn a_crashed_run_is_not_running() {
        let repo = task_repo();
        let root = repo.path();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        mark_running(root, gone);

        let status = load(root).unwrap();
        assert!(!status.running);
        assert!(!status.run.unwrap().alive);
        let text = status.to_text();
        assert!(
            text.contains("last stop: none: the last run did not stop cleanly\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!("running: no; the run with pid {gone} (started")),
            "{text}"
        );
        assert!(text.contains("next `junees task run` recovers"), "{text}");
        let value: Value = serde_json::from_str(&status.to_json()).unwrap();
        assert_eq!(value["running"], false);
        assert_eq!(value["run"]["alive"], false);
    }

    #[test]
    fn shows_budget_and_interrupted_stops() {
        let repo = task_repo();
        let root = repo.path();
        for (stop, line, reason) in [
            (
                Stop {
                    reason: StopReason::Budget,
                    session: 3,
                    detail: Some("token budget used up: 2100 of 2000 tokens".into()),
                },
                "last stop: token budget used up: 2100 of 2000 tokens (session 3)\n",
                "budget",
            ),
            (
                Stop {
                    reason: StopReason::Interrupted,
                    session: 3,
                    detail: None,
                },
                "last stop: interrupted (session 3)\n",
                "interrupted",
            ),
        ] {
            let mut state = State::load(root).unwrap();
            state.stopped_at = None;
            state.stop = Some(stop);
            state.save(root).unwrap();
            let status = load(root).unwrap();
            let text = status.to_text();
            assert!(text.contains(line), "{text}");
            let value: Value = serde_json::from_str(&status.to_json()).unwrap();
            assert_eq!(value["stop"]["reason"], reason);
        }
    }

    #[test]
    fn bar_fills_in_proportion() {
        assert_eq!(bar(0, 4), format!("[{}] 0%", ".".repeat(20)));
        assert_eq!(bar(1, 4), format!("[#####{}] 25%", ".".repeat(15)));
        assert_eq!(bar(4, 4), format!("[{}] 100%", "#".repeat(20)));
    }
}
