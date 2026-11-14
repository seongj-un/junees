//! The code search tools: `grep`, `glob` and `list_dir`. They run inside
//! Junees, read only, so finding code needs neither bash (its policy,
//! approval questions and sandbox) nor `rg`/`find` on the machine.
//!
//! The rules that matter:
//!
//! - Every file and folder a search meets goes through the read rules of
//!   the file tools ([`FileAccess::allows_read`]), not only the starting
//!   point: a hidden path (a webhook run's sessions and checkpoints, a
//!   credential folder in an unattended run, the settings file) is never
//!   shown, and a hidden folder is not gone into.
//! - The walk is ripgrep's ([`ignore`]): `.gitignore`, `.ignore` and
//!   `.git/info/exclude` hold (also outside a git repository), dot files
//!   only with `hidden`, links are not followed, and `.git/`, `target/` and
//!   `node_modules/` are always left out.
//! - A search stops after [`TIME_LIMIT`] with what it found so far, and as
//!   soon as the call is cancelled (the future dropped).

mod glob;
mod grep;
mod list_dir;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

pub use self::glob::Glob;
pub use self::grep::Grep;
pub use self::list_dir::ListDir;
use super::ToolSettings;
use super::file_access::FileAccess;

/// How long one search may take.
pub const TIME_LIMIT: Duration = Duration::from_secs(10);

/// Folders never searched, whatever the ignore files say.
pub const ALWAYS_SKIPPED: [&str; 3] = [".git", "target", "node_modules"];

/// The rules every search tool shares: what it may read, and how long it
/// may take.
#[derive(Debug, Clone)]
pub struct Search {
    access: FileAccess,
    time_limit: Duration,
}

impl Search {
    /// The rules of tools with `settings`: the read rules of the file tools.
    pub fn new(settings: &ToolSettings) -> Self {
        Self {
            access: FileAccess::new(settings),
            time_limit: TIME_LIMIT,
        }
    }

    /// Rules from `access`, stopping after `time_limit` (tests).
    #[cfg(test)]
    pub(crate) fn with_access(access: FileAccess, time_limit: Duration) -> Self {
        Self { access, time_limit }
    }

    /// The folder or file at `path` (the working folder without one),
    /// resolved and checked like a file `read_file` opens. The error is for
    /// the model.
    fn start(&self, path: Option<&str>) -> Result<Start, String> {
        let shown = path.filter(|path| !path.is_empty()).unwrap_or(".");
        let cannot = |err: std::io::Error| format!("cannot search {shown}: {err}");
        let given = std::path::absolute(shown).map_err(cannot)?;
        let root = std::fs::canonicalize(&given).map_err(cannot)?;
        let meta = std::fs::metadata(&root).map_err(cannot)?;
        self.access
            .check_read(&root, shown)
            .map_err(|refusal| refusal.message(shown, "search"))?;
        Ok(Start {
            root,
            is_dir: meta.is_dir(),
        })
    }

    /// Walks `start`, depth first and in name order, calling `visit` with
    /// every entry the rules let through (the start itself not included);
    /// `visit` returns `false` to stop. How the walk ended.
    fn walk(
        &self,
        start: &Start,
        walk: Walk,
        stop: &Stop,
        mut visit: impl FnMut(&ignore::DirEntry) -> bool,
    ) -> Ended {
        let mut builder = ignore::WalkBuilder::new(&start.root);
        builder
            .hidden(!walk.hidden)
            .follow_links(false)
            .require_git(false)
            .max_depth(walk.depth)
            .sort_by_file_name(|a, b| a.cmp(b));
        let access = self.access.clone();
        builder.filter_entry(move |entry| {
            if entry.depth() == 0 {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if is_dir
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| ALWAYS_SKIPPED.contains(&name))
            {
                return false;
            }
            access.allows_read(entry.path())
        });
        for entry in builder.build() {
            if let Some(ended) = stop.ended() {
                return ended;
            }
            // An unreadable folder or a broken ignore file: left out.
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue;
            }
            if !visit(&entry) {
                break;
            }
        }
        stop.ended().unwrap_or(Ended::Done)
    }
}

