//! Runs task sessions: the init session that plans a task, and the work
//! sessions that build it one feature at a time.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};

use super::budget::{self, Budget, format_duration};
use super::claude_code::{ClaudeCode, End, NotLoggedIn, Request};
use super::features::{self, Ledger};
use super::lock::RunLock;
use super::state::{Running, State, Stop, StopReason};
use super::tools::{MarkFeaturePassing, Verifier, session_tools};
use super::{
    STALL_LIMIT, append_run_log, dir, features_path, git, progress_path, prompt,
    rejected_features_path, run_log_path, sessions_dir,
};
use crate::agent::{Agent, AgentOptions, Cancelled, Event, Limit, LimitReached, TurnLimit};
use crate::cancel::Cancel;
use crate::cli::preview;
use crate::message::Usage;
use crate::notify::Notifier;
use crate::provider::Provider;
use crate::session::{SessionWriter, format_utc};
use crate::tools::{ToolRegistry, ToolSettings};

/// Commit message of the commit that starts a task.
pub const INIT_COMMIT: &str = "junees: initialize task";

/// Commit message of the commit that keeps what a crashed run left behind.
pub const RECOVER_COMMIT: &str = "junees: recover after crash";

/// Commit message of the commit that brings the `.junees/.gitignore` of an
/// older task up to date before a run.
pub const IGNORE_COMMIT: &str = "junees: keep the run log out of git";

/// Commit message of the commit that keeps what the start of a run changed
/// before any session: passes the recheck took back, the feature list in
/// the harness's formatting, files the verify commands left behind.
pub const RECHECK_COMMIT: &str = "junees: recheck passing features at run start";

/// What `.junees/.gitignore` keeps out of git: harness files that change
/// on every run.
const IGNORED: &[&str] = &[
    "state.json",
    ".*.tmp",
    "*.rejected.json",
    "run.log",
    "sessions/",
];

/// Build-output folders `task init` adds to the repository's `.gitignore`
/// when git does not ignore them yet: verify commands build, and the
/// harness commits everything new that is not ignored.
const BUILD_OUTPUT: &[&str] = &[
    "target/",
    "node_modules/",
    "dist/",
    "build/",
    ".venv/",
    "__pycache__/",
];

/// Files [`dirty_tree`] names before it only counts the rest.
const NAMED_LEFT_OUT: usize = 3;

const NO_TASK: &str = "cannot load the feature list (start a task with `junees task \"<goal>\"`)";

/// Why task mode does not start on a work tree with changes: its commits
/// take everything a session leaves behind, and would take the user's
/// unfinished work and stray files (`.env`) with it.
const DIRTY_TREE: &str = "the work tree has uncommitted changes or untracked files; \
                          commit or stash them first (`git stash -u`), because task \
                          mode commits everything in the work tree after each session";

/// Recent commits a work session sees in its system prompt.
const GIT_LOG_LINES: usize = 20;

/// Characters of a session error kept in progress.md.
const ERROR_CHARS: usize = 500;

/// Commit message of the commit the harness makes when session `session`
/// left changes behind.
pub fn checkpoint_message(session: u64) -> String {
    format!("junees: checkpoint after session {session}")
}

/// Makes the provider of each new session.
pub type ProviderFactory<'a> = dyn Fn() -> Result<Box<dyn Provider>> + 'a;

/// What task sessions need to run.
pub struct TaskEnv<'a> {
    /// Root of the git repository the task works on.
    pub root: PathBuf,
    /// Where session journals are saved; `None` runs sessions unsaved.
    pub home: Option<PathBuf>,
    pub model: String,
    pub max_tokens: u32,
    /// Model calls one session may make.
    pub max_turns: usize,
    /// The user's settings for the session tools and verify commands.
    pub tool_settings: ToolSettings,
    /// Context management of every session, as in a normal session.
    pub context_window: u64,
    pub compact_threshold: f64,
    pub provider: &'a ProviderFactory<'a>,
    /// Where `run` reports its stop (and sessions, if asked).
    pub notify: Notifier,
    /// Token and time limits of `task run`.
    pub budget: Budget,
    /// Set by SIGINT/SIGTERM: the session that runs is cancelled (its tool
    /// stopped), wrapped up and checkpointed, and the run stops.
    pub cancel: Cancel,
    /// Runs the sessions with the Claude Code CLI instead of `provider`
    /// (the `claude-code` backend).
    pub claude: Option<ClaudeCode>,
}

/// What the runner tells the user: its own status lines and the events of
/// the session agents.
pub enum Report<'a> {
    Line(&'a str),
    Event(Event<'a>),
}

pub type Reporter<'r> = dyn FnMut(Report<'_>) + 'r;

impl TaskEnv<'_> {
    /// The tools of a task session, without `mark_feature_passing`. Every
    /// task session gets its tools from here and every verify command runs
    /// through [`TaskEnv::verifier`], so tool settings belong in these two.
    fn tools(&self) -> ToolRegistry {
        session_tools(&self.tool_settings)
    }

    fn verifier(&self) -> Verifier {
        Verifier::new(self.root.clone(), &self.tool_settings)
    }

    /// With the `claude-code` backend, checks that `claude` runs on the
    /// subscription and tells the user which one, and whether the commands
    /// it runs can see its credentials.
    async fn check_claude(&self, report: &mut Reporter<'_>) -> Result<()> {
        if let Some(claude) = &self.claude {
            let version = claude.check(&self.root, &self.tool_settings).await?;
            line(
                report,
                &format!("== sessions run by Claude Code {version} =="),
            );
            for notice in claude.notices() {
                warn(report, &notice);
            }
        }
        Ok(())
    }

    /// A Claude Code session in the task's repository, its output saved as
    /// `.junees/sessions/<name>.jsonl`.
    fn claude_request<'r>(&'r self, system: &'r str, input: &'r str, log: &'r Path) -> Request<'r> {
        Request {
            root: &self.root,
            system,
            input,
            max_turns: self.max_turns,
            settings: &self.tool_settings,
            log: Some(log),
        }
    }

    /// A new agent with a fresh context, recording to a new session journal
    /// when there is a state folder. Returns the journal's session id.
    fn agent(
        &self,
        system: String,
        tools: ToolRegistry,
        report: &mut Reporter<'_>,
    ) -> Result<(Agent, Option<String>)> {
        let agent = Agent::new(
            (self.provider)()?,
            tools,
            AgentOptions {
                system,
                max_tokens: self.max_tokens,
                max_turns: self.max_turns,
                context_window: self.context_window,
                compact_threshold: self.compact_threshold,
            },
        )
        .with_cancel(self.cancel.clone());
        let Some(home) = &self.home else {
            return Ok((agent, None));
        };
        match SessionWriter::create(home, &self.root, &self.model) {
            Ok((meta, writer)) => Ok((agent.with_journal(Box::new(writer)), Some(meta.id))),
            Err(err) => {
                warn(report, &format!("session not saved: {err:#}"));
                Ok((agent, None))
            }
        }
    }
}

/// Plans a new task in the repository at `env.root`: an init session writes
/// the feature list and progress notes, the harness checks the list (giving
/// the session one chance to fix it) and commits everything.
pub async fn init(env: &TaskEnv<'_>, goal: &str, report: &mut Reporter<'_>) -> Result<()> {
    let goal = goal.trim();
    if goal.is_empty() {
        bail!("the goal is empty");
    }
    let root = &env.root;
    git::toplevel(root)?;
    let path = features_path(root);
    if path.exists() {
        bail!(
            "{} already exists: this repository already has a task \
             (continue it with `junees task`, or delete .junees/ if an \
             earlier `junees task init` did not finish)",
            path.display()
        );
    }
    // A failed init may have left .junees/ behind; the next one rewrites it.
    if git::is_dirty_outside_task(root)? {
        return Err(dirty_tree(root, &["--", ".", ":(exclude).junees"]));
    }
    prepare_dir(root)?;
    env.check_claude(report).await?;

    let (count, usage) = match plan(env, goal, report).await {
        Ok(planned) => planned,
        Err(err) => return Err(set_aside_rejected_list(root, err)),
    };
    let mut state = State::load(root)?;
    state.usage.add(usage);
    state.goal = Some(goal.to_string());
    state.save(root)?;
    append_progress(
        root,
        &format!("init: {count} features planned, {}", tokens(usage)),
    )?;
    // Only now: a failed init must leave the user's files as they were.
    ignore_build_output(root, report)?;
    let commit = git::commit_all(root, INIT_COMMIT).context("cannot commit the new task")?;
    warn_left_out(report, &commit.left_out);
    line(
        report,
        &format!("== init done: {count} features, committed =="),
    );
    Ok(())
}

/// The init session: writes the feature list and gets one chance to fix
/// it. Returns the number of features and the tokens used.
async fn plan(env: &TaskEnv<'_>, goal: &str, report: &mut Reporter<'_>) -> Result<(usize, Usage)> {
    if let Some(claude) = &env.claude {
        return plan_with_claude(env, claude, goal, report).await;
    }
    let root = &env.root;
    let (mut agent, journal) = env.agent(prompt::init_system(root), env.tools(), report)?;
    line(
        report,
        &format!("== init session{} ==", journal_note(&journal)),
    );
    agent
        .run(&prompt::init_input(goal), |event| {
            report(Report::Event(event))
        })
        .await
        .context("the init session failed")?;
    let verifier = env.verifier();
    let count = match check_new_list(root, &verifier, report).await {
        Ok(count) => count,
        Err(err) => {
            let problem = format!("{err:#}");
            warn(
                report,
                &format!("{problem}; asking the init session to fix it"),
            );
            agent
                .run(&prompt::fix_input(&problem), |event| {
                    report(Report::Event(event))
                })
                .await
                .context("the init session failed")?;
            check_new_list(root, &verifier, report)
                .await
                .context("the feature list is still invalid after one fix")?
        }
    };
    // Writes the list in the harness's own formatting.
    Ledger::load(root)?;
    Ok((count, agent.usage()))
}

/// [`plan`] with Claude Code. Claude Code keeps its own session, so the
/// fix is a second session, told the goal and what was wrong.
async fn plan_with_claude(
    env: &TaskEnv<'_>,
    claude: &ClaudeCode,
    goal: &str,
    report: &mut Reporter<'_>,
) -> Result<(usize, Usage)> {
    let root = &env.root;
    let system = prompt::claude_init_system();
    let verifier = env.verifier();
    let mut usage = Usage::default();
    let mut input = prompt::init_input(goal);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let name = if attempt == 1 { "init" } else { "init-fix" };
        let log = sessions_dir(root).join(format!("{name}.jsonl"));
        line(
            report,
            &format!(
                "== {name} session, Claude Code, output in {} ==",
                shown(root, &log)
            ),
        );
        let outcome = claude
            .run(
                &env.claude_request(&system, &input, &log),
                &env.cancel,
                None,
                report,
            )
            .await;
        usage.add(outcome.usage);
        match outcome.end.context("the init session failed")? {
            End::Answered => {}
            End::Interrupted => {
                return Err(anyhow::Error::new(Cancelled).context("the init session failed"));
            }
            End::TurnLimit => bail!(
                "the init session failed: it stopped at --max-turns {}",
                env.max_turns
            ),
            End::Budget(reason) => bail!("the init session failed: {reason}"),
        }
        match check_new_list(root, &verifier, report).await {
            Ok(count) => {
                // Writes the list in the harness's own formatting.
                Ledger::load(root)?;
                return Ok((count, usage));
            }
            Err(err) if attempt == 1 => {
                let problem = format!("{err:#}");
                warn(
                    report,
                    &format!("{problem}; asking a new init session to fix it"),
                );
                input = format!(
                    "{}\n\n{}",
                    prompt::init_input(goal),
                    prompt::fix_input(&problem)
                );
            }
            Err(err) => return Err(err.context("the feature list is still invalid after one fix")),
        }
    }
}

