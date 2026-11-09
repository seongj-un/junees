//! `task init` and `task run` with the `claude-code` backend, against fake
//! `claude` programs: the harness verifies, commits, counts the budget and
//! stops the same way as with its own agent.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::budget::Budget;
use super::claude_code::testing::fake_claude;
use super::claude_code::{ClaudeCode, LOGIN_HINT};
use super::git::testing::{repo, subjects};
use super::runner::{INIT_COMMIT, ProviderFactory, Report, TaskEnv, checkpoint_message, init, run};
use super::state::{State, StopReason};
use super::{features, git, progress_path, sessions_dir};
use crate::agent::Event;
use crate::cancel::Cancel;
use crate::notify::Notifier;
use crate::provider::Provider;
use crate::tools::{SettingsFile, ToolSettings};

fn no_provider() -> anyhow::Result<Box<dyn Provider>> {
    panic!("the API provider was used with the claude-code backend")
}

fn env<'a>(root: &Path, factory: &'a ProviderFactory<'a>, bin: &Path) -> TaskEnv<'a> {
    TaskEnv {
        skill_manage: false,
        root: root.to_path_buf(),
        home: None,
        model: "unused".into(),
        max_tokens: 100,
        max_turns: 9,
        tool_settings: ToolSettings {
            settings_file: Some(SettingsFile::new(
                "/home/tester/.junees/config.toml",
                Some(PathBuf::from("/home/tester")),
            )),
            ..ToolSettings::default()
        },
        context_window: 200_000,
        compact_threshold: 0.8,
        provider: factory,
        notify: Notifier::default(),
        budget: Budget::default(),
        cancel: Cancel::new(),
        claude: Some(ClaudeCode {
            bin: bin.to_path_buf(),
            model: None,
            grace: Duration::from_millis(300),
            scrub: true,
        }),
    }
}

/// A fake `claude` that plays `sessions` in order, one per run, and
/// records each run's arguments as `args-<n>` in its own folder.
struct Fake {
    dir: tempfile::TempDir,
    bin: PathBuf,
}

impl Fake {
    /// The fake in `dir`, whose sessions may name `dir`.
    fn new(dir: tempfile::TempDir, sessions: &[String]) -> Self {
        let mut body = format!(
            "[ \"$1\" = --version ] && echo '2.1.319 (Claude Code)' && exit 0\n\
             F='{}'\n\
             n=$(( $(cat \"$F/count\" 2>/dev/null || echo 0) + 1 ))\n\
             echo $n > \"$F/count\"\n\
             printf '%s\\0' \"$@\" > \"$F/args-$n\"\n\
             case $n in\n",
            dir.path().display()
        );
        for (i, session) in sessions.iter().enumerate() {
            body.push_str(&format!("{})\n{session}\n;;\n", i + 1));
        }
        body.push_str("*) echo \"unexpected session $n\" >&2; exit 9;;\nesac\n");
        let bin = fake_claude(dir.path(), &body);
        Self { dir, bin }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The arguments of run `n`.
    fn args(&self, n: usize) -> Vec<String> {
        fs::read(self.path(&format!("args-{n}")))
            .unwrap()
            .split(|&b| b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect()
    }

    fn runs(&self) -> usize {
        fs::read_to_string(self.path("count")).map_or(0, |count| count.trim().parse().unwrap())
    }
}

/// The value after `flag` in `args`.
fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let at = args.iter().position(|arg| arg == flag).unwrap();
    &args[at + 1]
}

fn print(events: &[Value]) -> String {
    let lines: Vec<String> = events.iter().map(Value::to_string).collect();
    format!("cat <<'JSON'\n{}\nJSON", lines.join("\n"))
}

fn assistant(id: &str, text: &str, input_tokens: u64) -> Value {
    json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s",
        "message": {"id": id, "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": input_tokens, "output_tokens": 1}}})
}

