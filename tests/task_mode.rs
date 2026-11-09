//! Runs `junees task init` and `junees task run` as real processes against a
//! fake API server, in a temporary git repository.

use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, text_reply};

/// Git without the user's configuration, with a fixed identity, and blind to
/// a repository named in the environment (`git rebase -x` in a worktree
/// exports `GIT_DIR`).
fn isolate_git(command: &mut tokio::process::Command, home: &Path) {
    for name in junees::task::git::REPO_ENV_VARS {
        command.env_remove(name);
    }
    command
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Junees Test")
        .env("GIT_AUTHOR_EMAIL", "test@junees.invalid")
        .env("GIT_COMMITTER_NAME", "Junees Test")
        .env("GIT_COMMITTER_EMAIL", "test@junees.invalid");
}

async fn git(repo: &Path, home: &Path, args: &[&str]) -> String {
    let mut command = tokio::process::Command::new("git");
    command.args(args).current_dir(repo);
    isolate_git(&mut command, home);
    let output = command.output().await.unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap()
}

async fn junees(cwd: &Path, home: &Path, url: &str, args: &[&str]) -> Output {
    let output = junees_env(cwd, home, url, args, &[]).await;
    assert!(
        output.status.success(),
        "junees {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs junees with these extra environment variables and returns its
/// output, whether it succeeded or not.
async fn junees_env(
    cwd: &Path,
    home: &Path,
    url: &str,
    args: &[&str],
    vars: &[(&str, &str)],
) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home.join("junees"))
        .env_remove("JUNEES_MODEL")
        .env_remove("JUNEES_MAX_TOKENS")
        .env_remove("JUNEES_MAX_TURNS")
        .env_remove("JUNEES_NOTIFY_WEBHOOK")
        .env_remove("JUNEES_NOTIFY_EACH_SESSION")
        .envs(vars.iter().copied())
        .stdin(Stdio::null());
    isolate_git(&mut command, home);
    command.output().await.unwrap()
}

/// `junees task status` with the given arguments, without an API key: the
/// status needs none.
async fn status(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(["task", "status"])
        .args(args)
        .current_dir(cwd)
        .env_remove("ANTHROPIC_API_KEY")
        .env("JUNEES_HOME", home.join("junees"))
        .stdin(Stdio::null());
    isolate_git(&mut command, home);
    command.output().await.unwrap()
}

/// A reply calling these tools, in order.
fn tool_calls(calls: &[(&str, Value)]) -> Value {
    let content: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, input))| {
            json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": input})
        })
        .collect();
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": content,
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn write_file(path: &str, content: &str) -> (&'static str, Value) {
    ("write_file", json!({"path": path, "content": content}))
}

fn mark(id: &str) -> (&'static str, Value) {
    ("mark_feature_passing", json!({"id": id}))
}

/// Every tool result in a request, as (content, is_error).
fn tool_results(request: &str) -> Vec<(String, bool)> {
    let body = body_of(request);
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap().clone())
        .filter(|block| block["type"] == "tool_result")
        .map(|block| {
            (
                block["content"].as_str().unwrap().to_string(),
                block["is_error"] == true,
            )
        })
        .collect()
}