/// `path` relative to `root` when it is inside it, for status lines.
fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// After a failed init, moves the list the session wrote out of the way:
/// `task run` must not start from a list the harness rejected, and the
/// next `task init` must not find a task already there. The file is kept
/// (and ignored by git) for the user to look at.
fn set_aside_rejected_list(root: &Path, err: anyhow::Error) -> anyhow::Error {
    let path = features_path(root);
    if !path.exists() {
        return err;
    }
    let rejected = rejected_features_path(root);
    match fs::rename(&path, &rejected) {
        Ok(()) => err.context(format!(
            "task init failed; its feature list was moved to {}, so you can run \
             `junees task init` again",
            rejected.display()
        )),
        Err(rename) => err.context(format!(
            "task init failed, and its feature list could not be moved out of the way \
             ({rename}); delete {} before running `junees task init` again",
            path.display()
        )),
    }
}

/// Runs work sessions on the task in `env.root` until every feature passes,
/// `max_sessions` sessions have run, [`STALL_LIMIT`] sessions in a row
/// passed no new feature, a session fails, the budget runs out, or
/// `env.cancel` is set. Returns why it stopped, which is also recorded in
/// state.json and posted through `env.notify`.
///
/// The budget is checked before each session and, inside a session, before
/// each model call; a session that runs out ends there and is wrapped up
/// like any other.
///
/// Every session is a new agent with a fresh context and its own journal.
/// After each one the harness puts its copy of the feature list back into
/// features.json, adds a line to progress.md and commits whatever the
/// session left uncommitted. Session numbers and counters continue from
/// state.json, so a run picks up where the last one (or a crash) left off.
///
/// A run holds the task's [`RunLock`] and marks itself running in
/// state.json until it stops. A run that finds the mark of one that no
/// longer holds the lock recovers from that crash first (see [`recover`]).
///
/// A run that fails before it can record a stop (a dirty work tree, a
/// feature list that does not load, a state file that cannot be written)
/// returns the error, and posts it through `env.notify` too: an unattended
/// run that never starts needs the alert most.
pub async fn run(
    env: &TaskEnv<'_>,
    max_sessions: usize,
    report: &mut Reporter<'_>,
) -> Result<Stop> {
    let result = run_until_stop(env, max_sessions, report).await;
    if let Err(err) = &result
        && let Err(notify) = env.notify.run_failed(&env.root, err).await
    {
        warn(report, &notify.to_string());
    }
    result
}

/// [`run`] without the notification of a failure.
async fn run_until_stop(
    env: &TaskEnv<'_>,
    max_sessions: usize,
    report: &mut Reporter<'_>,
) -> Result<Stop> {
    let root = &env.root;
    let started = env.budget.now();
    git::toplevel(root)?;
    if !dir(root).is_dir() {
        features::load(root).context(NO_TASK)?;
    }
    let Some(_lock) = RunLock::acquire(root)? else {
        let holder = match State::load(root).ok().and_then(|state| state.running) {
            Some(running) => format!(
                " (pid {}, started {} UTC)",
                running.pid,
                format_utc(running.started)
            ),
            None => String::new(),
        };
        bail!("another `junees task run`{holder} is working on this task; wait for it to stop");
    };
    let mut state = State::load(root)?;
    if let Some(running) = state.running.take() {
        recover(env, &mut state, &running, report)?;
    }
    features::load(root).context(NO_TASK)?;
    if git::is_dirty(root)? {
        return Err(dirty_tree(root, &[]));
    }
    env.check_claude(report).await?;
    let ledger = Ledger::load(root)?;
    let ledger = Arc::new(Mutex::new(ledger));
    // A task from an older Junees learns to ignore the run log here, in its
    // own commit, so the tree stays clean even if no session runs.
    if prepare_dir(root)? && git::is_dirty(root)? {
        git::commit_all(root, IGNORE_COMMIT)?;
    }
    let unchecked =
        recheck_passing(env, &ledger, budget::tokens(state.usage), started, report).await?;
    // The tree was clean before the list was loaded, so whatever changed
    // since is the harness's doing. Committed now, the run leaves a clean
    // tree even when it stops before any session (see `recheck_commit`).
    recheck_commit(root, report)?;
    // A run that stopped on its own does not hold its stall streak against
    // the next one; after a crash (no stop recorded) the streak carries on.
    if state.stop.take().is_some() {
        state.stalled = 0;
    }
    state.running = Some(Running {
        pid: std::process::id(),
        started: unix_now(),
    });
    save_state(root, &mut state, &ledger)?;
    let (passing, total) = {
        let ledger = ledger.lock().unwrap();
        (ledger.passing(), ledger.total())
    };
    log(
        root,
        report,
        &format!(
            "run start (pid {}): {passing}/{total} passing, {}, max sessions {max_sessions}",
            std::process::id(),
            spent(env, &state, started)
        ),
    );

    let mut ran = 0;
    let stop = loop {
        let stop = |reason, session| Stop {
            reason,
            session,
            detail: None,
        };
        // Passes the recheck did not get to are unconfirmed, so they
        // cannot complete the task.
        match &unchecked {
            Some(Unchecked::Cancelled) => break stop(StopReason::Interrupted, state.sessions),
            Some(Unchecked::Budget(detail)) => {
                break Stop {
                    reason: StopReason::Budget,
                    session: state.sessions,
                    detail: Some(detail.clone()),
                };
            }
            None => {}
        }
        if ledger.lock().unwrap().all_pass() {
            break stop(StopReason::Complete, state.sessions);
        }
        if env.cancel.is_cancelled() {
            break stop(StopReason::Interrupted, state.sessions);
        }
        if ran == max_sessions {
            break stop(StopReason::MaxSessions, state.sessions);
        }
        let used = budget::tokens(state.usage);
        if let Some(detail) = env.budget.exceeded(used, started) {
            break Stop {
                reason: StopReason::Budget,
                session: state.sessions,
                detail: Some(detail),
            };
        }
        ran += 1;
        state.sessions += 1;
        let n = state.sessions;
        save_state(root, &mut state, &ledger)?;
        log(root, report, &format!("session {n} start"));
        let before = ledger.lock().unwrap().passing_ids();

        let limit = env.budget.session_limit(used, started);
        let (result, usage) = match &env.claude {
            Some(claude) => claude_session(env, claude, n, &ledger, limit, started, report).await,
            None => work_session(env, n, &ledger, limit, report).await,
        };
        state.usage.add(usage);

        let (summary, progressed, passing, total) =
            match wrap_up(env, n, &ledger, &before, &result, usage, report) {
                Ok(wrapped) => wrapped,
                // The session's work may be uncommitted, but the stop is
                // recorded, so the next run does not take this for a crash.
                Err(err) => {
                    let detail = format!("could not wrap up session {n}: {err:#}");
                    warn(report, &detail);
                    break Stop {
                        reason: StopReason::Error,
                        session: n,
                        detail: Some(detail),
                    };
                }
            };
        state.stalled = if progressed { 0 } else { state.stalled + 1 };
        line(report, &format!("== session {n} done: {summary} =="));
        log(
            root,
            report,
            &format!(
                "session {n} end: {summary}; {}",
                spent(env, &state, started)
            ),
        );

        let ended = match result {
            Err(err) => Some(Stop {
                reason: StopReason::Error,
                session: n,
                detail: Some(format!("{err:#}")),
            }),
            Ok(_) if passing == total => Some(stop(StopReason::Complete, n)),
            Ok(Ended::Interrupted) => Some(stop(StopReason::Interrupted, n)),
            Ok(Ended::Budget(detail)) => Some(Stop {
                reason: StopReason::Budget,
                session: n,
                detail: Some(detail),
            }),
            Ok(_) if state.stalled >= STALL_LIMIT => Some(stop(StopReason::Stalled, n)),
            Ok(_) => None,
        };
        // Saved before the notification, which may take seconds: a run
        // killed while it waits (a second Ctrl-C) keeps the session's tokens
        // and its stop, so the next run does not take it for a crash.
        if let Some(ended) = &ended {
            state.record_stop(ended.clone());
            state.running = None;
        }
        save_state(root, &mut state, &ledger)?;
        // After a signal the run only waits for its stop notification.
        if !env.cancel.is_cancelled()
            && let Err(err) = env.notify.session_done(root, n, &summary).await
        {
            warn(report, &err.to_string());
        }
        if let Some(ended) = ended {
            break ended;
        }
    };
    if state.stop.is_none() {
        state.record_stop(stop.clone());
        state.running = None;
        save_state(root, &mut state, &ledger)?;
    }
    let reason = serde_json::to_value(stop.reason).unwrap_or_default();
    log(
        root,
        report,
        &format!(
            "run stop ({}): {stop}; {}",
            reason.as_str().unwrap_or("?"),
            spent(env, &state, started)
        ),
    );
    let (passing, total) = {
        let ledger = ledger.lock().unwrap();
        (ledger.passing(), ledger.total())
    };
    // A failed notification does not change how the run ended.
    if let Err(err) = env
        .notify
        .task_stopped(root, &stop, passing, total, &state)
        .await
    {
        warn(report, &err.to_string());
    }
    Ok(stop)
}

/// Commits what the start of a run changed in the work tree, if anything:
/// [`Ledger::load`] writes the list in the harness's formatting, the
/// recheck takes passes back in features.json and notes it in progress.md,
/// and verify commands may leave files behind. Without this commit, a run
/// that stops before any session (a budget already used up, a signal,
/// nothing left to do) would leave them uncommitted, and the next run would
/// refuse the dirty tree.
fn recheck_commit(root: &Path, report: &mut Reporter<'_>) -> Result<()> {
    if !git::is_dirty(root)? {
        return Ok(());
    }
    let commit = git::commit_all(root, RECHECK_COMMIT)
        .context("cannot commit the changes of the start-of-run recheck")?;
    warn_left_out(report, &commit.left_out);
    if commit.made {
        line(report, &format!("recheck commit: {RECHECK_COMMIT}"));
    }
    Ok(())
}

/// The task's tokens and this run's time, with their budgets.
fn spent(env: &TaskEnv<'_>, state: &State, started: std::time::Instant) -> String {
    let tokens = budget::tokens(state.usage);
    let time = format_duration(env.budget.now().saturating_duration_since(started));
    let tokens = match env.budget.max_tokens {
        Some(max) => format!("{tokens} of {max}"),
        None => tokens.to_string(),
    };
    let time = match env.budget.max_time {
        Some(max) => format!("{time} of {}", format_duration(max)),
        None => time,
    };
    format!("task tokens {tokens}, run time {time}")
}

/// Adds a line to `.junees/run.log`. The log is for people watching; a
/// failure to write it is a warning, not a reason to stop.
fn log(root: &Path, report: &mut Reporter<'_>, text: &str) {
    if let Err(err) = append_run_log(root, unix_now(), text) {
        warn(
            report,
            &format!("could not write {}: {err}", run_log_path(root).display()),
        );
    }
}

/// Saves state.json with the harness's current copy of the feature list.
fn save_state(root: &Path, state: &mut State, ledger: &Mutex<Ledger>) -> Result<()> {
    state.features = Some(ledger.lock().unwrap().features().to_vec());
    state.save(root)
}

