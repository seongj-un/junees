use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::agent::{Agent, AgentOptions, Cancelled, Event, LimitReached};
use crate::approval::{Answer, Approval, ApprovalMode, Ask, ExternalTool, Request};
use crate::cancel::Cancel;
use crate::message::{ContentBlock, Message, Role, StopReason, Usage};
use crate::provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use crate::session::{self, Session, SessionWriter};
use crate::tools::ToolRegistry;

type Reply = std::result::Result<CompletionResponse, ProviderError>;

/// What one model call was sent.
#[derive(Debug, Clone)]
struct Seen {
    key: String,
    system: String,
    tools: Vec<String>,
    messages: Vec<Message>,
}

/// Answers each conversation from its own list of replies, picked by the
/// first line of its first message: the parent's first input or a
/// subagent's goal. Keeps track of how many calls are in flight at once.
#[derive(Default)]
struct Router {
    routes: Mutex<HashMap<String, VecDeque<Reply>>>,
    delays: Mutex<HashMap<String, Duration>>,
    seen: Mutex<Vec<Seen>>,
    in_flight: AtomicUsize,
    most: AtomicUsize,
}

impl Router {
    fn new(routes: Vec<(&str, Vec<Reply>)>) -> Arc<Self> {
        let router = Self::default();
        for (key, replies) in routes {
            router
                .routes
                .lock()
                .unwrap()
                .insert(key.to_string(), replies.into());
        }
        Arc::new(router)
    }

    fn delay(self: &Arc<Self>, key: &str, millis: u64) -> Arc<Self> {
        self.delays
            .lock()
            .unwrap()
            .insert(key.to_string(), Duration::from_millis(millis));
        Arc::clone(self)
    }

    fn seen(&self, key: &str) -> Vec<Seen> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .filter(|seen| seen.key == key)
            .cloned()
            .collect()
    }

    fn left(&self, key: &str) -> usize {
        self.routes
            .lock()
            .unwrap()
            .get(key)
            .map_or(0, VecDeque::len)
    }
}

struct Shared(Arc<Router>);

#[async_trait]
impl Provider for Shared {
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
    ) -> std::result::Result<CompletionResponse, ProviderError> {
        let router = &self.0;
        let first = request
            .messages
            .first()
            .map(Message::text)
            .unwrap_or_default();
        let key = first.lines().next().unwrap_or_default().to_string();
        router.seen.lock().unwrap().push(Seen {
            key: key.clone(),
            system: request.system.to_string(),
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            messages: request.messages.to_vec(),
        });
        let now = router.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        router.most.fetch_max(now, Ordering::SeqCst);
        let delay = router.delays.lock().unwrap().get(&key).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        router.in_flight.fetch_sub(1, Ordering::SeqCst);
        router
            .routes
            .lock()
            .unwrap()
            .get_mut(&key)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| panic!("no reply left for `{key}`"))
    }
}

fn text(text: &str, tokens: u64) -> Reply {
    Ok(CompletionResponse {
        content: vec![ContentBlock::text(text)],
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
    })
}

fn call(id: &str, name: &str, input: Value, tokens: u64) -> Reply {
    Ok(CompletionResponse {
        content: vec![
            ContentBlock::text("let me check"),
            ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            },
        ],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
    })
}

/// A `delegate` call `d1` with these goals, using 7 tokens.
fn delegate(goals: &[&str]) -> Reply {
    let tasks: Vec<Value> = goals.iter().map(|goal| json!({ "goal": goal })).collect();
    call("d1", NAME, json!({ "tasks": tasks }), 7)
}