/// Where a search starts: the resolved path, and whether it is a folder.
struct Start {
    root: PathBuf,
    is_dir: bool,
}

/// How to walk.
#[derive(Debug, Clone, Copy, Default)]
struct Walk {
    /// Show dot files and go into dot folders.
    hidden: bool,
    /// How deep below the start (`None`: all the way).
    depth: Option<usize>,
}

/// How a walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Done,
    TimedOut,
    Cancelled,
}

impl Ended {
    /// The line that says the results are partial, if they are.
    fn note(self, limit: Duration) -> Option<String> {
        match self {
            Ended::Done => None,
            Ended::TimedOut => Some(format!(
                "[stopped at the {} s time limit: these are the results so far; search a \
                 smaller folder or a narrower pattern]",
                limit.as_secs()
            )),
            Ended::Cancelled => Some("[cancelled]".to_string()),
        }
    }
}

/// When a search must stop: its deadline, and a flag the call sets when it
/// is dropped (cancelled) while the search runs on a blocking thread.
#[derive(Debug, Clone)]
struct Stop {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Stop {
    fn new(limit: Duration) -> Self {
        Self {
            deadline: Instant::now() + limit,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn ended(&self) -> Option<Ended> {
        if self.cancelled.load(Ordering::Relaxed) {
            Some(Ended::Cancelled)
        } else if Instant::now() >= self.deadline {
            Some(Ended::TimedOut)
        } else {
            None
        }
    }

    /// A guard that cancels the search when dropped: hold it across the
    /// await, so a cancelled call stops the blocking walk too.
    fn guard(&self) -> CancelOnDrop {
        CancelOnDrop(Some(Arc::clone(&self.cancelled)))
    }
}

struct CancelOnDrop(Option<Arc<AtomicBool>>);

impl CancelOnDrop {
    /// The search finished: dropping no longer cancels anything.
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(flag) = &self.0 {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Runs `search` on a blocking thread with a [`Stop`] after `limit`,
/// stopping it when the call is dropped.
async fn run_blocking<T: Send + 'static>(
    limit: Duration,
    search: impl FnOnce(&Stop) -> T + Send + 'static,
) -> Result<T, String> {
    let stop = Stop::new(limit);
    let guard = stop.guard();
    let result = tokio::task::spawn_blocking(move || search(&stop))
        .await
        .map_err(|err| format!("the search failed: {err}"));
    guard.disarm();
    result
}

/// `path` as the model should see it: relative to the working folder when
/// it is inside it, else absolute, control and direction characters in
/// names written out.
fn shown(path: &Path) -> String {
    let path = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_path_buf))
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or_else(|| path.to_path_buf());
    crate::prompt_text::escape_hidden(&path.display().to_string())
}

/// When `entry` was last changed, for newest-first lists.
fn modified(entry: &ignore::DirEntry) -> SystemTime {
    entry
        .metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// An optional string argument, or the error for the model.
fn opt_str<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

/// A whole-number argument in `range` (`default` without one), or the
/// error for the model.
fn bounded(
    input: &Value,
    key: &str,
    range: std::ops::RangeInclusive<u64>,
    default: u64,
) -> Result<u64, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|number| range.contains(number))
            .ok_or_else(|| {
                format!(
                    "`{key}` must be a whole number from {} to {}",
                    range.start(),
                    range.end()
                )
            }),
    }
}

/// An optional boolean argument, or the error for the model.
fn opt_bool(input: &Value, key: &str) -> Result<bool, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("`{key}` must be true or false")),
    }
}

/// Whether `path` names somewhere the tools would leave the working
/// folder for (approval asks about those in a conversation).
pub fn names_outside(path: Option<&str>, workspace: &super::policy::Workspace) -> bool {
    !workspace.contains(path.filter(|path| !path.is_empty()).unwrap_or("."))
}