/// Output of a session that answers `text` after `input_tokens` tokens.
fn answered(text: &str, input_tokens: u64) -> String {
    print(&[
        json!({"type": "system", "subtype": "init", "session_id": "s"}),
        assistant("msg_1", text, input_tokens),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": text,
            "num_turns": 1, "total_cost_usd": 0.01, "session_id": "s",
            "usage": {"input_tokens": input_tokens, "output_tokens": 1}}),
    ])
}

/// An init session writing a two-feature list (verifies count their runs
/// in the fake's `verifies` file).
fn plan(fake_dir: &Path) -> String {
    let count = fake_dir.join("verifies").display().to_string();
    let list = json!([
        {"id": "F1", "description": "a", "verify": format!("echo F1 >> '{count}'; test -f a.txt"), "passes": false},
        {"id": "F2", "description": "b", "verify": format!("echo F2 >> '{count}'; test -f b.txt"), "passes": false},
    ]);
    format!(
        "cat > .junees/features.json <<'LIST'\n{list}\nLIST\n\
         echo '# Plan' > .junees/progress.md\n{}",
        answered("planned", 10)
    )
}

/// A fake whose first run plans the task; `sessions` (given the fake's
/// folder) follow.
fn planned(sessions: impl FnOnce(&Path) -> Vec<String>) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let mut all = vec![plan(dir.path())];
    all.extend(sessions(dir.path()));
    Fake::new(dir, &all)
}

#[derive(Default)]
struct Collected {
    lines: Vec<String>,
    warnings: Vec<String>,
    texts: Vec<String>,
}