/// Picks up after a run that crashed (killed, or the machine went down)
/// while `running` it: puts the feature list back into features.json as
/// the task's init commit defined it (see [`features::restore_defined`]),
/// notes the crash in progress.md and commits whatever the crashed session
/// left behind, so the run can start from a clean tree. The crash's mark
/// is cleared on disk at once, so a run that then refuses to start does
/// not recover twice.
///
/// The definitions (ids, descriptions, verify commands) do not come from
/// the copy in state.json: a session can rewrite that file and kill the
/// run before the harness writes it again, and the recheck would then run
/// verify commands the session chose. Rewriting the init commit takes
/// rewriting the history, which the harness cannot stop either, but which
/// one edited file does not do.
fn recover(
    env: &TaskEnv<'_>,
    state: &mut State,
    running: &Running,
    report: &mut Reporter<'_>,
) -> Result<()> {
    let root = &env.root;
    warn(
        report,
        &format!(
            "the run started {} UTC (pid {}) did not stop cleanly; recovering",
            format_utc(running.started),
            running.pid
        ),
    );
    let (init, defined) = init_list(root).context("cannot recover after the crash")?;
    prepare_dir(root)?;
    let copy = state.features.as_deref();
    let restored = features::restore_defined(root, &defined, copy)?;
    let mut note = format!(
        "recovered after a crash: the run started {} UTC stopped without a record \
         during session {}",
        format_utc(running.started),
        state.sessions
    );
    if copy.is_some_and(|copy| !features::same_definitions(copy, &defined)) {
        warn(
            report,
            &format!(
                "the copy of the feature list in state.json does not match the task's init \
                 commit {init}; the init commit's definitions are used"
            ),
        );
        note.push_str(&format!(
            "; the copy in state.json did not match the task's init commit {init}, so its \
             definitions were not used"
        ));
    }
    if restored {
        note.push_str(&format!(
            "; .junees/features.json was restored from the init commit {init}"
        ));
    }
    append_progress(root, &note)?;
    if git::is_dirty(root)? {
        let commit = git::commit_all(root, RECOVER_COMMIT)?;
        warn_left_out(report, &commit.left_out);
        if commit.made {
            line(report, &format!("recovery commit: {RECOVER_COMMIT}"));
        }
    }
    state.save(root)?;
    log(
        root,
        report,
        &format!(
            "recovered after a crash of the run started {} UTC (pid {}) during session {}",
            format_utc(running.started),
            running.pid,
            state.sessions
        ),
    );
    Ok(())
}

/// The oldest [`INIT_COMMIT`] in the history (shortened) and the feature
/// list it committed, with every feature not passing.
fn init_list(root: &Path) -> Result<(String, Vec<features::Feature>)> {
    let Some(commit) = git::oldest_with_subject(root, INIT_COMMIT)? else {
        bail!(
            "there is no `{INIT_COMMIT}` commit in the history to take the feature \
             definitions from; put .junees/features.json right yourself, commit it, and \
             remove \"running\" from .junees/state.json"
        );
    };
    let short: String = commit.chars().take(12).collect();
    let text = git::show(root, &commit, ".junees/features.json")
        .with_context(|| format!("cannot read the feature list of {short}"))?;
    let mut list =
        features::parse(&text).with_context(|| format!("the feature list of {short}"))?;
    for feature in &mut list {
        feature.passes = false;
    }
    Ok((short, list))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The harness's part after a work session: puts its copy of the feature
/// list back, adds the session's line to progress.md and commits what the
/// session left behind. Returns the summary line, whether a feature newly
/// passes, and the passing and total counts.
fn wrap_up(
    env: &TaskEnv<'_>,
    n: u64,
    ledger: &Arc<Mutex<Ledger>>,
    before: &HashSet<String>,
    result: &Result<Ended>,
    usage: Usage,
    report: &mut Reporter<'_>,
) -> Result<(String, bool, usize, usize)> {
    let root = &env.root;
    // The session may have deleted .junees/ or its .gitignore, which keeps
    // state.json out of the checkpoint commit.
    prepare_dir(root)?;
    let (edited, newly, passing, total) = {
        let mut ledger = ledger.lock().unwrap();
        let edited = ledger.restore()?;
        let newly: Vec<&str> = ledger
            .features()
            .iter()
            .filter(|feature| feature.passes && !before.contains(&feature.id))
            .map(|feature| feature.id.as_str())
            .collect();
        (edited, newly.join(", "), ledger.passing(), ledger.total())
    };
    if edited {
        warn(
            report,
            "the session edited .junees/features.json; the harness restored its own copy",
        );
        append_progress(
            root,
            &format!(
                "session {n}: .junees/features.json was edited directly; \
                 the harness restored its own copy"
            ),
        )?;
    }
    let passes = if newly.is_empty() {
        format!("{passing}/{total} passing, nothing new")
    } else {
        format!("{passing}/{total} passing, new: {newly}")
    };
    let summary = match &result {
        Ok(Ended::Answered) => passes,
        Ok(Ended::TurnLimit) => format!(
            "{passes}, ended at the turn limit ({} model calls)",
            env.max_turns
        ),
        Ok(Ended::Budget(reason)) => format!("{passes}, ended early: {reason}"),
        Ok(Ended::Interrupted) => format!("{passes}, interrupted"),
        Err(err) => format!("failed: {}", preview(&format!("{err:#}"), ERROR_CHARS)),
    };
    append_progress(root, &format!("session {n}: {summary}, {}", tokens(usage)))?;
    if git::is_dirty(root)? {
        let commit = git::commit_all(root, &checkpoint_message(n))?;
        warn_left_out(report, &commit.left_out);
        if commit.made {
            line(
                report,
                &format!("checkpoint commit: {}", checkpoint_message(n)),
            );
        }
    }
    Ok((summary, !newly.is_empty(), passing, total))
}

/// How a work session that did not fail ended.
enum Ended {
    /// The model gave its final answer.
    Answered,
    /// The model was still working when the session used up its model
    /// calls. Its work is kept like any other session's, and a session that
    /// keeps doing this counts towards the stall limit.
    TurnLimit,
    /// The budget ran out before a model call; the run stops after it.
    Budget(String),
    /// The session was cancelled (SIGINT/SIGTERM); the run stops after it.
    Interrupted,
}

/// How a session whose agent returned `err` ended: the turn limit and the
/// budget end a session, anything else is a failure.
fn ended_early(err: anyhow::Error) -> Result<Ended> {
    if err.downcast_ref::<TurnLimit>().is_some() {
        return Ok(Ended::TurnLimit);
    }
    if let Some(LimitReached(reason)) = err.downcast_ref::<LimitReached>() {
        return Ok(Ended::Budget(reason.clone()));
    }
    if err.downcast_ref::<Cancelled>().is_some() {
        return Ok(Ended::Interrupted);
    }
    Err(err)
}

/// One work session: a fresh agent told where the task stands, with the
/// default tools and `mark_feature_passing`, stopped by `limit` before a
/// model call. Returns how it ended and the tokens it used.
async fn work_session(
    env: &TaskEnv<'_>,
    n: u64,
    ledger: &Arc<Mutex<Ledger>>,
    limit: Option<Limit>,
    report: &mut Reporter<'_>,
) -> (Result<Ended>, Usage) {
    let root = &env.root;
    let progress = fs::read_to_string(progress_path(root)).unwrap_or_default();
    let git_log = git::recent_log(root, GIT_LOG_LINES);
    let (system, passing, total) = {
        let ledger = ledger.lock().unwrap();
        let system = prompt::work_system(root, n, ledger.features(), &progress, &git_log);
        (system, ledger.passing(), ledger.total())
    };
    let mut tools = env.tools();
    tools.register(MarkFeaturePassing::new(Arc::clone(ledger), env.verifier()));
    let (mut agent, journal) = match env.agent(system, tools, report) {
        Ok(created) => created,
        Err(err) => return (Err(err), Usage::default()),
    };
    if let Some(limit) = limit {
        agent = agent.with_limit(limit);
    }
    line(
        report,
        &format!(
            "== session {n} start: {passing}/{total} passing{} ==",
            journal_note(&journal)
        ),
    );
    let result = match agent
        .run(prompt::WORK_INPUT, |event| report(Report::Event(event)))
        .await
    {
        Ok(_) => Ok(Ended::Answered),
        Err(err) => ended_early(err),
    };
    (result, agent.usage())
}

/// [`work_session`] with Claude Code. Claude Code cannot mark a feature
/// as passing, so afterwards the harness runs the verify command of every
/// feature that does not pass yet (see [`record_passes`]), unless the
/// session was interrupted or Claude Code is not signed in.
async fn claude_session(
    env: &TaskEnv<'_>,
    claude: &ClaudeCode,
    n: u64,
    ledger: &Arc<Mutex<Ledger>>,
    mut limit: Option<Limit>,
    started: std::time::Instant,
    report: &mut Reporter<'_>,
) -> (Result<Ended>, Usage) {
    let root = &env.root;
    let progress = fs::read_to_string(progress_path(root)).unwrap_or_default();
    let git_log = git::recent_log(root, GIT_LOG_LINES);
    let (system, passing, total) = {
        let ledger = ledger.lock().unwrap();
        let system = prompt::claude_work_system(n, ledger.features(), &progress, &git_log);
        (system, ledger.passing(), ledger.total())
    };
    let output = sessions_dir(root).join(format!("session-{n}.jsonl"));
    line(
        report,
        &format!(
            "== session {n} start: {passing}/{total} passing, Claude Code, output in {} ==",
            shown(root, &output)
        ),
    );
    let outcome = claude
        .run(
            &env.claude_request(&system, prompt::WORK_INPUT, &output),
            &env.cancel,
            limit.as_mut(),
            report,
        )
        .await;
    log(
        root,
        report,
        &format!("session {n} claude-code: {}", outcome.summary()),
    );
    let result = outcome.end.map(|end| match end {
        End::Answered => Ended::Answered,
        End::TurnLimit => Ended::TurnLimit,
        End::Budget(reason) => Ended::Budget(reason),
        End::Interrupted => Ended::Interrupted,
    });
    let verify = match &result {
        Ok(Ended::Interrupted) => false,
        Ok(_) => true,
        Err(err) => err.downcast_ref::<NotLoggedIn>().is_none(),
    };
    if verify && let Err(err) = record_passes(env, ledger, started, report).await {
        return (Err(err), outcome.usage);
    }
    (result, outcome.usage)
}

/// The harness's part of a Claude Code session: runs the verify command of
/// every feature that does not pass yet, as `mark_feature_passing` would,
/// and records the ones that exit 0. It stops early, leaving the rest for
/// a later session, when `env.cancel` is set or the run's time is up.
async fn record_passes(
    env: &TaskEnv<'_>,
    ledger: &Arc<Mutex<Ledger>>,
    started: std::time::Instant,
    report: &mut Reporter<'_>,
) -> Result<()> {
    let pending: Vec<(String, String)> = ledger
        .lock()
        .unwrap()
        .features()
        .iter()
        .filter(|feature| !feature.passes)
        .map(|feature| (feature.id.clone(), feature.verify.clone()))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    line(
        report,
        &format!(
            "== running the verify commands of the {} feature(s) that do not pass yet ==",
            pending.len()
        ),
    );
    let verifier = env.verifier();
    let mut passed = Vec::new();
    for (id, verify) in pending {
        // The tokens are spent already; only the time limit applies here.
        if let Some(reason) = env.budget.exceeded(0, started) {
            warn(
                report,
                &format!("{reason}; the other verify commands wait for the next run"),
            );
            break;
        }
        let output = tokio::select! {
            biased;
            _ = env.cancel.cancelled() => break,
            output = verifier.run(&verify) => output,
        };
        if !output.is_error {
            ledger.lock().unwrap().mark_passing(&id)?;
            passed.push(id);
        }
    }
    if !passed.is_empty() {
        line(
            report,
            &format!("verified by the harness: {}", passed.join(", ")),
        );
    }
    Ok(())
}

/// Why [`recheck_passing`] stopped before it checked every pass.
enum Unchecked {
    /// `env.cancel` was set.
    Cancelled,
    /// The budget ran out, for this reason.
    Budget(String),
}

/// Runs the verify command of every feature the list marks as passing and
/// takes back the passes that do not hold. The harness's copy of the list
/// is made from the file at the start of a run, and the file may say
/// things the harness never decided: a session killed before the harness
/// could undo its edits, or a list written by hand. Running the commands
/// again also catches features a later change broke.
///
/// The budget (`used` tokens, the run `started`) is checked before each
/// command, as each may take as long as the verify timeout. Returns why it
/// stopped, having changed nothing, if `env.cancel` was set or the budget
/// ran out first.
async fn recheck_passing(
    env: &TaskEnv<'_>,
    ledger: &Arc<Mutex<Ledger>>,
    used: u64,
    started: std::time::Instant,
    report: &mut Reporter<'_>,
) -> Result<Option<Unchecked>> {
    let marked: Vec<(String, String)> = ledger
        .lock()
        .unwrap()
        .features()
        .iter()
        .filter(|feature| feature.passes)
        .map(|feature| (feature.id.clone(), feature.verify.clone()))
        .collect();
    if marked.is_empty() {
        return Ok(None);
    }
    line(
        report,
        &format!(
            "== checking the {} feature(s) marked as passing ==",
            marked.len()
        ),
    );
    let verifier = env.verifier();
    let mut failed = Vec::new();
    for (id, verify) in marked {
        if let Some(detail) = env.budget.exceeded(used, started) {
            return Ok(Some(Unchecked::Budget(detail)));
        }
        let output = tokio::select! {
            biased;
            _ = env.cancel.cancelled() => return Ok(Some(Unchecked::Cancelled)),
            output = verifier.run(&verify) => output,
        };
        if output.is_error {
            failed.push(id);
        }
    }
    if failed.is_empty() {
        return Ok(None);
    }
    {
        let mut ledger = ledger.lock().unwrap();
        for id in &failed {
            ledger.mark_failing(id)?;
        }
    }
    let text = format!(
        "marked as passing, but their verify commands fail: {}; \
         they count as not passing",
        failed.join(", ")
    );
    warn(report, &text);
    append_progress(env.root.as_path(), &format!("run start: {text}"))?;
    Ok(None)
}

/// Checks the list a new task starts from and returns its length. Besides
/// its form, every verify command must fail now: one that passes before
/// any work (`true`, a test filter that matches no test, a pipe into
/// `tail`) would pass a feature nobody built.
async fn check_new_list(
    root: &Path,
    verifier: &Verifier,
    report: &mut Reporter<'_>,
) -> Result<usize> {
    let list = features::load(root)?;
    features::check_fresh(&list)?;
    line(
        report,
        &format!(
            "== checking that the {} verify commands fail before any work ==",
            list.len()
        ),
    );
    let mut passing = Vec::new();
    for feature in &list {
        if !verifier.run(&feature.verify).await.is_error {
            passing.push(format!("{} (`{}`)", feature.id, feature.verify));
        }
    }
    if !passing.is_empty() {
        bail!(
            "these verify commands already pass before any work: {}; a verify \
             command must fail until its feature is built",
            passing.join(", ")
        );
    }
    Ok(list.len())
}

/// Creates `.junees/` and keeps the harness state out of git: adds what
/// [`IGNORED`] lists to `.junees/.gitignore` if it is not there (a session
/// deleted the file, or an older Junees wrote it). Returns whether the
/// file changed.
fn prepare_dir(root: &Path) -> Result<bool> {
    let dir = dir(root);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    let mut text = match fs::read_to_string(&ignore) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read {}", ignore.display()));
        }
    };
    let missing: Vec<&str> = IGNORED
        .iter()
        .copied()
        .filter(|pattern| !text.lines().any(|line| line.trim() == *pattern))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for pattern in missing {
        text.push_str(pattern);
        text.push('\n');
    }
    fs::write(&ignore, text).with_context(|| format!("cannot write {}", ignore.display()))?;
    Ok(true)
}