#[tokio::test]
async fn init_then_run_builds_the_features_and_commits() {
    let features = json!([
        {"id": "F1", "description": "hello.txt says hello", "verify": "grep -q hello hello.txt", "passes": false},
        {"id": "F2", "description": "world.txt exists", "verify": "test -f world.txt", "passes": false}
    ]);
    let (url, server) = fake_api(vec![
        // task init
        (
            200,
            tool_calls(&[
                write_file(".junees/features.json", &features.to_string()),
                write_file(".junees/progress.md", "# Plan\n\nF1, then F2.\n"),
            ]),
        ),
        (200, text_reply("Planned two features.")),
        // session 1: writes the file, then asks for F1.
        (200, tool_calls(&[write_file("hello.txt", "hello\n")])),
        (200, tool_calls(&[mark("F1")])),
        (200, text_reply("F1 done.")),
        // session 2: commits its own work, then asks for F2.
        (
            200,
            tool_calls(&[
                (
                    "bash",
                    json!({"command": "echo world > world.txt && git add world.txt && git commit -q -m 'add world'"}),
                ),
                mark("F2"),
            ]),
        ),
        (200, text_reply("F2 done.")),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;

    let init = junees(
        root,
        home.path(),
        &url,
        &["task", "init", "make hello and world"],
    )
    .await;
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(
        stdout.contains("init done: 2 features, committed"),
        "{stdout}"
    );

    // Started from a subfolder: the task still runs in the repository root.
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let run = junees(
        &sub,
        home.path(),
        &url,
        &["task", "run", "--max-sessions", "2"],
    )
    .await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stdout.contains("== session 1 start: 0/2 passing, journal "),
        "{stdout}"
    );
    assert!(
        stdout.contains("== session 1 done: 1/2 passing, new: F1 =="),
        "{stdout}"
    );
    assert!(
        stdout.contains("== session 2 done: 2/2 passing, new: F2 =="),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("task stopped: every feature passes\n"),
        "{stdout}"
    );
    assert!(
        stderr.contains("  -> mark_feature_passing {\"id\":\"F1\"}"),
        "{stderr}"
    );
    assert!(
        stderr.contains("  <- mark_feature_passing ok: F1 passes"),
        "{stderr}"
    );

    let log = git(root, home.path(), &["log", "--format=%s"]).await;
    assert_eq!(
        log,
        "junees: checkpoint after session 2\nadd world\n\
         junees: checkpoint after session 1\njunees: initialize task\n"
    );
    assert_eq!(git(root, home.path(), &["status", "--porcelain"]).await, "");
    let tracked = git(root, home.path(), &["ls-files"]).await;
    assert_eq!(
        tracked,
        ".junees/.gitignore\n.junees/features.json\n.junees/progress.md\nhello.txt\nworld.txt\n"
    );

    let list: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".junees/features.json")).unwrap())
            .unwrap();
    assert!(list.as_array().unwrap().iter().all(|f| f["passes"] == true));
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".junees/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["sessions"], 2);
    assert_eq!(state["stop"]["reason"], "complete");
    let progress = std::fs::read_to_string(root.join(".junees/progress.md")).unwrap();
    assert!(progress.starts_with("# Plan\n\nF1, then F2.\n- [junees] init: 2 features planned"));
    assert!(progress.contains("- [junees] session 2: 2/2 passing, new: F2, tokens in 13 out 6\n"));
    // One journal per session: init and two work sessions.
    let journals = std::fs::read_dir(home.path().join("junees/sessions"))
        .unwrap()
        .count();
    assert_eq!(journals, 3);

    // The status, from a subfolder, as JSON and as text.
    let shown = status(&sub, home.path(), &["--json"]).await;
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["goal"], "make hello and world");
    assert_eq!(
        (shown["passing"].clone(), shown["total"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(shown["features"][1]["id"], "F2");
    assert_eq!(shown["sessions"], 2);
    assert_eq!(shown["tokens"]["input"], 49);
    assert_eq!(shown["stop"]["reason"], "complete");
    assert!(shown["stop"]["at"].as_u64().is_some());
    assert!(
        shown["progress_tail"][4]
            .as_str()
            .unwrap()
            .starts_with("- [junees] session 2: 2/2 passing, new: F2")
    );
    let shown = status(root, home.path(), &[]).await;
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(text.contains("\ngoal: make hello and world\n"), "{text}");
    assert!(
        text.contains("\n  [x] F1  hello.txt says hello\n"),
        "{text}"
    );
    assert!(
        text.contains("\nlast stop: every feature passes (session 2, "),
        "{text}"
    );
    // Reading the status changed nothing.
    assert_eq!(git(root, home.path(), &["status", "--porcelain"]).await, "");

    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 7);
    let first_work = body_of(&requests[2]);
    // A string or content blocks, depending on prompt caching.
    let system = first_work["system"].to_string();
    assert!(system.contains("work session 1"), "{system}");
    assert!(system.contains("- [ ] F1: hello.txt says hello"));
    let tools: Vec<&str> = first_work["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tools,
        ["bash", "read_file", "write_file", "mark_feature_passing"]
    );
    let results = tool_results(&requests[4]);
    assert_eq!(results.len(), 2);
    assert!(
        results[1]
            .0
            .starts_with("F1 passes: its verify command succeeded. 1/2 features pass now."),
        "{results:?}"
    );
    // Session 2 starts from a fresh context that knows about session 1.
    let second = body_of(&requests[5]);
    assert_eq!(second["messages"].as_array().unwrap().len(), 1);
    assert!(second["system"].to_string().contains("- [x] F1"));
    assert!(
        second["system"]
            .to_string()
            .contains("junees: checkpoint after session 1")
    );
}

/// The replies of a `task init` that plans two features, F1 (a.txt) and
/// F2 (b.txt).
fn init_replies() -> Vec<(u16, Value)> {
    let features = json!([
        {"id": "F1", "description": "a.txt exists", "verify": "test -f a.txt", "passes": false},
        {"id": "F2", "description": "b.txt exists", "verify": "test -f b.txt", "passes": false}
    ]);
    vec![
        (
            200,
            tool_calls(&[
                write_file(".junees/features.json", &features.to_string()),
                write_file(".junees/progress.md", "# Plan\n"),
            ]),
        ),
        (200, text_reply("Planned.")),
    ]
}

/// The requests a fake webhook got; fails rather than waits if it did not
/// get as many as it was set up for.
async fn posts_of(webhook: tokio::task::JoinHandle<Vec<String>>) -> Vec<String> {
    tokio::time::timeout(std::time::Duration::from_secs(5), webhook)
        .await
        .expect("the webhook got fewer posts than expected")
        .unwrap()
}

/// The token part of the fake webhook URLs, which must never be printed.
const HOOK_TOKEN: &str = "hook-t0ken-do-not-print";

#[tokio::test]
async fn task_run_notifies_the_webhook_of_each_session_and_the_stop() {
    let mut replies = init_replies();
    replies.extend([
        // session 1 also checks that its bash cannot see the webhook.
        (
            200,
            tool_calls(&[
                ("bash", json!({"command": "printenv JUNEES_NOTIFY_WEBHOOK"})),
                write_file("a.txt", "a"),
                mark("F1"),
            ]),
        ),
        (200, text_reply("F1 done.")),
        (200, tool_calls(&[write_file("b.txt", "b"), mark("F2")])),
        (200, text_reply("F2 done.")),
    ]);
    let (url, api) = fake_api(replies).await;
    let (hook, webhook) =
        fake_api(vec![(200, json!({})), (200, json!({})), (200, json!({}))]).await;
    let hook = format!("{hook}/api/webhooks/7/{HOOK_TOKEN}");
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    let name = root.file_name().unwrap().to_string_lossy().to_string();
    git(root, home.path(), &["init", "-q"]).await;
    junees(root, home.path(), &url, &["task", "init", "make a and b"]).await;

    let run = junees_env(
        root,
        home.path(),
        &url,
        &["task", "run"],
        &[
            ("JUNEES_NOTIFY_WEBHOOK", &hook),
            ("JUNEES_NOTIFY_EACH_SESSION", "1"),
        ],
    )
    .await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.ends_with("task stopped: every feature passes\n"),
        "{stdout}"
    );
    assert!(!stdout.contains(HOOK_TOKEN) && !stderr.contains(HOOK_TOKEN));

    let posts = posts_of(webhook).await;
    assert_eq!(posts.len(), 3);
    for post in &posts {
        assert!(
            post.starts_with(&format!("POST /api/webhooks/7/{HOOK_TOKEN} HTTP/1.1\r\n")),
            "{post}"
        );
        assert_eq!(body_of(post)["allowed_mentions"], json!({"parse": []}));
    }
    let content = |i: usize| body_of(&posts[i])["content"].as_str().unwrap().to_string();
    assert_eq!(
        content(0),
        format!("junees: task in {name} session 1 done: 1/2 passing, new: F1")
    );
    assert_eq!(
        content(1),
        format!("junees: task in {name} session 2 done: 2/2 passing, new: F2")
    );
    assert_eq!(
        content(2),
        format!(
            "junees: task in {name} stopped (complete): every feature passes\n\
             features: 2/2 passing\n\
             sessions: 2\n\
             tokens: input 39, output 18, cache write 0, cache read 0"
        )
    );

    let requests = api.await.unwrap();
    let results = tool_results(&requests[3]);
    assert_eq!(results[0].0, "[exit code 1]");
}