impl Collected {
    fn reporter(&mut self) -> impl FnMut(Report<'_>) + '_ {
        |report| match report {
            Report::Line(text) => self.lines.push(text.to_string()),
            Report::Event(Event::Warning(text)) => self.warnings.push(text.to_string()),
            Report::Event(Event::TextDelta(text)) => self.texts.push(text.to_string()),
            Report::Event(_) => {}
        }
    }
}

fn verifies(fake: &Fake) -> usize {
    fs::read_to_string(fake.path("verifies"))
        .unwrap_or_default()
        .lines()
        .count()
}

async fn initialized(root: &Path, fake: &Fake) {
    let factory = no_provider;
    init(
        &env(root, &factory, &fake.bin),
        "build a and b",
        &mut |_| {},
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn claude_code_sessions_get_the_skill_list() {
    let fake = planned(|_| vec![answered("nothing done", 5)]);
    let repo = repo();
    let root = repo.path();
    let home = tempfile::tempdir().unwrap();
    crate::skills::Store::new(crate::skills::dir(home.path()))
        .create(
            "deploy",
            "---\nname: deploy\ndescription: Use when deploying\n---\n1. ship\n",
        )
        .unwrap();
    let factory = no_provider;
    let mut env = env(root, &factory, &fake.bin);
    env.home = Some(home.path().to_path_buf());
    init(&env, "build a and b", &mut |_| {}).await.unwrap();
    run(&env, 1, &mut |_| {}).await.unwrap();
    assert_eq!(fake.runs(), 2);
    for n in [1, 2] {
        let args = fake.args(n);
        let system = after(&args, "--append-system-prompt");
        assert!(
            system.ends_with(
                "\n# Skills\nThe user keeps these Junees skills (saved procedures for \
                 recurring tasks). Only their names and descriptions are given here; their \
                 files cannot be read in this session.\n\n- deploy: Use when deploying\n"
            ),
            "{system}"
        );
    }
}

#[tokio::test]
async fn claude_code_plans_and_builds_while_the_harness_verifies() {
    let fake = planned(|_| {
        vec![
            // Builds F1 and commits it, leaves a note uncommitted, and
            // claims both features in the list.
            format!(
                "echo a > a.txt && git add a.txt && git commit -qm 'add a'\n\
                 echo note > note.txt\n\
                 sed -i 's/\"passes\": *false/\"passes\": true/g' .junees/features.json\n{}",
                answered("did F1", 20)
            ),
            format!("echo b > b.txt\n{}", answered("did F2", 30)),
        ]
    });
    let repo = repo();
    let root = repo.path();
    let factory = no_provider;
    let mut out = Collected::default();
    init(
        &env(root, &factory, &fake.bin),
        "build a and b",
        &mut out.reporter(),
    )
    .await
    .unwrap();

    assert_eq!(subjects(root), [INIT_COMMIT]);
    assert!(!git::is_dirty(root).unwrap());
    assert_eq!(features::load(root).unwrap().len(), 2);
    assert_eq!(State::load(root).unwrap().usage.input_tokens, 10);
    assert_eq!(
        out.lines[..2],
        [
            "== sessions run by Claude Code 2.1.319 (Claude Code) ==",
            "== init session, Claude Code, output in .junees/sessions/init.jsonl ==",
        ]
    );
    assert_eq!(out.texts, ["planned"]);
    // The output is kept, and kept out of git.
    let saved = fs::read_to_string(sessions_dir(root).join("init.jsonl")).unwrap();
    assert!(saved.contains("\"result\":\"planned\""), "{saved}");
    let tracked = git::show(root, "HEAD", ".junees/.gitignore").unwrap();
    assert!(tracked.lines().any(|line| line == "sessions/"), "{tracked}");
    let args = fake.args(1);
    assert_eq!(after(&args, "-p"), "The goal of the task:\n\nbuild a and b");
    let system = after(&args, "--append-system-prompt");
    assert!(
        system.starts_with("You are running inside Junees"),
        "{system}"
    );
    assert!(system.contains("You are the initializer of a long-running task."));
    assert_eq!(after(&args, "--max-turns"), "9");
    assert_eq!(after(&args, "--permission-mode"), "acceptEdits");
    assert!(!args.iter().any(|arg| arg.contains("dangerously")));
    // Claude Code stays off the user's settings file and its folder.
    assert_eq!(
        args[args.len() - 2..],
        ["Bash(*$JUNEES_HOME*)", "Bash(*${JUNEES_HOME}*)"]
    );
    for rule in [
        "Read(//home/tester/.junees/config.toml)",
        "Read(//home/tester/.junees/**)",
        "Edit(//home/tester/.junees/**)",
        "Bash(*~/.junees*)",
    ] {
        assert!(args.iter().any(|arg| arg == rule), "{rule}: {args:?}");
    }

    let mut out = Collected::default();
    let stop = run(&env(root, &factory, &fake.bin), 5, &mut out.reporter())
        .await
        .unwrap();
    assert_eq!(stop.reason, StopReason::Complete);
    assert_eq!(stop.session, 2);
    assert_eq!(
        subjects(root),
        [
            checkpoint_message(2),
            checkpoint_message(1),
            "add a".to_string(),
            INIT_COMMIT.to_string()
        ]
    );
    assert!(!git::is_dirty(root).unwrap());
    assert!(features::load(root).unwrap().iter().all(|f| f.passes));
    let state = State::load(root).unwrap();
    assert_eq!(state.usage.input_tokens, 60);
    assert_eq!(state.sessions, 2);
    // The claim in features.json was undone; only the verify counted.
    assert!(
        out.warnings.contains(
            &"the session edited .junees/features.json; the harness restored its own copy"
                .to_string()
        ),
        "{:?}",
        out.warnings
    );
    assert!(
        out.lines
            .contains(&"verified by the harness: F1".to_string()),
        "{:?}",
        out.lines
    );
    let progress = fs::read_to_string(progress_path(root)).unwrap();
    assert!(
        progress.contains("- [junees] session 1: 1/2 passing, new: F1, tokens in 20 out 1\n"),
        "{progress}"
    );
    assert!(progress.contains("- [junees] session 2: 2/2 passing, new: F2, tokens in 30 out 1\n"));
    // Each session was told where the task stands, and what the harness does.
    let first = fake.args(2);
    let system = after(&first, "--append-system-prompt");
    assert!(system.contains("in work session 1."), "{system}");
    assert!(system.contains("Features (0/2 pass):"));
    assert!(system.contains("the harness runs the verify command of every feature"));
    assert!(after(&fake.args(3), "--append-system-prompt").contains("- [x] F1: a"));
    assert!(sessions_dir(root).join("session-2.jsonl").exists());
    let log = fs::read_to_string(super::run_log_path(root)).unwrap();
    assert!(
        log.contains("session 1 claude-code: answered, 1 turns, 21 tokens, estimated cost $0.0100, claude session s"),
        "{log}"
    );
}

#[tokio::test]
async fn the_budget_stops_a_claude_code_session_and_its_work_is_kept() {
    let fake = planned(|_| {
        vec![format!(
            "echo a > a.txt\n{}\nsleep 30\n",
            print(&[assistant("msg_1", "working", 100)])
        )]
    });
    let repo = repo();
    let root = repo.path();
    initialized(root, &fake).await;
    let factory = no_provider;
    let mut env = env(root, &factory, &fake.bin);
    env.budget.max_tokens = Some(50);
    let started = Instant::now();
    let stop = run(&env, 5, &mut |_| {}).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(stop.reason, StopReason::Budget);
    assert_eq!(stop.session, 1);
    assert!(
        stop.detail
            .as_deref()
            .unwrap()
            .starts_with("token budget used up: 112 of 50 tokens"),
        "{stop:?}"
    );
    // The harness still verified what the session built.
    assert!(features::load(root).unwrap()[0].passes);
    assert_eq!(subjects(root)[0], checkpoint_message(1));
    assert_eq!(State::load(root).unwrap().usage.input_tokens, 110);
}

#[tokio::test]
async fn a_claude_code_without_login_stops_the_run_with_the_hint() {
    let fake = planned(|_| {
        vec![format!(
            "{}\nexit 1",
            print(&[
                json!({"type": "result", "subtype": "success", "is_error": true,
                "result": "Not logged in · Please run /login"})
            ])
        )]
    });
    let repo = repo();
    let root = repo.path();
    initialized(root, &fake).await;
    let checked = verifies(&fake);
    let factory = no_provider;
    let stop = run(&env(root, &factory, &fake.bin), 5, &mut |_| {})
        .await
        .unwrap();
    assert_eq!(stop.reason, StopReason::Error);
    assert_eq!(stop.session, 1);
    let detail = stop.detail.unwrap();
    assert!(detail.contains("Not logged in"), "{detail}");
    assert!(detail.ends_with(LOGIN_HINT), "{detail}");
    // Nothing to verify after a session that never started.
    assert_eq!(verifies(&fake), checked);
    assert_eq!(fake.runs(), 2);
}

#[tokio::test]
async fn the_usage_limit_stops_the_run_with_the_reset_time() {
    let fake = planned(|_| {
        vec![format!(
            "echo a > a.txt\n{}\nexit 1",
            print(&[
                json!({"type": "system", "subtype": "init", "session_id": "s"}),
                assistant("msg_1", "working", 10),
                json!({"type": "rate_limit_event", "session_id": "s",
                    "rate_limit_info": {"status": "rejected", "resetsAt": 1_794_205_024}}),
                json!({"type": "assistant", "error": "rate_limit", "parent_tool_use_id": null,
                    "session_id": "s", "message": {"content": [{"type": "text",
                    "text": "You've hit your session limit · resets 6:17am"}]}}),
                json!({"type": "result", "subtype": "success", "is_error": true,
                    "api_error_status": 429, "session_id": "s",
                    "result": "You've hit your session limit · resets 6:17am"}),
            ])
        )]
    });
    let repo = repo();
    let root = repo.path();
    initialized(root, &fake).await;
    let factory = no_provider;
    let mut seen = Collected::default();
    let stop = run(&env(root, &factory, &fake.bin), 5, &mut seen.reporter())
        .await
        .unwrap();
    assert_eq!(stop.reason, StopReason::Limit);
    assert!(!stop.reason.is_failure());
    assert_eq!(stop.session, 1);
    assert_eq!(stop.resets_at, Some(1_794_205_024));
    assert_eq!(
        stop.detail.as_deref(),
        Some("You've hit your session limit · resets 6:17am")
    );
    assert!(
        seen.warnings
            .iter()
            .any(|warning| warning.contains("requests are refused until 2026-11-09 06:17 UTC")),
        "{:?}",
        seen.warnings
    );
    // The session's work is verified and kept, and the stop is recorded
    // for `task status` and the next run.
    assert!(features::load(root).unwrap()[0].passes);
    assert_eq!(subjects(root)[0], checkpoint_message(1));
    let state = State::load(root).unwrap();
    assert_eq!(state.stop, Some(stop));
    let progress = fs::read_to_string(progress_path(root)).unwrap();
    assert!(
        progress
            .contains("session 1: 1/2 passing, new: F1, stopped at the Claude Code usage limit"),
        "{progress}"
    );
    let status = super::status::load(root).unwrap();
    assert!(
        status
            .to_text()
            .contains("last stop: session 1 hit the Claude Code usage limit (resets 2026-11-09 06:17 UTC; run `junees task` again after the reset)"),
        "{}",
        status.to_text()
    );
    assert_eq!(fake.runs(), 2);
}

#[tokio::test]
async fn cancel_stops_a_claude_code_session_and_the_run() {
    let fake = planned(|dir| {
        let dir = dir.display();
        vec![format!(
            "sleep 30 &\necho $! > '{dir}/pid'\n{}\ntouch '{dir}/printed'\nwait\n",
            print(&[assistant("msg_1", "working", 5)])
        )]
    });
    let repo = repo();
    let root = repo.path();
    initialized(root, &fake).await;
    let checked = verifies(&fake);
    let factory = no_provider;
    let env = env(root, &factory, &fake.bin);
    let stopper = async {
        for _ in 0..400 {
            if fake.path("printed").exists() {
                env.cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the session never printed");
    };
    let mut report = |_: Report<'_>| {};
    let (stop, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(run(&env, 5, &mut report), stopper)
    })
    .await
    .expect("the run did not stop");
    let stop = stop.unwrap();
    assert_eq!(stop.reason, StopReason::Interrupted);
    assert_eq!(stop.session, 1);
    assert_eq!(verifies(&fake), checked);
    assert_eq!(State::load(root).unwrap().usage.input_tokens, 15);
    let pid = fs::read_to_string(fake.path("pid")).unwrap();
    let stat = format!("/proc/{}/stat", pid.trim());
    for _ in 0..100 {
        let gone = fs::read_to_string(&stat).map_or(true, |stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        });
        if gone {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the session's command {pid} is still running");
}

#[tokio::test]
async fn an_unusable_claude_stops_before_any_session() {
    let fake = planned(|_| vec![answered("never", 1)]);
    let repo = repo();
    let root = repo.path();
    initialized(root, &fake).await;
    let old = fake_claude(
        tempfile::tempdir().unwrap().keep().as_path(),
        "echo '2.1.200 (Claude Code)'; exit 0",
    );
    let factory = no_provider;
    let err = run(&env(root, &factory, &old), 5, &mut |_| {})
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("is too old"), "{err:#}");
    assert_eq!(fake.runs(), 1);
    assert_eq!(State::load(root).unwrap().sessions, 0);

    let missing = fake.path("missing");
    let other = super::git::testing::repo();
    let err = init(&env(other.path(), &factory, &missing), "goal", &mut |_| {})
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("install Claude Code"),
        "{err:#}"
    );
}