/// A tool named `name` that echoes its input, counting its runs; `wait`
/// never finishes.
struct Fake(&'static str, Arc<AtomicUsize>);

#[async_trait]
impl Tool for Fake {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "fake"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn run(&self, input: Value) -> ToolOutput {
        self.1.fetch_add(1, Ordering::SeqCst);
        if self.0 == "wait" {
            std::future::pending::<()>().await;
        }
        ToolOutput::ok(format!("{} ran with {input}", self.0))
    }
}

const SYSTEM: &str = "You are a test.";

fn settings(parallel: usize) -> Settings {
    Settings {
        parallel,
        ..Settings::default()
    }
}

/// A parent agent with fake tools (a `memory` and a `skill_manage` among
/// them) and `delegate`; also returns how often the tools ran.
fn parent(router: &Arc<Router>, settings: Settings) -> (Agent, Arc<AtomicUsize>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    for name in ["echo", "wait", "memory", "skill_manage"] {
        tools.register(Fake(name, Arc::clone(&runs)));
    }
    let agent = Agent::new(
        Box::new(Shared(Arc::clone(router))),
        tools,
        AgentOptions {
            system: format!("{SYSTEM}{PROMPT_NOTE}"),
            max_tokens: 100,
            max_turns: 5,
            context_window: 100_000,
            compact_threshold: 0.8,
        },
    )
    .with_delegate(settings);
    (agent, runs)
}

/// Collects the `[n] line` progress lines of a run.
fn lines(log: &mut Vec<String>) -> impl FnMut(Event<'_>) + '_ {
    |event| {
        if let Event::Subagent { index, line } = event {
            log.push(format!("[{index}] {line}"));
        }
    }
}

/// The result of the parent's `delegate` call `id`.
fn result_of(agent: &Agent, id: &str) -> (String, bool) {
    agent
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == id => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("no such tool result")
}

#[tokio::test]
async fn runs_tasks_in_parallel_up_to_the_limit_and_keeps_their_order() {
    for parallel in [1, 2] {
        let router = Router::new(vec![
            (
                "go",
                vec![delegate(&["alpha", "beta", "gamma"]), text("all done", 3)],
            ),
            ("alpha", vec![text("alpha answer", 10)]),
            ("beta", vec![text("beta answer", 20)]),
            ("gamma", vec![text("gamma answer", 30)]),
        ])
        .delay("alpha", 150)
        .delay("beta", 20)
        .delay("gamma", 20);
        let (mut agent, _) = parent(&router, settings(parallel));
        let mut log = Vec::new();
        let answer = agent.run("go", lines(&mut log)).await.unwrap();
        assert_eq!(answer, "all done");
        assert_eq!(router.most.load(Ordering::SeqCst), parallel);

        // In task order, whichever finished first.
        assert_eq!(
            result_of(&agent, "d1"),
            (
                "[1] done: alpha\nalpha answer\n\n[2] done: beta\nbeta answer\n\n\
                 [3] done: gamma\ngamma answer"
                    .to_string(),
                false
            )
        );
        assert!(log.contains(&"[1] started: alpha".into()), "{log:?}");
        assert!(log.contains(&"[3] done (30 tokens)".into()), "{log:?}");
        if parallel == 2 {
            let at = |line: &str| log.iter().position(|l| l == line).unwrap();
            assert!(at("[2] done (20 tokens)") < at("[1] done (10 tokens)"));
        }
        // The subagents' tokens count toward the parent.
        assert_eq!(agent.usage().input_tokens, 7 + 3 + 10 + 20 + 30);
    }
}

#[tokio::test]
async fn subagents_start_fresh_with_fewer_tools() {
    let router = Router::new(vec![
        (
            "earlier",
            vec![text("hi", 1), delegate(&["alpha"]), text("ok", 1)],
        ),
        ("alpha", vec![text("fine", 1)]),
    ]);
    let (mut agent, _) = parent(&router, settings(3));
    agent.run("earlier", |_| {}).await.unwrap();
    agent.run("now delegate", |_| {}).await.unwrap();
    let parent_call = &router.seen("earlier")[1];
    assert_eq!(
        parent_call.tools,
        ["echo", "wait", "memory", "skill_manage", NAME]
    );
    assert!(parent_call.system.contains(PROMPT_NOTE));

    let child = &router.seen("alpha")[0];
    assert_eq!(child.tools, ["echo", "wait"]);
    assert_eq!(child.system, format!("{SYSTEM}{CHILD_NOTE}"));
    assert_eq!(child.messages, [Message::user_text("alpha")]);
}

#[tokio::test]
async fn the_context_follows_the_goal() {
    let tasks = json!({"tasks": [{"goal": "alpha", "context": "  the repo is in /srv/x  "}]});
    let router = Router::new(vec![
        ("go", vec![call("d1", NAME, tasks, 1), text("ok", 1)]),
        ("alpha", vec![text("fine", 1)]),
    ]);
    let (mut agent, _) = parent(&router, settings(3));
    agent.run("go", |_| {}).await.unwrap();
    assert_eq!(
        router.seen("alpha")[0].messages,
        [Message::user_text(
            "alpha\n\nContext from the agent that delegated this task:\nthe repo is in /srv/x"
        )]
    );
}

#[tokio::test]
async fn a_subagent_cannot_delegate_further() {
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha"]), text("ok", 1)]),
        (
            "alpha",
            vec![
                call("c1", NAME, json!({"tasks": [{"goal": "deeper"}]}), 1),
                text("could not", 1),
            ],
        ),
    ]);
    let (mut agent, _) = parent(&router, settings(3));
    agent.run("go", |_| {}).await.unwrap();
    let second = &router.seen("alpha")[1];
    assert_eq!(
        second.messages.last().unwrap().content,
        [ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "unknown tool `delegate`".into(),
            is_error: true,
        }]
    );
    assert!(router.seen("deeper").is_empty());
}