#[tokio::test]
async fn a_failing_webhook_leaves_the_exit_code_alone() {
    let mut replies = init_replies();
    replies.extend([
        // run 1: one session that passes nothing.
        (200, text_reply("Looked around.")),
        // run 2: the session fails.
        (
            400,
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}}),
        ),
    ]);
    let (url, _api) = fake_api(replies).await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;
    junees(root, home.path(), &url, &["task", "init", "make a and b"]).await;

    // The webhook answers 500: the run still ends as it would without it.
    let (hook, webhook) = fake_api(vec![(500, json!({"message": "oops"}))]).await;
    let hook = format!("{hook}/api/webhooks/7/{HOOK_TOKEN}");
    let run = junees_env(
        root,
        home.path(),
        &url,
        &["task", "run", "--max-sessions", "1"],
        &[("JUNEES_NOTIFY_WEBHOOK", &hook)],
    )
    .await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.ends_with("task stopped: reached --max-sessions\n"),
        "{stdout}"
    );
    assert!(
        stderr.contains("  !! notification to 127.0.0.1 failed: HTTP 500 Internal Server Error\n"),
        "{stderr}"
    );
    assert!(!stdout.contains(HOOK_TOKEN) && !stderr.contains(HOOK_TOKEN));
    assert_eq!(posts_of(webhook).await.len(), 1);

    // Nothing listens at the webhook: a failed run still exits with 1.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hook = format!(
        "http://{}/api/webhooks/7/{HOOK_TOKEN}",
        closed.local_addr().unwrap()
    );
    drop(closed);
    let run = junees_env(
        root,
        home.path(),
        &url,
        &["task", "run"],
        &[("JUNEES_NOTIFY_WEBHOOK", &hook)],
    )
    .await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("error: task stopped: session 2 failed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("  !! notification to 127.0.0.1 failed: "),
        "{stderr}"
    );
    assert!(!stdout.contains(HOOK_TOKEN) && !stderr.contains(HOOK_TOKEN));
}

