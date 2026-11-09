//! Runs task sessions: the init session that plans a task, and the work
//! sessions that build it one feature at a time.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};

use super::features::{self, Ledger};
use super::state::{State, Stop, StopReason};
use super::tools::{MarkFeaturePassing, Verifier, session_tools};
use super::{STALL_LIMIT, dir, features_path, git, progress_path, prompt};
use crate::agent::{Agent, AgentOptions, Event};
use crate::cli::preview;
use crate::message::Usage;
use crate::provider::Provider;
use crate::session::SessionWriter;
use crate::tools::ToolRegistry;

/// Commit message of the commit that starts a task.
pub const INIT_COMMIT: &str = "junees: initialize task";

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
    pub provider: &'a ProviderFactory<'a>,
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
        session_tools()
    }

    fn verifier(&self) -> Verifier {
        Verifier::new(self.root.clone())
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
            },
        );
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
             (continue it with `junees task run`)",
            path.display()
        );
    }
    prepare_dir(root)?;

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
    let count = match check_new_list(root) {
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
            check_new_list(root).context("the feature list is still invalid after one fix")?
        }
    };
    // Writes the list in the harness's own formatting.
    Ledger::load(root)?;

    let usage = agent.usage();
    let mut state = State::load(root)?;
    state.usage.add(usage);
    state.save(root)?;
    append_progress(
        root,
        &format!("init: {count} features planned, {}", tokens(usage)),
    )?;
    git::commit_all(root, INIT_COMMIT).context("cannot commit the new task")?;
    line(
        report,
        &format!("== init done: {count} features, committed; start with `junees task run` =="),
    );
    Ok(())
}

/// Runs work sessions on the task in `env.root` until every feature passes,
/// `max_sessions` sessions have run, [`STALL_LIMIT`] sessions in a row
/// passed no new feature, or a session fails. Returns why it stopped, which
/// is also recorded in state.json.
///
/// Every session is a new agent with a fresh context and its own journal.
/// After each one the harness puts its copy of the feature list back into
/// features.json, adds a line to progress.md and commits whatever the
/// session left uncommitted. Session numbers and counters continue from
/// state.json, so a run picks up where the last one (or a crash) left off.
pub async fn run(
    env: &TaskEnv<'_>,
    max_sessions: usize,
    report: &mut Reporter<'_>,
) -> Result<Stop> {
    let root = &env.root;
    git::toplevel(root)?;
    let ledger = Ledger::load(root)
        .context("cannot load the feature list (start a task with `junees task init`)")?;
    let ledger = Arc::new(Mutex::new(ledger));
    prepare_dir(root)?;
    let mut state = State::load(root)?;
    // A run that stopped on its own does not hold its stall streak against
    // the next one; after a crash (no stop recorded) the streak carries on.
    if state.stop.take().is_some() {
        state.stalled = 0;
    }
    state.save(root)?;

    let mut ran = 0;
    let stop = loop {
        let stop = |reason, session| Stop {
            reason,
            session,
            detail: None,
        };
        if ledger.lock().unwrap().all_pass() {
            break stop(StopReason::Complete, state.sessions);
        }
        if ran == max_sessions {
            break stop(StopReason::MaxSessions, state.sessions);
        }
        ran += 1;
        state.sessions += 1;
        let n = state.sessions;
        state.save(root)?;
        let before = ledger.lock().unwrap().passing_ids();

        let (result, usage) = work_session(env, n, &ledger, report).await;
        state.usage.add(usage);

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
        let summary = match &result {
            Ok(()) if newly.is_empty() => format!("{passing}/{total} passing, nothing new"),
            Ok(()) => format!("{passing}/{total} passing, new: {newly}"),
            Err(err) => format!("failed: {}", preview(&format!("{err:#}"), ERROR_CHARS)),
        };
        append_progress(root, &format!("session {n}: {summary}, {}", tokens(usage)))?;
        if git::is_dirty(root)? {
            git::commit_all(root, &checkpoint_message(n))?;
            line(
                report,
                &format!("checkpoint commit: {}", checkpoint_message(n)),
            );
        }
        state.stalled = if newly.is_empty() {
            state.stalled + 1
        } else {
            0
        };
        line(report, &format!("== session {n} done: {summary} =="));

        if let Err(err) = result {
            break Stop {
                reason: StopReason::Error,
                session: n,
                detail: Some(format!("{err:#}")),
            };
        }
        if passing == total {
            break stop(StopReason::Complete, n);
        }
        if state.stalled >= STALL_LIMIT {
            break stop(StopReason::Stalled, n);
        }
        state.save(root)?;
    };
    state.stop = Some(stop.clone());
    state.save(root)?;
    Ok(stop)
}

/// One work session: a fresh agent told where the task stands, with the
/// default tools and `mark_feature_passing`. Returns how it ended and the
/// tokens it used.
async fn work_session(
    env: &TaskEnv<'_>,
    n: u64,
    ledger: &Arc<Mutex<Ledger>>,
    report: &mut Reporter<'_>,
) -> (Result<()>, Usage) {
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
    line(
        report,
        &format!(
            "== session {n} start: {passing}/{total} passing{} ==",
            journal_note(&journal)
        ),
    );
    let result = agent
        .run(prompt::WORK_INPUT, |event| report(Report::Event(event)))
        .await
        .map(|_| ());
    (result, agent.usage())
}

/// Checks the list a new task starts from and returns its length.
fn check_new_list(root: &Path) -> Result<usize> {
    let list = features::load(root)?;
    features::check_fresh(&list)?;
    Ok(list.len())
}

/// Creates `.junees/` and keeps the harness state out of git.
fn prepare_dir(root: &Path) -> Result<()> {
    let dir = dir(root);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(&ignore, "state.json\n.*.tmp\n")
            .with_context(|| format!("cannot write {}", ignore.display()))?;
    }
    Ok(())
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

fn line(report: &mut Reporter<'_>, text: &str) {
    report(Report::Line(text));
}

fn warn(report: &mut Reporter<'_>, text: &str) {
    report(Report::Event(Event::Warning(text)));
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

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
    /// records each request's system prompt and messages.
    #[derive(Clone, Default)]
    struct Scripted {
        responses: Arc<Mutex<VecDeque<Reply>>>,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl Scripted {
        fn new(responses: Vec<Reply>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into())),
                seen: Arc::default(),
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
            provider: factory,
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
                write(&features_path(root), &feature_list(&["true", "true"])),
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
        assert_eq!(State::load(root).unwrap().usage.input_tokens, 15);
        assert!(super::super::state_path(root).exists());
        assert!(
            out.lines[0].starts_with("== init session, journal "),
            "{:?}",
            out.lines
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
                &feature_list(&["true", "true"]),
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