#[tokio::test]
async fn subagent_tokens_count_toward_the_parent_limit() {
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha", "beta"])]),
        ("alpha", vec![text("alpha answer", 995)]),
        ("beta", vec![text("never asked", 1)]),
    ]);
    let (agent, _) = parent(&router, settings(1));
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let mut agent = agent.with_limit(Box::new(move |usage: Usage| {
        let tokens = crate::task::budget::tokens(usage);
        seen.lock().unwrap().push(tokens);
        (tokens >= 1000).then(|| format!("token budget used up: {tokens} of 1000 tokens"))
    }));
    let mut log = Vec::new();
    let err = agent.run("go", lines(&mut log)).await.unwrap_err();
    // The parent stops before its next call: the subagents' tokens are its
    // own.
    assert_eq!(
        err.downcast_ref::<LimitReached>().unwrap().0,
        "token budget used up: 1002 of 1000 tokens"
    );
    assert_eq!(*asked.lock().unwrap(), [0, 7, 1002, 1002]);
    assert_eq!(router.left("beta"), 1);
    assert_eq!(
        result_of(&agent, "d1"),
        (
            "[1] done: alpha\nalpha answer\n\n[2] stopped: beta\n\
             token budget used up: 1002 of 1000 tokens"
                .to_string(),
            false
        )
    );
    assert!(log.contains(&"[2] stopped: token budget used up: 1002 of 1000 tokens".into()));
    assert_eq!(agent.usage().input_tokens, 1002);
    assert_eq!(agent.messages().last().unwrap().role, Role::User);
}

#[tokio::test]
async fn a_subagent_stops_at_its_own_token_and_turn_limits() {
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha", "beta"]), text("ok", 1)]),
        (
            "alpha",
            vec![call("a1", "echo", json!({"n": 1}), 150), text("unused", 1)],
        ),
        (
            "beta",
            vec![
                call("b1", "echo", json!({"n": 1}), 1),
                call("b2", "echo", json!({"n": 2}), 1),
            ],
        ),
    ]);
    let limits = Settings {
        parallel: 2,
        max_turns: 2,
        max_tokens: 100,
    };
    let (mut agent, runs) = parent(&router, limits);
    agent.run("go", |_| {}).await.unwrap();
    assert_eq!(
        result_of(&agent, "d1"),
        (
            "[1] stopped: alpha\nsubagent token limit used up: 150 of 100 tokens\n\
             its last message:\nlet me check\n\n\
             [2] stopped: beta\nstopped after 2 model calls without a final answer\n\
             its last message:\nlet me check"
                .to_string(),
            // No task gave an answer.
            true
        )
    );
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    assert_eq!(router.left("alpha"), 1);
}

