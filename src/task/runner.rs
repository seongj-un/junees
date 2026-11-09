//! Runs task sessions: the init session that plans a task, and the work
//! sessions that build it one feature at a time.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::features::{self, Ledger};
use super::state::State;
use super::{dir, features_path, git, progress_path, prompt};
use crate::agent::{Agent, AgentOptions, Event};
use crate::message::Usage;
use crate::provider::Provider;
use crate::session::SessionWriter;
use crate::tools::ToolRegistry;

/// Commit message of the commit that starts a task.
pub const INIT_COMMIT: &str = "junees: initialize task";

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

    let (mut agent, journal) = env.agent(
        prompt::init_system(root),
        ToolRegistry::with_defaults(),
        report,
    )?;
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
}