/// Adds the [`BUILD_OUTPUT`] folders git does not ignore yet to the
/// repository's `.gitignore`, so the init commit carries them. A folder
/// that already holds tracked files is the project's own and stays as it
/// is.
fn ignore_build_output(root: &Path, report: &mut Reporter<'_>) -> Result<()> {
    let mut missing = Vec::new();
    for folder in BUILD_OUTPUT {
        if !git::is_ignored(root, folder)? && !git::tracks_any(root, folder)? {
            missing.push(*folder);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let ignore = root.join(".gitignore");
    let mut text = match fs::read_to_string(&ignore) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("cannot read {}", ignore.display()));
        }
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for folder in &missing {
        text.push_str(folder);
        text.push('\n');
    }
    fs::write(&ignore, text).with_context(|| format!("cannot write {}", ignore.display()))?;
    line(
        report,
        &format!(
            "build output kept out of git (.gitignore): {}",
            missing.join(" ")
        ),
    );
    Ok(())
}

/// The error of a run or init that finds the work tree dirty. When new
/// files the harness does not commit are among the changes (it left them
/// out of an earlier commit, or would leave them out), it names a few and
/// says why, since committing everything would not clean the tree.
fn dirty_tree(root: &Path, pathspec: &[&str]) -> anyhow::Error {
    let left_out = git::untracked_left_out(root, pathspec).unwrap_or_default();
    if left_out.is_empty() {
        return anyhow!(DIRTY_TREE);
    }
    let mut named: Vec<String> = left_out
        .iter()
        .take(NAMED_LEFT_OUT)
        .map(ToString::to_string)
        .collect();
    if left_out.len() > NAMED_LEFT_OUT {
        named.push(format!("and {} more", left_out.len() - NAMED_LEFT_OUT));
    }
    anyhow!(
        "{DIRTY_TREE}. Task mode does not commit new files whose names look like \
         secrets or that are larger than 1 MiB, so these stay uncommitted: {}; \
         commit them yourself, add them to .gitignore, or move them away",
        named.join(", ")
    )
}

/// Adds one harness line to progress.md.
fn append_progress(root: &Path, text: &str) -> Result<()> {
    let path = progress_path(root);
    let needs_newline = fs::read(&path).is_ok_and(|data| data.last().is_some_and(|&b| b != b'\n'));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let lead = if needs_newline { "\n" } else { "" };
    file.write_all(format!("{lead}- [junees] {text}\n").as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))
}

fn tokens(usage: Usage) -> String {
    format!(
        "tokens in {} out {}",
        usage.input_tokens, usage.output_tokens
    )
}

fn journal_note(journal: &Option<String>) -> String {
    match journal {
        Some(id) => format!(", journal {id}"),
        None => ", not saved".to_string(),
    }
}

fn warn_left_out(report: &mut Reporter<'_>, left_out: &[git::LeftOut]) {
    if !left_out.is_empty() {
        let files: Vec<String> = left_out.iter().map(ToString::to_string).collect();
        warn(
            report,
            &format!(
                "not committed: {} (commit them yourself if they belong in git, \
                 or add them to .gitignore)",
                files.join(", ")
            ),
        );
    }
}

fn line(report: &mut Reporter<'_>, text: &str) {
    report(Report::Line(text));
}