#[tokio::test]
async fn cancel_stops_every_subagent() {
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha", "beta", "gamma"])]),
        ("alpha", vec![call("a1", "wait", json!({}), 1)]),
        ("beta", vec![call("b1", "wait", json!({}), 1)]),
    ]);
    let home = tempfile::tempdir().unwrap();
    let (meta, writer) = SessionWriter::create(home.path(), home.path(), "m").unwrap();
    let (agent, runs) = parent(&router, settings(2));
    let cancel = Cancel::new();
    let mut agent = agent
        .with_cancel(cancel.clone())
        .with_journal(Box::new(writer));
    let started = Arc::clone(&runs);
    tokio::spawn(async move {
        while started.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
    });
    let mut log = Vec::new();
    let run = agent.run("go", lines(&mut log));
    let err = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("the cancel did not stop the subagents")
        .unwrap_err();
    assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
    assert_eq!(
        result_of(&agent, "d1"),
        (
            "[1] cancelled: alpha\ncancelled by the user\n\n\
             [2] cancelled: beta\ncancelled by the user\n\n\
             [3] cancelled: gamma\ncancelled by the user"
                .to_string(),
            true
        )
    );
    assert!(
        log.contains(&"[3] cancelled before it started".into()),
        "{log:?}"
    );
    assert_eq!(agent.messages().last().unwrap().role, Role::User);

    // Each subagent that started recorded its cancelled call; the third
    // never started and has no journal.
    let dir = session::sessions_dir(home.path()).join(&meta.id);
    for (n, id) in [(1, "a1"), (2, "b1")] {
        let sub = Session::load(&dir.join(format!("sub-{n}.jsonl"))).unwrap();
        assert_eq!(
            sub.transcript.messages.last().unwrap().content,
            [ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: crate::agent::CANCELLED.into(),
                is_error: true,
            }]
        );
    }
    assert!(!dir.join("sub-3.jsonl").exists());
}

#[tokio::test]
async fn subagents_keep_their_own_journals() {
    let gamma = json!({"tasks": [{"goal": "gamma"}]});
    let router = Router::new(vec![
        (
            "go",
            vec![
                delegate(&["alpha", "beta"]),
                text("first", 1),
                call("d2", NAME, gamma, 7),
                text("second", 1),
            ],
        ),
        (
            "alpha",
            vec![
                call("child-call-1", "echo", json!({}), 10),
                text("alpha answer", 10),
            ],
        ),
        ("beta", vec![text("beta answer", 20)]),
        ("gamma", vec![text("gamma answer", 30)]),
    ]);
    let home = tempfile::tempdir().unwrap();
    let (meta, writer) = SessionWriter::create(home.path(), home.path(), "the-model").unwrap();
    let (agent, _) = parent(&router, settings(2));
    let mut agent = agent.with_journal(Box::new(writer));
    agent.run("go", |_| {}).await.unwrap();
    agent.run("again", |_| {}).await.unwrap();

    let sessions = session::sessions_dir(home.path());
    let dir = sessions.join(&meta.id);
    let mode = |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode(&dir) & 0o777, 0o700);
    for (n, goal, messages) in [(1, "alpha", 4), (2, "beta", 2), (3, "gamma", 2)] {
        let path = dir.join(format!("sub-{n}.jsonl"));
        assert_eq!(mode(&path) & 0o777, 0o600);
        let sub = Session::load(&path).unwrap();
        assert_eq!(sub.meta.id, format!("{}/sub-{n}", meta.id));
        assert_eq!(sub.meta.model, "the-model");
        assert_eq!(sub.meta.cwd, meta.cwd);
        assert_eq!(sub.transcript.messages[0], Message::user_text(goal));
        assert_eq!(sub.transcript.messages.len(), messages);
    }

    // The parent's journal has the delegate calls and their results, and
    // counts the subagents' tokens, but not their histories.
    let path = sessions.join(format!("{}.jsonl", meta.id));
    let main = Session::load(&path).unwrap();
    assert_eq!(main.transcript.messages, agent.messages());
    assert_eq!(main.transcript.messages.len(), 8);
    assert_eq!(main.transcript.usage, agent.usage());
    assert_eq!(
        main.transcript.usage.input_tokens,
        7 + 1 + 7 + 1 + 10 + 10 + 20 + 30
    );
    let journal = std::fs::read_to_string(&path).unwrap();
    assert!(!journal.contains("child-call-1"));
    assert_eq!(journal.matches("\"type\":\"subagent_usage\"").count(), 2);
    // `/sessions` lists the parent only.
    let listed = session::list_recent(home.path(), 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, meta.id);
}