#[tokio::test]
async fn a_run_that_cannot_start_notifies_the_webhook() {
    let (url, _api) = fake_api(init_replies()).await;
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    let name = root.file_name().unwrap().to_string_lossy().to_string();
    git(root, home.path(), &["init", "-q"]).await;
    junees(root, home.path(), &url, &["task", "init", "make a and b"]).await;
    // Left behind by something else: the run refuses to start.
    std::fs::write(root.join("stray.txt"), "left over").unwrap();

    let (hook, webhook) = fake_api(vec![(204, json!({}))]).await;
    let hook = format!("{hook}/api/webhooks/7/{HOOK_TOKEN}");
    let run = junees_env(
        root,
        home.path(),
        &url,
        &["task", "run"],
        &[("JUNEES_NOTIFY_WEBHOOK", &hook)],
    )
    .await;
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("error: the work tree has uncommitted changes"),
        "{stderr}"
    );
    assert!(!stdout.contains(HOOK_TOKEN) && !stderr.contains(HOOK_TOKEN));

    let posts = posts_of(webhook).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(body_of(&posts[0])["allowed_mentions"], json!({"parse": []}));
    let content = body_of(&posts[0])["content"].as_str().unwrap().to_string();
    assert!(
        content.starts_with(&format!(
            "junees: task in {name} run failed: the work tree has uncommitted changes"
        )),
        "{content}"
    );
}

#[tokio::test]
async fn status_without_a_task_says_to_run_init() {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), home.path(), &["init", "-q"]).await;
    let output = status(repo.path(), home.path(), &["--json"]).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no task (no .junees/ folder); start one with `junees task init"),
        "{stderr}"
    );
    assert!(!repo.path().join(".junees").exists());
}

#[tokio::test]
async fn task_mode_refuses_folders_outside_git() {
    let home = tempfile::tempdir().unwrap();
    let plain = tempfile::tempdir().unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(["task", "init", "goal"])
        .current_dir(plain.path())
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .stdin(Stdio::null());
    isolate_git(&mut command, home.path());
    let output = command.output().await.unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not inside a git repository (run `git init` first)"),
        "{stderr}"
    );
    assert!(!plain.path().join(".junees").exists());
}