fn warn(report: &mut Reporter<'_>, text: &str) {
    report(Report::Event(Event::Warning(text)));
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::message::{ContentBlock, Message, StopReason as ModelStop};
    use crate::provider::{CompletionRequest, CompletionResponse, ProviderError};
    use crate::task::git::testing::{repo, subjects};

    type Reply = std::result::Result<CompletionResponse, ProviderError>;
    /// A request's system prompt and messages.
    type Seen = (String, Vec<Message>);

    /// Replays canned responses, shared by every session it makes, and
    /// records each request's system prompt and messages. Each call moves
    /// `clock` on by `tick` seconds.
    #[derive(Clone, Default)]
    struct Scripted {
        responses: Arc<Mutex<VecDeque<Reply>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
        clock: Arc<AtomicU64>,
        tick: u64,
    }

    impl Scripted {
        fn new(responses: Vec<Reply>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                ..Self::default()
            }
        }

        fn left(&self) -> usize {
            self.responses.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Provider for Scripted {
        async fn complete(
            &self,
            request: CompletionRequest<'_>,
        ) -> Result<CompletionResponse, ProviderError> {
            self.clock.fetch_add(self.tick, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push((request.system.to_string(), request.messages.to_vec()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("no scripted response left")
        }
    }

    fn text(text: &str) -> Reply {
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: ModelStop::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 1,
                ..Usage::default()
            },
        })
    }

    /// One response calling the given tools in order.
    fn calls(calls: &[(&str, Value)]) -> Reply {
        Ok(CompletionResponse {
            content: calls
                .iter()
                .enumerate()
                .map(|(i, (name, input))| ContentBlock::ToolUse {
                    id: format!("toolu_{i}"),
                    name: name.to_string(),
                    input: input.clone(),
                })
                .collect(),
            stop_reason: ModelStop::ToolUse,
            usage: Usage {
                input_tokens: 5,
                output_tokens: 2,
                ..Usage::default()
            },
        })
    }

    fn write(path: &Path, content: &str) -> (&'static str, Value) {
        (
            "write_file",
            json!({"path": path.to_string_lossy(), "content": content}),
        )
    }

    fn feature_list(verifies: &[&str]) -> String {
        let list: Vec<Value> = verifies
            .iter()
            .enumerate()
            .map(|(i, verify)| {
                json!({"id": format!("F{}", i + 1), "description": format!("feature {}", i + 1), "verify": verify, "passes": false})
            })
            .collect();
        Value::from(list).to_string()
    }

    /// A task environment over `root` whose sessions get their provider
    /// from `factory` and save their journals under `home`.
    fn env<'a>(root: &Path, home: &Path, factory: &'a ProviderFactory<'a>) -> TaskEnv<'a> {
        TaskEnv {
            root: root.to_path_buf(),
            home: Some(home.to_path_buf()),
            model: "m".into(),
            max_tokens: 100,
            max_turns: 10,
            tool_settings: ToolSettings::default(),
            context_window: 200_000,
            compact_threshold: 0.8,
            provider: factory,
            notify: Notifier::default(),
            budget: Budget::default(),
            cancel: Cancel::new(),
            claude: None,
        }
    }

    fn factory(provider: &Scripted) -> impl Fn() -> Result<Box<dyn Provider>> + '_ {
        move || Ok(Box::new(provider.clone()) as Box<dyn Provider>)
    }

    /// Collects the status lines and warnings a runner reports.
    #[derive(Default)]
    struct Collected {
        lines: Vec<String>,
        warnings: Vec<String>,
    }

    impl Collected {
        fn reporter(&mut self) -> impl FnMut(Report<'_>) + '_ {
            |report| match report {
                Report::Line(text) => self.lines.push(text.to_string()),
                Report::Event(Event::Warning(text)) => self.warnings.push(text.to_string()),
                Report::Event(_) => {}
            }
        }
    }

    #[tokio::test]
    async fn init_plans_and_commits_the_task() {
        let repo = repo();
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![
            calls(&[
                write(
                    &features_path(root),
                    &feature_list(&["test -f a", "test -f b"]),
                ),
                write(&progress_path(root), "# Plan\n\nTwo features."),
            ]),
            text("planned"),
        ]);
        let factory = factory(&provider);
        let mut out = Collected::default();
        init(
            &env(root, home.path(), &factory),
            "  build it  ",
            &mut out.reporter(),
        )
        .await
        .unwrap();

        assert_eq!(subjects(root), [INIT_COMMIT]);
        assert!(!git::is_dirty(root).unwrap());
        let seen = provider.seen.lock().unwrap();
        assert!(seen[0].0.contains("initializer of a long-running task"));
        assert_eq!(seen[0].1[0].text(), "The goal of the task:\n\nbuild it");
        // The harness rewrote the list in its own format and noted the session.
        let list = fs::read_to_string(features_path(root)).unwrap();
        assert_eq!(list, features::to_json(&features::load(root).unwrap()));
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.ends_with(
                "Two features.\n- [junees] init: 2 features planned, tokens in 15 out 3\n"
            ),
            "{progress}"
        );
        // state.json stays out of git.
        let state = State::load(root).unwrap();
        assert_eq!(state.usage.input_tokens, 15);
        assert_eq!(state.goal.as_deref(), Some("build it"));
        assert!(super::super::state_path(root).exists());
        // Build output stays out of the harness's commits from the start.
        assert_eq!(
            git::show(root, "HEAD", ".gitignore").unwrap(),
            "target/\nnode_modules/\ndist/\nbuild/\n.venv/\n__pycache__/\n"
        );
        assert_eq!(
            out.lines[2],
            "build output kept out of git (.gitignore): target/ node_modules/ \
             dist/ build/ .venv/ __pycache__/"
        );
        assert!(
            out.lines[0].starts_with("== init session, journal "),
            "{:?}",
            out.lines
        );
        assert_eq!(
            out.lines[1],
            "== checking that the 2 verify commands fail before any work =="
        );
        assert_eq!(
            fs::read_dir(home.path().join("sessions")).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn init_lets_the_session_fix_a_bad_list_once() {
        let repo = repo();
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let bad = r#"[{"id": "F1", "description": "a", "verify": "true", "passes": false},
                      {"id": "F1", "description": "b", "verify": "", "passes": true}]"#;
        let provider = Scripted::new(vec![
            calls(&[write(&features_path(root), bad)]),
            text("planned"),
            calls(&[write(
                &features_path(root),
                &feature_list(&["test -f a", "test -f b"]),
            )]),
            text("fixed"),
        ]);
        let factory = factory(&provider);
        let mut out = Collected::default();
        init(
            &env(root, home.path(), &factory),
            "goal",
            &mut out.reporter(),
        )
        .await
        .unwrap();
        assert_eq!(provider.left(), 0);
        let seen = provider.seen.lock().unwrap();
        let fix = seen[2].1.last().unwrap().text();
        assert!(
            fix.starts_with("The harness rejected .junees/features.json"),
            "{fix}"
        );
        assert!(fix.contains("`F1` is used more than once"), "{fix}");
        assert!(fix.contains("empty verify"), "{fix}");
        assert_eq!(out.warnings.len(), 1);
        assert_eq!(subjects(root), [INIT_COMMIT]);
    }

    #[tokio::test]
    async fn init_rejects_verify_commands_that_already_pass() {
        let repo = repo();
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![
            calls(&[write(
                &features_path(root),
                &feature_list(&["true", "test -f a", "ls | tail -1"]),
            )]),
            text("planned"),
            calls(&[write(
                &features_path(root),
                &feature_list(&["test -f z", "test -f a", "test -f b"]),
            )]),
            text("fixed"),
        ]);
        let factory = factory(&provider);
        let mut out = Collected::default();
        init(
            &env(root, home.path(), &factory),
            "goal",
            &mut out.reporter(),
        )
        .await
        .unwrap();
        assert_eq!(provider.left(), 0);
        let seen = provider.seen.lock().unwrap();
        let fix = seen[2].1.last().unwrap().text();
        assert!(
            fix.contains("these verify commands already pass before any work: F1 (`true`), F3 ("),
            "{fix}"
        );
        assert!(!fix.contains("F2"), "{fix}");
        assert_eq!(subjects(root), [INIT_COMMIT]);
        // The verify commands ran in the repository and left nothing behind.
        assert!(!git::is_dirty(root).unwrap());
    }

    #[tokio::test]
    async fn init_gives_up_after_one_fix_and_commits_nothing() {
        let repo = repo();
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let passing = r#"[{"id": "F1", "description": "a", "verify": "true", "passes": true}]"#;
        let provider = Scripted::new(vec![
            calls(&[write(&features_path(root), passing)]),
            text("planned"),
            text("it is fine as it is"),
        ]);
        let factory = factory(&provider);
        let err = init(&env(root, home.path(), &factory), "goal", &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("still invalid after one fix"),
            "{err:#}"
        );
        assert!(format!("{err:#}").contains("these are true: F1"), "{err:#}");
        assert!(subjects(root).is_empty());

        // The rejected list is out of the way: no run starts from it, and
        // init can start over.
        assert!(
            format!("{err:#}").contains("feature list was moved to"),
            "{err:#}"
        );
        assert!(!features_path(root).exists());
        assert!(super::super::rejected_features_path(root).exists());
        let none = Scripted::new(vec![]);
        let none_factory = self::factory(&none);
        let err = run(&env(root, home.path(), &none_factory), 5, &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot load the feature list"),
            "{err:#}"
        );
        let again = Scripted::new(vec![
            calls(&[write(&features_path(root), &feature_list(&["test -f a"]))]),
            text("planned"),
        ]);
        let again_factory = self::factory(&again);
        init(&env(root, home.path(), &again_factory), "goal", &mut |_| {})
            .await
            .unwrap();
        assert_eq!(subjects(root), [INIT_COMMIT]);
        // The rejected list stays out of git.
        assert!(!git::is_dirty(root).unwrap());
    }

    #[tokio::test]
    async fn init_refuses_folders_outside_git_and_existing_tasks() {
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let home = tempfile::tempdir().unwrap();

        let plain = tempfile::tempdir().unwrap();
        let err = init(
            &env(plain.path(), home.path(), &factory),
            "goal",
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("not inside a git repository"),
            "{err}"
        );
        assert!(!dir(plain.path()).exists());

        let repo = repo();
        fs::create_dir_all(dir(repo.path())).unwrap();
        fs::write(features_path(repo.path()), "[]").unwrap();
        let err = init(
            &env(repo.path(), home.path(), &factory),
            "goal",
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already has a task"), "{err}");
        assert_eq!(provider.seen.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn init_and_run_refuse_a_work_tree_with_changes() {
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let home = tempfile::tempdir().unwrap();

        let fresh = repo();
        fs::write(fresh.path().join("wip.txt"), "unfinished").unwrap();
        let err = init(
            &env(fresh.path(), home.path(), &factory),
            "goal",
            &mut |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("uncommitted changes"), "{err}");
        assert!(!err.to_string().contains("stay uncommitted"), "{err}");
        assert!(!dir(fresh.path()).exists());

        let task = task_repo(&["false"]);
        let root = task.path();
        fs::write(root.join(".env"), "DB_PASSWORD=hunter2\n").unwrap();
        let err = run(&env(root, home.path(), &factory), 5, &mut |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("git stash -u"), "{err}");
        assert!(
            err.to_string()
                .contains("stay uncommitted: .env (the name looks like a secret); "),
            "{err}"
        );
        assert_eq!(subjects(root), [INIT_COMMIT]);
        assert_eq!(provider.seen.lock().unwrap().len(), 0);
        assert_eq!(State::load(root).unwrap().sessions, 0);
    }

    #[test]
    fn init_ignores_only_build_output_git_does_not_ignore_yet() {
        let repo = repo();
        let root = repo.path();
        fs::write(root.join(".gitignore"), "/target\n*.log").unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/gen.sh"), "true").unwrap();
        git::commit_all(root, "project").unwrap();
        let mut out = Collected::default();
        ignore_build_output(root, &mut out.reporter()).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            "/target\n*.log\nnode_modules/\ndist/\n.venv/\n__pycache__/\n"
        );
        // Nothing left to add: the file stays as it is.
        let mut out = Collected::default();
        ignore_build_output(root, &mut out.reporter()).unwrap();
        assert!(out.lines.is_empty(), "{:?}", out.lines);
    }

    #[test]
    fn dirty_tree_names_a_few_left_out_files_and_counts_the_rest() {
        let repo = repo();
        let root = repo.path();
        for name in [
            ".env.a",
            ".env.b",
            ".env.c",
            ".env.d",
            ".env.e",
            "notes.txt",
        ] {
            fs::write(root.join(name), "x").unwrap();
        }
        let err = dirty_tree(root, &[]).to_string();
        assert!(
            err.contains(
                "stay uncommitted: .env.a (the name looks like a secret), \
                 .env.b (the name looks like a secret), \
                 .env.c (the name looks like a secret), and 2 more; "
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn large_files_a_verify_leaves_stay_out_and_the_next_run_says_why() {
        let repo = task_repo_marked(&[(
            "echo log > small.txt && head -c 2000000 /dev/zero > big.bin",
            true,
        )]);
        let root = repo.path();
        let mut out = Collected::default();
        run_task(root, &Scripted::new(vec![]), 0, &mut out).await;

        // The recheck commit takes the small file and leaves the large one.
        assert_eq!(subjects(root)[0], RECHECK_COMMIT);
        assert_eq!(git::show(root, "HEAD", "small.txt").unwrap(), "log\n");
        assert!(git::show(root, "HEAD", "big.bin").is_err());
        assert!(root.join("big.bin").exists());
        assert!(
            out.warnings.iter().any(|warning| warning.starts_with(
                "not committed: big.bin (1.9 MiB, over the 1 MiB limit for new files)"
            )),
            "{:?}",
            out.warnings
        );

        // The next run refuses the tree and names the file and the reason.
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let home = tempfile::tempdir().unwrap();
        let err = run(&env(root, home.path(), &factory), 1, &mut |_| {})
            .await
            .unwrap_err();
        let err = err.to_string();
        assert!(err.contains("uncommitted changes"), "{err}");
        assert!(
            err.contains(
                "stay uncommitted: big.bin (1.9 MiB, over the 1 MiB limit for new files); "
            ),
            "{err}"
        );
    }

    fn mark(id: &str) -> (&'static str, Value) {
        ("mark_feature_passing", json!({"id": id}))
    }

    /// A repository with a committed task whose features have these verify
    /// commands.
    fn task_repo(verifies: &[&str]) -> tempfile::TempDir {
        let repo = repo();
        let root = repo.path();
        prepare_dir(root).unwrap();
        let list = features::parse(&feature_list(verifies)).unwrap();
        fs::write(features_path(root), features::to_json(&list)).unwrap();
        fs::write(progress_path(root), "# Plan\n").unwrap();
        git::commit_all(root, INIT_COMMIT).unwrap();
        repo
    }

    async fn run_task(
        root: &Path,
        provider: &Scripted,
        max_sessions: usize,
        out: &mut Collected,
    ) -> Stop {
        let home = tempfile::tempdir().unwrap();
        let factory = factory(provider);
        run(
            &env(root, home.path(), &factory),
            max_sessions,
            &mut out.reporter(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn sessions_pass_features_until_all_pass() {
        let repo = task_repo(&["true", "test -f b.txt"]);
        let root = repo.path();
        let provider = Scripted::new(vec![
            calls(&[mark("F1")]),
            text("F1 works"),
            calls(&[write(&root.join("b.txt"), "b"), mark("F2")]),
            text("F2 works"),
        ]);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 5, &mut out).await;

        assert_eq!(stop.reason, StopReason::Complete);
        assert_eq!(stop.session, 2);
        assert_eq!(provider.left(), 0);
        assert!(features::load(root).unwrap().iter().all(|f| f.passes));
        assert_eq!(
            subjects(root),
            [
                checkpoint_message(2),
                checkpoint_message(1),
                INIT_COMMIT.to_string()
            ]
        );
        assert!(!git::is_dirty(root).unwrap());
        let state = State::load(root).unwrap();
        assert_eq!((state.sessions, state.stalled), (2, 0));
        assert_eq!(state.stop, Some(stop));
        assert!(state.stopped_at.is_some());
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains("- [junees] session 1: 1/2 passing, new: F1, tokens in 15 out 3\n")
        );
        assert!(progress.contains("- [junees] session 2: 2/2 passing, new: F2, "));
        assert!(out.lines[0].starts_with("== session 1 start: 0/2 passing, journal "));
        assert!(
            out.lines
                .contains(&"== session 2 done: 2/2 passing, new: F2 ==".to_string())
        );

        // The second session saw the first one's result and commit.
        {
            let seen = provider.seen.lock().unwrap();
            let system = &seen[2].0;
            assert!(system.contains("work session 2"));
            assert!(system.contains("- [x] F1: feature 1"));
            assert!(system.contains("- [ ] F2: feature 2"));
            assert!(system.contains(&format!(" {}\n", checkpoint_message(1))));
            assert_eq!(seen[2].1[0].text(), prompt::WORK_INPUT);
        }

        // Nothing is left to do, so another run starts no session.
        let again = run_task(root, &provider, 5, &mut Collected::default()).await;
        assert_eq!(again.reason, StopReason::Complete);
        assert_eq!(State::load(root).unwrap().sessions, 2);
    }

    #[tokio::test]
    async fn edits_to_the_feature_list_are_undone() {
        let repo = task_repo(&["false", "false"]);
        let root = repo.path();
        let original = fs::read_to_string(features_path(root)).unwrap();
        let forged = original.replace("\"passes\": false", "\"passes\": true");
        let provider = Scripted::new(vec![
            calls(&[write(&features_path(root), &forged)]),
            text("all done!"),
        ]);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 1, &mut out).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(fs::read_to_string(features_path(root)).unwrap(), original);
        assert!(!git::is_dirty(root).unwrap());
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(progress.contains("session 1: .junees/features.json was edited directly"));
        assert!(progress.contains("session 1: 0/2 passing, nothing new"));
    }

    /// A repository whose committed task has these features, each
    /// `(verify, passes)`.
    fn task_repo_marked(features: &[(&str, bool)]) -> tempfile::TempDir {
        let verifies: Vec<&str> = features.iter().map(|(verify, _)| *verify).collect();
        let repo = task_repo(&verifies);
        let root = repo.path();
        let mut list = features::load(root).unwrap();
        for (feature, (_, passes)) in list.iter_mut().zip(features) {
            feature.passes = *passes;
        }
        fs::write(features_path(root), features::to_json(&list)).unwrap();
        git::commit_all(root, "mark by hand").unwrap();
        repo
    }

    #[tokio::test]
    async fn a_run_believes_only_passes_whose_verify_still_passes() {
        let repo = task_repo_marked(&[("true", true), ("false", true), ("false", false)]);
        let root = repo.path();
        let provider = Scripted::new(vec![text("looking around")]);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 1, &mut out).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(provider.left(), 0);
        let passes: Vec<bool> = features::load(root)
            .unwrap()
            .iter()
            .map(|f| f.passes)
            .collect();
        assert_eq!(passes, [true, false, false]);
        assert_eq!(
            out.lines[0],
            "== checking the 2 feature(s) marked as passing =="
        );
        assert_eq!(
            out.warnings,
            ["marked as passing, but their verify commands fail: F2; they count as not passing"]
        );
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains(
                "- [junees] run start: marked as passing, but their verify commands fail: F2;"
            ),
            "{progress}"
        );
        // The session saw the corrected list.
        let seen = provider.seen.lock().unwrap();
        assert!(seen[0].0.contains("- [ ] F2: feature 2"));
    }

    #[tokio::test]
    async fn a_list_marked_passing_by_hand_does_not_complete_the_task() {
        let repo = task_repo_marked(&[("false", true), ("false", true)]);
        let provider = Scripted::new(vec![text("hm")]);
        let stop = run_task(repo.path(), &provider, 1, &mut Collected::default()).await;
        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(provider.left(), 0);
    }

    #[tokio::test]
    async fn changes_left_behind_get_a_checkpoint_commit() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let provider = Scripted::new(vec![
            calls(&[write(&root.join("src/half.rs"), "fn half() {}")]),
            text("ran out of ideas"),
        ]);
        let mut out = Collected::default();
        run_task(root, &provider, 1, &mut out).await;

        assert_eq!(subjects(root)[0], checkpoint_message(1));
        assert!(!git::is_dirty(root).unwrap());
        let mut ls_files = std::process::Command::new("git");
        ls_files
            .args(["ls-files", "src/half.rs", ".junees"])
            .current_dir(root);
        for name in git::REPO_ENV_VARS {
            ls_files.env_remove(name);
        }
        let tracked = ls_files.output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&tracked.stdout),
            ".junees/.gitignore\n.junees/features.json\n.junees/progress.md\nsrc/half.rs\n"
        );
        assert!(
            out.lines
                .contains(&format!("checkpoint commit: {}", checkpoint_message(1)))
        );
    }

    #[tokio::test]
    async fn checkpoints_leave_files_that_look_like_secrets_out() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let provider = Scripted::new(vec![
            calls(&[
                write(&root.join(".env"), "DB_PASSWORD=hunter2\n"),
                write(&root.join("app.rs"), "fn main() {}"),
            ]),
            text("done for now"),
            text("nothing to add"),
        ]);
        let mut out = Collected::default();
        run_task(root, &provider, 2, &mut out).await;

        assert_eq!(
            subjects(root),
            [
                checkpoint_message(2),
                checkpoint_message(1),
                INIT_COMMIT.to_string()
            ]
        );
        let mut ls_files = std::process::Command::new("git");
        ls_files
            .args(["ls-files", ".env", "app.rs"])
            .current_dir(root);
        for name in git::REPO_ENV_VARS {
            ls_files.env_remove(name);
        }
        let tracked = ls_files.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&tracked.stdout), "app.rs\n");
        assert!(root.join(".env").exists());
        assert_eq!(out.warnings.len(), 2, "{:?}", out.warnings);
        assert!(
            out.warnings[0].contains("not committed: .env (the name looks like a secret) "),
            "{:?}",
            out.warnings
        );
    }

    #[tokio::test]
    async fn stops_at_max_sessions() {
        let repo = task_repo(&["false", "false"]);
        let provider = Scripted::new(vec![text("thinking"), text("still thinking")]);
        let stop = run_task(repo.path(), &provider, 2, &mut Collected::default()).await;
        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(stop.session, 2);
        assert_eq!(provider.left(), 0);
        assert_eq!(State::load(repo.path()).unwrap().stalled, 2);
    }

    #[tokio::test]
    async fn stops_when_sessions_stall() {
        let repo = task_repo(&["true", "false"]);
        let provider = Scripted::new(vec![
            calls(&[mark("F1")]),
            text("one done"),
            calls(&[mark("F2")]),
            text("F2 does not pass yet"),
            text("no idea"),
            text("still no idea"),
        ]);
        let stop = run_task(repo.path(), &provider, 10, &mut Collected::default()).await;
        assert_eq!(stop.reason, StopReason::Stalled);
        assert_eq!(stop.session, 4);
        assert_eq!(provider.left(), 0);
        assert!(stop.to_string().starts_with("stalled"));
    }

    #[tokio::test]
    async fn a_failed_session_stops_the_run_and_is_recorded() {
        let repo = task_repo(&["false", "false"]);
        let root = repo.path();
        let provider = Scripted::new(vec![
            calls(&[write(&root.join("wip.txt"), "wip")]),
            Err(ProviderError::Transport("network down".into())),
        ]);
        let stop = run_task(root, &provider, 5, &mut Collected::default()).await;

        assert_eq!(stop.reason, StopReason::Error);
        assert_eq!(stop.session, 1);
        assert!(stop.detail.as_deref().unwrap().contains("network down"));
        assert!(stop.reason.is_failure());
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains("- [junees] session 1: failed: "),
            "{progress}"
        );
        assert!(progress.contains("network down"));
        // The work in progress is kept in a checkpoint.
        assert_eq!(subjects(root)[0], checkpoint_message(1));
        assert_eq!(State::load(root).unwrap().stop, Some(stop));
    }

    #[tokio::test]
    async fn a_session_at_the_turn_limit_ends_and_the_run_goes_on() {
        let repo = task_repo(&["false", "false"]);
        let root = repo.path();
        // max_turns is 10 in these tests: ten replies that keep working.
        let mut replies: Vec<Reply> = (0..10)
            .map(|i| calls(&[write(&root.join(format!("step{i}.txt")), "x")]))
            .collect();
        replies.push(text("picking up from step 9"));
        let provider = Scripted::new(replies);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 2, &mut out).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(stop.session, 2);
        assert_eq!(provider.left(), 0);
        assert_eq!(State::load(root).unwrap().stalled, 2);
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains(
                "- [junees] session 1: 0/2 passing, nothing new, \
                 ended at the turn limit (10 model calls), "
            ),
            "{progress}"
        );
        // The work is kept.
        assert_eq!(subjects(root)[1], checkpoint_message(1));
        assert!(root.join("step9.txt").exists());
    }

    #[tokio::test]
    async fn a_session_that_deletes_the_task_folder_does_not_break_the_run() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let original = fs::read_to_string(features_path(root)).unwrap();
        let provider = Scripted::new(vec![
            calls(&[bash(format!("rm -rf '{}'", dir(root).display()))]),
            text("cleaned up"),
        ]);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 1, &mut out).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(fs::read_to_string(features_path(root)).unwrap(), original);
        assert!(dir(root).join(".gitignore").exists());
        assert!(!git::is_dirty(root).unwrap());
        assert_eq!(subjects(root)[0], checkpoint_message(1));
        // state.json stays out of git.
        let mut ls_files = std::process::Command::new("git");
        ls_files.args(["ls-files", ".junees"]).current_dir(root);
        for name in git::REPO_ENV_VARS {
            ls_files.env_remove(name);
        }
        let tracked = ls_files.output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&tracked.stdout),
            ".junees/.gitignore\n.junees/features.json\n.junees/progress.md\n"
        );
        assert_eq!(State::load(root).unwrap().stop, Some(stop));
    }

    #[tokio::test]
    async fn session_bash_follows_the_users_deny_rules() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![
            calls(&[bash(format!("cd '{}' && touch denied.txt", root.display()))]),
            text("blocked"),
        ]);
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        env.tool_settings.deny_commands = vec!["touch".into()];
        run(&env, 1, &mut |_| {}).await.unwrap();

        assert!(!root.join("denied.txt").exists());
        let seen = provider.seen.lock().unwrap();
        let result = seen[1].1.last().unwrap();
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &result.content[0]
        else {
            panic!("expected a tool result: {result:?}");
        };
        assert!(is_error);
        assert!(content.starts_with("blocked by policy"), "{content}");
    }

    fn bash(command: String) -> (&'static str, Value) {
        ("bash", json!({ "command": command }))
    }

    #[tokio::test]
    async fn a_failed_checkpoint_commit_stops_the_run_and_is_recorded() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        // The session leaves the repository unable to commit.
        let provider = Scripted::new(vec![
            calls(&[bash(format!(
                "cd '{}' && git config commit.gpgsign true && git config gpg.program false \
                 && echo wip > wip.txt",
                root.display()
            ))]),
            text("done"),
        ]);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 3, &mut out).await;

        assert_eq!(stop.reason, StopReason::Error);
        assert_eq!(stop.session, 1);
        let detail = stop.detail.as_deref().unwrap();
        assert!(
            detail.starts_with("could not wrap up session 1: "),
            "{detail}"
        );
        assert!(detail.contains("git commit"), "{detail}");
        assert_eq!(State::load(root).unwrap().stop, Some(stop));
        assert_eq!(out.warnings.len(), 1, "{:?}", out.warnings);
    }

    #[tokio::test]
    async fn the_token_budget_ends_the_session_and_counts_over_runs() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        // 7 tokens a call; the budget is checked before each call.
        let provider = Scripted::new(
            ["a", "b", "c", "d"]
                .iter()
                .map(|name| calls(&[write(&root.join(name), name)]))
                .collect(),
        );
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        env.budget.max_tokens = Some(20);
        let mut out = Collected::default();
        let stop = run(&env, 5, &mut out.reporter()).await.unwrap();

        assert_eq!(stop.reason, StopReason::Budget);
        assert_eq!(stop.session, 1);
        assert_eq!(
            stop.detail.as_deref(),
            Some("token budget used up: 21 of 20 tokens")
        );
        assert!(!stop.reason.is_failure());
        assert_eq!(provider.left(), 1);
        // The session's work is in its checkpoint.
        assert_eq!(subjects(root)[0], checkpoint_message(1));
        assert!(root.join("c").exists());
        assert!(!git::is_dirty(root).unwrap());
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains(
                "- [junees] session 1: 0/1 passing, nothing new, ended early: \
                 token budget used up: 21 of 20 tokens, tokens in 15 out 6\n"
            ),
            "{progress}"
        );
        let state = State::load(root).unwrap();
        assert_eq!(state.stop, Some(stop));
        assert_eq!(state.usage.input_tokens + state.usage.output_tokens, 21);

        // Tokens add up over runs: the next run starts no session.
        let stop = run(&env, 5, &mut |_| {}).await.unwrap();
        assert_eq!(stop.reason, StopReason::Budget);
        assert_eq!(stop.session, 1);
        assert_eq!(provider.left(), 1);
        assert_eq!(State::load(root).unwrap().sessions, 1);
    }

    #[tokio::test]
    async fn the_time_budget_stops_the_run_between_calls() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        // Each model call takes 40 seconds of the fake clock.
        let mut provider = Scripted::new(vec![
            calls(&[write(&root.join("a"), "a")]),
            calls(&[write(&root.join("b"), "b")]),
            text("never asked for"),
        ]);
        provider.tick = 40;
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        env.budget.max_time = Some(Duration::from_secs(60));
        env.budget.clock = budget::testing::fake_clock(provider.clock.clone());
        let stop = run(&env, 5, &mut |_| {}).await.unwrap();

        assert_eq!(stop.reason, StopReason::Budget);
        assert_eq!(
            stop.to_string(),
            "time budget used up: 1m20s of 1m",
            "{stop:?}"
        );
        assert_eq!(provider.left(), 1);
        assert_eq!(subjects(root)[0], checkpoint_message(1));
        assert!(root.join("b").exists());

        // Time counts per run: a new run gets the full minute again.
        let stop = run(&env, 1, &mut |_| {}).await.unwrap();
        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(provider.left(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_session_is_checkpointed_and_the_run_stops() {
        let repo = task_repo(&["false", "false"]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let pid_file = home.path().join("pid");
        let provider = Scripted::new(vec![calls(&[
            write(&root.join("wip.txt"), "half done"),
            bash(format!(
                "sleep 30 & echo $! > '{}'; wait",
                pid_file.display()
            )),
            write(&root.join("never.txt"), "x"),
        ])]);
        let factory = factory(&provider);
        let env = env(root, home.path(), &factory);
        let cancel = env.cancel.clone();
        let canceller = tokio::spawn(async move {
            loop {
                let text = fs::read_to_string(&pid_file).unwrap_or_default();
                if let Ok(pid) = text.trim().parse::<u32>() {
                    cancel.cancel();
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let mut out = Collected::default();
        let stop = run(&env, 5, &mut out.reporter()).await.unwrap();
        let pid = canceller.await.unwrap();

        assert_eq!(stop.reason, StopReason::Interrupted);
        assert_eq!(stop.session, 1);
        assert!(!stop.reason.is_failure());
        assert!(
            crate::tools::testing::gone_soon(pid).await,
            "{pid} survived"
        );
        assert!(!root.join("never.txt").exists());
        assert_eq!(subjects(root)[0], checkpoint_message(1));
        assert!(!git::is_dirty(root).unwrap());
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains("- [junees] session 1: 0/2 passing, nothing new, interrupted, "),
            "{progress}"
        );
        assert_eq!(State::load(root).unwrap().stop, Some(stop));
        // No model call after the cancel, and the journal answers the
        // cancelled calls.
        assert_eq!(provider.seen.lock().unwrap().len(), 1);
        let journal: String = fs::read_dir(home.path().join("sessions"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert_eq!(
            journal.matches(crate::agent::CANCELLED).count(),
            2,
            "{journal}"
        );

        // Still set: the next run stops before starting a session.
        let stop = run(&env, 5, &mut |_| {}).await.unwrap();
        assert_eq!(stop.reason, StopReason::Interrupted);
        assert_eq!(stop.session, 1);
    }

    #[tokio::test]
    async fn a_run_marks_itself_running_and_refuses_a_second_one() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let provider = Scripted::new(vec![
            calls(&[bash(format!(
                "cat '{}'",
                super::super::state_path(root).display()
            ))]),
            text("looked"),
        ]);
        run_task(root, &provider, 1, &mut Collected::default()).await;
        // While the session ran, state.json named this process.
        let result = provider.seen.lock().unwrap()[1].1[2].content[0].clone();
        let ContentBlock::ToolResult { content, .. } = result else {
            panic!("expected the tool result: {result:?}");
        };
        assert!(
            content.contains(&format!("\"pid\": {}", std::process::id())),
            "{content}"
        );
        // It is cleared at the stop, and the copy of the list is kept.
        let state = State::load(root).unwrap();
        assert_eq!(state.running, None);
        assert_eq!(state.features, Some(features::load(root).unwrap()));

        // Another run holds the lock: refused, nothing touched.
        let mut state = State::load(root).unwrap();
        state.running = Some(Running {
            pid: 4242,
            started: 1_794_205_024,
        });
        state.save(root).unwrap();
        let held = RunLock::acquire(root).unwrap().unwrap();
        let none = Scripted::new(vec![]);
        let home = tempfile::tempdir().unwrap();
        let none_factory = factory(&none);
        let err = run(&env(root, home.path(), &none_factory), 5, &mut |_| {})
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "another `junees task run` (pid 4242, started 2026-11-09 06:17 UTC) is working \
             on this task; wait for it to stop"
        );
        assert_eq!(State::load(root).unwrap(), state);
        assert_eq!(subjects(root)[0], checkpoint_message(1));
        drop(held);
    }

    #[tokio::test]
    async fn a_crashed_run_is_recovered_before_the_next_one() {
        let repo = task_repo(&["false", "test -f b.txt"]);
        let root = repo.path();
        let provider = Scripted::new(vec![text("looking"), text("next")]);
        run_task(root, &provider, 1, &mut Collected::default()).await;

        // Session 2 crashed: its mark is left, the tree has its half-done
        // work, and features.json its edits (a verify that always passes,
        // and passes it never got from the harness).
        let mut state = State::load(root).unwrap();
        state.sessions = 2;
        state.stop = None;
        state.running = Some(Running {
            pid: 4242,
            started: 1_794_205_024,
        });
        state.save(root).unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        let forged = features::load(root)
            .unwrap()
            .into_iter()
            .map(|mut feature| {
                feature.passes = true;
                if feature.id == "F1" {
                    feature.verify = "true".into();
                }
                feature
            })
            .collect::<Vec<_>>();
        fs::write(features_path(root), features::to_json(&forged)).unwrap();

        let mut out = Collected::default();
        let stop = run_task(root, &provider, 1, &mut out).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(stop.session, 3);
        assert_eq!(provider.left(), 0);
        assert_eq!(
            subjects(root)[..4],
            [
                checkpoint_message(3),
                RECHECK_COMMIT.to_string(),
                RECOVER_COMMIT.to_string(),
                checkpoint_message(1)
            ]
        );
        assert!(!git::is_dirty(root).unwrap());
        // The verify came back from the init commit; F1's forged pass failed it,
        // F2's claim was checked with its own verify and holds.
        let list = features::load(root).unwrap();
        assert_eq!(list[0].verify, "false");
        let passes: Vec<bool> = list.iter().map(|f| f.passes).collect();
        assert_eq!(passes, [false, true]);
        assert!(
            out.warnings[0].starts_with(
                "the run started 2026-11-09 06:17 UTC (pid 4242) did not stop cleanly"
            ),
            "{:?}",
            out.warnings
        );
        assert!(
            out.lines
                .contains(&format!("recovery commit: {RECOVER_COMMIT}"))
        );
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains(
                "- [junees] recovered after a crash: the run started 2026-11-09 06:17 UTC \
                 stopped without a record during session 2; .junees/features.json was \
                 restored from the init commit "
            ),
            "{progress}"
        );
        let state = State::load(root).unwrap();
        assert_eq!(state.running, None);
        assert_eq!(state.stop, Some(stop));
    }

    /// Leaves the task in `root` as a crashed run would, with `copy` as the
    /// list copy in state.json.
    fn crash(root: &Path, copy: Option<Vec<features::Feature>>) {
        let state = State {
            running: Some(Running {
                pid: 4242,
                started: 1_794_205_024,
            }),
            features: copy,
            ..State::default()
        };
        state.save(root).unwrap();
    }

    /// The task's list with F1's verify replaced by one that always passes
    /// and every feature marked passing.
    fn forged_list(root: &Path) -> Vec<features::Feature> {
        features::load(root)
            .unwrap()
            .into_iter()
            .map(|mut feature| {
                feature.passes = true;
                if feature.id == "F1" {
                    feature.verify = "true".into();
                }
                feature
            })
            .collect()
    }

    #[tokio::test]
    async fn a_forged_copy_in_state_json_does_not_complete_the_task() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        // The session rewrote the copy in state.json, then killed the run.
        crash(root, Some(forged_list(root)));
        let provider = Scripted::new(vec![]);
        let mut out = Collected::default();
        let stop = run_task(root, &provider, 0, &mut out).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        let list = features::load(root).unwrap();
        assert_eq!(list[0].verify, "false");
        assert!(!list[0].passes);
        assert!(!git::is_dirty(root).unwrap());
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("does not match the task's init commit")),
            "{:?}",
            out.warnings
        );
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(
            progress.contains("did not match the task's init commit"),
            "{progress}"
        );
    }

    #[tokio::test]
    async fn a_crash_without_a_copy_still_restores_the_init_definitions() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        // The session forged features.json and dropped the copy.
        fs::write(features_path(root), features::to_json(&forged_list(root))).unwrap();
        crash(root, None);
        let provider = Scripted::new(vec![]);
        let stop = run_task(root, &provider, 0, &mut Collected::default()).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(features::load(root).unwrap()[0].verify, "false");
        // The recheck took the restored pass back in a commit of its own.
        assert_eq!(subjects(root)[..2], [RECHECK_COMMIT, RECOVER_COMMIT]);
        assert!(!git::is_dirty(root).unwrap());
    }

    #[tokio::test]
    async fn recovery_needs_the_init_commit() {
        // A task whose history lost the init commit (squashed, say).
        let repo = repo();
        let root = repo.path();
        prepare_dir(root).unwrap();
        let list = features::parse(&feature_list(&["false"])).unwrap();
        fs::write(features_path(root), features::to_json(&list)).unwrap();
        git::commit_all(root, "squashed").unwrap();
        crash(root, Some(list));
        let none = Scripted::new(vec![]);
        let home = tempfile::tempdir().unwrap();
        let factory = factory(&none);
        let err = run(&env(root, home.path(), &factory), 0, &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no `junees: initialize task` commit"),
            "{err:#}"
        );
        // Nothing was recovered, so the next try recovers again.
        assert!(State::load(root).unwrap().running.is_some());
    }

    #[tokio::test]
    async fn the_run_log_has_a_line_per_event_and_stays_out_of_git() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        // A task from before the run log: its .gitignore does not know it.
        fs::write(
            dir(root).join(".gitignore"),
            "state.json\n.*.tmp\n*.rejected.json\n",
        )
        .unwrap();
        git::commit_all(root, "older task").unwrap();
        let provider = Scripted::new(vec![text("hm"), text("again")]);
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        env.budget.max_tokens = Some(1000);
        env.budget.clock = budget::testing::fake_clock(provider.clock.clone());
        let stop = run(&env, 1, &mut |_| {}).await.unwrap();
        assert_eq!(stop.reason, StopReason::MaxSessions);

        assert_eq!(
            subjects(root)[..3],
            [
                checkpoint_message(1),
                IGNORE_COMMIT.to_string(),
                "older task".to_string()
            ]
        );
        assert!(!git::is_dirty(root).unwrap());
        let log = fs::read_to_string(run_log_path(root)).unwrap();
        let lines: Vec<&str> = log
            .lines()
            .map(|line| {
                let (stamp, text) = line.split_once(' ').unwrap();
                assert_eq!(stamp.len(), 20, "{line}");
                assert!(stamp.ends_with('Z') && &stamp[10..11] == "T", "{line}");
                text
            })
            .collect();
        assert_eq!(
            lines,
            [
                format!(
                    "run start (pid {}): 0/1 passing, task tokens 0 of 1000, run time 0s, \
                     max sessions 1",
                    std::process::id()
                )
                .as_str(),
                "session 1 start",
                "session 1 end: 0/1 passing, nothing new; task tokens 11 of 1000, run time 0s",
                "run stop (max_sessions): reached --max-sessions; task tokens 11 of 1000, \
                 run time 0s",
            ]
        );

        // The next run adds to it.
        run(&env, 1, &mut |_| {}).await.unwrap();
        let log = fs::read_to_string(run_log_path(root)).unwrap();
        assert_eq!(log.lines().count(), 8, "{log}");
        assert!(
            log.ends_with(" run stop (max_sessions): reached --max-sessions; task tokens 22 of 1000, run time 0s\n"),
            "{log}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_recheck_changes_nothing_and_completes_nothing() {
        // Both marked passing; F2's verify would take the full timeout.
        let repo = task_repo_marked(&[("true", true), ("sleep 30", true)]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let env = env(root, home.path(), &factory);
        let cancel = env.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let stop = run(&env, 5, &mut |_| {}).await.unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(stop.reason, StopReason::Interrupted);
        assert_eq!(stop.session, 0);
        assert!(!git::is_dirty(root).unwrap());
        assert_eq!(subjects(root)[0], "mark by hand");
        assert_eq!(State::load(root).unwrap().running, None);
    }

    #[tokio::test]
    async fn the_time_budget_stops_the_recheck_between_verify_commands() {
        let home = tempfile::tempdir().unwrap();
        let ran = home.path().join("ran");
        let second = format!("touch '{}'", ran.display());
        let repo = task_repo_marked(&[("true", true), (second.as_str(), true)]);
        let root = repo.path();
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        // Each reading of the clock is 40 seconds after the one before:
        // the run starts at 0s, F1 is checked at 40s, F2 would be at 80s.
        let readings = Arc::new(AtomicU64::new(0));
        let start = std::time::Instant::now();
        env.budget.clock = Arc::new(move || {
            start + Duration::from_secs(40 * readings.fetch_add(1, Ordering::SeqCst))
        });
        env.budget.max_time = Some(Duration::from_secs(60));
        let stop = run(&env, 0, &mut |_| {}).await.unwrap();

        assert_eq!(stop.reason, StopReason::Budget);
        assert_eq!(stop.session, 0);
        assert_eq!(
            stop.detail.as_deref(),
            Some("time budget used up: 1m20s of 1m")
        );
        assert!(!ran.exists());
        // The passes not checked yet stay as they were, unconfirmed.
        let passes: Vec<bool> = features::load(root)
            .unwrap()
            .iter()
            .map(|f| f.passes)
            .collect();
        assert_eq!(passes, [true, true]);
        assert!(!git::is_dirty(root).unwrap());
        assert_eq!(State::load(root).unwrap().stop, Some(stop));
    }

    /// After a run that stopped before any session: the work tree is clean,
    /// the start of the run is in its own commit, and the next run starts
    /// a session.
    async fn clean_and_next_run_starts(root: &Path) {
        assert!(!git::is_dirty(root).unwrap());
        assert_eq!(subjects(root)[0], RECHECK_COMMIT);
        assert_eq!(State::load(root).unwrap().sessions, 0);
        let provider = Scripted::new(vec![text("next")]);
        let stop = run_task(root, &provider, 1, &mut Collected::default()).await;
        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(stop.session, 1);
        assert_eq!(provider.left(), 0);
    }

    /// A reporter that runs `then` once the recheck takes a pass back.
    fn on_taken_back(mut then: impl FnMut()) -> impl FnMut(Report<'_>) {
        move |report| {
            if let Report::Event(Event::Warning(text)) = report
                && text.starts_with("marked as passing, but")
            {
                then();
            }
        }
    }

    #[tokio::test]
    async fn a_recheck_without_sessions_is_committed() {
        // F1's verify fails and leaves a file behind, as a build might.
        let repo = task_repo_marked(&[("touch built.txt && false", true), ("false", false)]);
        let root = repo.path();
        let provider = Scripted::new(vec![]);
        let stop = run_task(root, &provider, 0, &mut Collected::default()).await;

        assert_eq!(stop.reason, StopReason::MaxSessions);
        assert_eq!(stop.session, 0);
        let passes: Vec<bool> = features::load(root)
            .unwrap()
            .iter()
            .map(|f| f.passes)
            .collect();
        assert_eq!(passes, [false, false]);
        assert!(root.join("built.txt").exists());
        let progress = fs::read_to_string(progress_path(root)).unwrap();
        assert!(progress.contains("- [junees] run start: marked as passing, but"));
        clean_and_next_run_starts(root).await;
    }

    #[tokio::test]
    async fn a_signal_right_after_the_recheck_leaves_a_clean_tree() {
        let repo = task_repo_marked(&[("false", true)]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let env = env(root, home.path(), &factory);
        let cancel = env.cancel.clone();
        let stop = run(&env, 5, &mut on_taken_back(|| cancel.cancel()))
            .await
            .unwrap();

        assert_eq!(stop.reason, StopReason::Interrupted);
        assert_eq!(stop.session, 0);
        assert!(!features::load(root).unwrap()[0].passes);
        clean_and_next_run_starts(root).await;
    }

    #[tokio::test]
    async fn a_time_budget_used_up_by_the_recheck_leaves_a_clean_tree() {
        let repo = task_repo_marked(&[("false", true)]);
        let root = repo.path();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        let secs = Arc::new(AtomicU64::new(0));
        env.budget.clock = budget::testing::fake_clock(secs.clone());
        env.budget.max_time = Some(Duration::from_secs(60));
        // The last verify command takes the run past its minute.
        let stop = run(
            &env,
            5,
            &mut on_taken_back(|| secs.store(120, Ordering::SeqCst)),
        )
        .await
        .unwrap();

        assert_eq!(stop.reason, StopReason::Budget);
        assert_eq!(stop.session, 0);
        assert_eq!(
            stop.detail.as_deref(),
            Some("time budget used up: 2m of 1m")
        );
        assert!(!features::load(root).unwrap()[0].passes);
        clean_and_next_run_starts(root).await;
    }

    #[tokio::test]
    async fn a_token_budget_already_used_up_leaves_a_clean_tree() {
        // A list committed by hand in its own formatting, which the harness
        // rewrites in its own.
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let by_hand = feature_list(&["false"]).replace("\"passes\":false", "\"passes\":true");
        fs::write(features_path(root), &by_hand).unwrap();
        git::commit_all(root, "mark by hand").unwrap();
        let mut state = State::load(root).unwrap();
        state.usage.input_tokens = 50;
        state.save(root).unwrap();
        let home = tempfile::tempdir().unwrap();
        let provider = Scripted::new(vec![]);
        let factory = factory(&provider);
        let mut env = env(root, home.path(), &factory);
        env.budget.max_tokens = Some(20);
        let stop = run(&env, 5, &mut |_| {}).await.unwrap();

        assert_eq!(stop.reason, StopReason::Budget);
        assert_eq!(stop.session, 0);
        assert_eq!(
            stop.detail.as_deref(),
            Some("token budget used up: 50 of 20 tokens")
        );
        // Not rechecked, so the pass stays as it was, unconfirmed.
        assert_ne!(fs::read_to_string(features_path(root)).unwrap(), by_hand);
        assert!(features::load(root).unwrap()[0].passes);
        clean_and_next_run_starts(root).await;
    }

    #[tokio::test]
    async fn a_new_run_continues_from_state_json() {
        let repo = task_repo(&["false"]);
        let root = repo.path();
        let provider = Scripted::new(vec![text("a"), text("b"), text("c")]);
        run_task(root, &provider, 1, &mut Collected::default()).await;
        assert_eq!(State::load(root).unwrap().stalled, 1);

        let mut out = Collected::default();
        let stop = run_task(root, &provider, 1, &mut out).await;
        assert_eq!(stop.session, 2);
        assert!(
            out.lines[0].starts_with("== session 2 start"),
            "{:?}",
            out.lines
        );
        assert_eq!(subjects(root)[0], checkpoint_message(2));
        // The first run stopped on its own, so its streak was cleared.
        assert_eq!(State::load(root).unwrap().stalled, 1);

        // After a crash (no stop recorded) the streak carries on.
        let mut state = State::load(root).unwrap();
        state.stop = None;
        state.stalled = STALL_LIMIT - 1;
        state.save(root).unwrap();
        let stop = run_task(root, &provider, 5, &mut Collected::default()).await;
        assert_eq!(stop.reason, StopReason::Stalled);
        assert_eq!(stop.session, 3);
        assert_eq!(State::load(root).unwrap().usage.input_tokens, 30);
    }
}