#[tokio::test]
async fn an_unsaved_session_saves_no_subagents() {
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha"]), text("ok", 1)]),
        ("alpha", vec![text("fine", 1)]),
    ]);
    let (mut agent, _) = parent(&router, settings(1));
    let mut log = Vec::new();
    agent.run("go", lines(&mut log)).await.unwrap();
    assert_eq!(log, ["[1] started: alpha", "[1] done (1 tokens)"]);
}

/// A gate that records who asked about what, and turns subagent 2 down.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl ToolGate for Recorder {
    async fn check(&self, tool: &str, _input: &Value) -> Option<ToolOutput> {
        self.0.lock().unwrap().push(format!("parent: {tool}"));
        None
    }

    async fn check_for(&self, agent: &str, tool: &str, _input: &Value) -> Option<ToolOutput> {
        self.0.lock().unwrap().push(format!("{agent}: {tool}"));
        agent
            .starts_with("[2]")
            .then(|| crate::approval::declined("not this one"))
    }
}

#[tokio::test]
async fn subagents_ask_through_the_parent_gate() {
    let router = Router::new(vec![
        (
            "go",
            vec![
                call("e0", "echo", json!({}), 1),
                delegate(&["alpha", "beta\nmore"]),
                text("ok", 1),
            ],
        ),
        (
            "alpha",
            vec![call("a1", "echo", json!({}), 1), text("alpha answer", 1)],
        ),
        (
            "beta",
            vec![call("b1", "echo", json!({}), 1), text("beta answer", 1)],
        ),
    ]);
    let recorder = Recorder::default();
    let (agent, runs) = parent(&router, settings(1));
    let mut agent = agent.with_gate(Box::new(recorder.clone()));
    let mut log = Vec::new();
    agent.run("go", lines(&mut log)).await.unwrap();
    assert_eq!(
        *recorder.0.lock().unwrap(),
        ["parent: echo", "[1] alpha: echo", "[2] beta more: echo"]
    );
    // The parent's and subagent 1's calls ran; subagent 2's was declined.
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert_eq!(
        router.seen("beta")[1].messages.last().unwrap().content,
        [ContentBlock::ToolResult {
            tool_use_id: "b1".into(),
            content: "the user declined: not this one".into(),
            is_error: true,
        }]
    );
    assert!(
        log.contains(&"[2] <- echo error: the user declined: not this one".into()),
        "{log:?}"
    );
}

/// Answers a question only once subagent `beta` made its second model
/// call (its tool call ran), or after a few seconds; declines either way.
struct AnswerAfterBeta {
    router: Arc<Router>,
    timed_out: Arc<Mutex<Option<bool>>>,
}

#[async_trait]
impl Ask for AnswerAfterBeta {
    async fn ask(&mut self, _request: &Request) -> Answer {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut timed_out = false;
        while self.router.seen("beta").len() < 2 {
            if tokio::time::Instant::now() > deadline {
                timed_out = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        *self.timed_out.lock().unwrap() = Some(timed_out);
        Answer::No
    }
}

#[tokio::test]
async fn a_question_does_not_hold_up_calls_that_need_none() {
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha", "beta"]), text("ok", 1)]),
        (
            "alpha",
            vec![call("a1", "ext", json!({}), 1), text("alpha answer", 1)],
        ),
        (
            "beta",
            vec![call("b1", "echo", json!({}), 1), text("beta answer", 1)],
        ),
    ]);
    let timed_out = Arc::new(Mutex::new(None));
    let root = std::path::Path::new("/nonexistent-junees-workspace");
    let gate = Approval::new(
        ApprovalMode::Risky,
        &crate::tools::ToolSettings::default(),
        crate::tools::Workspace::new(root),
    )
    .with_external_tools([(
        "ext".to_string(),
        ExternalTool {
            server: "fake".into(),
            tool: "ext".into(),
            trusted: false,
        },
    )])
    .asking(Box::new(AnswerAfterBeta {
        router: Arc::clone(&router),
        timed_out: Arc::clone(&timed_out),
    }));
    let (agent, runs) = parent(&router, settings(2));
    let mut agent = agent.with_gate(Box::new(gate));
    let mut log = Vec::new();
    agent.run("go", lines(&mut log)).await.unwrap();
    // Subagent 1's question waited for its answer while subagent 2's echo,
    // which needs no question, ran and subagent 2 finished.
    assert_eq!(*timed_out.lock().unwrap(), Some(false), "{log:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    let beta_done = log.iter().position(|line| line.starts_with("[2] done"));
    let alpha_declined = log
        .iter()
        .position(|line| line.starts_with("[1] <- ext error: the user declined"));
    assert!(
        beta_done.is_some() && alpha_declined.is_some() && beta_done < alpha_declined,
        "{log:?}"
    );
}

#[tokio::test]
async fn results_share_a_size_limit() {
    let long = "가".repeat(RESULT_CHARS);
    let router = Router::new(vec![
        ("go", vec![delegate(&["alpha", "beta"]), text("ok", 1)]),
        ("alpha", vec![text(&long, 1)]),
        ("beta", vec![text("short", 1)]),
    ]);
    let (mut agent, _) = parent(&router, settings(2));
    agent.run("go", |_| {}).await.unwrap();
    let (result, _) = result_of(&agent, "d1");
    let chars = result.chars().count();
    assert!(chars <= RESULT_CHARS / 2 + 100, "{chars}");
    assert!(result.contains("characters omitted"));
    assert!(result.ends_with("[2] done: beta\nshort"));
}

#[tokio::test]
async fn bad_input_runs_nothing() {
    let eleven: Vec<Value> = (0..=MAX_TASKS)
        .map(|n| json!({ "goal": n.to_string() }))
        .collect();
    for (input, error) in [
        (json!({}), "missing required array argument `tasks`"),
        (
            json!({"tasks": "a"}),
            "missing required array argument `tasks`",
        ),
        (
            json!({"tasks": []}),
            "`tasks` must hold 1 to 10 tasks, got 0",
        ),
        (
            json!({ "tasks": eleven }),
            "`tasks` must hold 1 to 10 tasks, got 11",
        ),
        (
            json!({"tasks": [{"goal": "a"}, {"goal": "  "}]}),
            "task 2 needs a non-empty string `goal`",
        ),
        (
            json!({"tasks": [{"goal": "a", "context": 3}]}),
            "task 1: `context` must be a string",
        ),
    ] {
        let router = Router::new(vec![(
            "go",
            vec![call("d1", NAME, input, 1), text("ok", 1)],
        )]);
        let (mut agent, runs) = parent(&router, settings(3));
        agent.run("go", |_| {}).await.unwrap();
        assert_eq!(result_of(&agent, "d1"), (error.to_string(), true));
        assert_eq!(router.seen.lock().unwrap().len(), 2);
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn the_tool_alone_runs_no_subagents() {
    let output = Delegate::new(Settings::default())
        .run(json!({"tasks": [{"goal": "a"}]}))
        .await;
    assert!(output.is_error);
}

#[test]
fn reads_the_settings() {
    let read = |pairs: &[(&str, &str)]| {
        let pairs: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Settings::read(|key| pairs.get(key).cloned())
    };
    assert_eq!(read(&[]).unwrap(), Some(Settings::default()));
    assert_eq!(read(&[("JUNEES_SUBAGENTS", "0")]).unwrap(), None);
    assert_eq!(
        read(&[
            ("JUNEES_SUBAGENTS", "5"),
            ("JUNEES_SUBAGENT_MAX_TURNS", "7"),
            ("JUNEES_SUBAGENT_MAX_TOKENS", "5000"),
        ])
        .unwrap(),
        Some(Settings {
            parallel: 5,
            max_turns: 7,
            max_tokens: 5000
        })
    );
    for (key, value, error) in [
        (
            "JUNEES_SUBAGENTS",
            "11",
            "JUNEES_SUBAGENTS must be a whole number from 0 (no subagents) to 10, got `11`",
        ),
        (
            "JUNEES_SUBAGENT_MAX_TURNS",
            "0",
            "JUNEES_SUBAGENT_MAX_TURNS must be a positive integer, got `0`",
        ),
        (
            "JUNEES_SUBAGENT_MAX_TOKENS",
            "many",
            "JUNEES_SUBAGENT_MAX_TOKENS must be a positive integer, got `many`",
        ),
    ] {
        assert_eq!(read(&[(key, value)]).unwrap_err().to_string(), error);
    }
    assert_eq!(prompt_note(None), "");
    assert_eq!(prompt_note(Some(Settings::default())), PROMPT_NOTE);
}
