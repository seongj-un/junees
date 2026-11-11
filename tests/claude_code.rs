//! Runs `junees task init` and `junees task run` with the `claude-code`
//! backend as real processes, against fake `claude` programs: no API key is
//! needed, API credentials never reach `claude`, the login hint and the
//! stop reach the user and the webhook, and a signal stops `claude`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api};

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

/// A fake `claude` playing `sessions` in order (after answering
/// `--version`, and `auth status` as signed in with the subscription),
/// saving each run's environment as `env-<n>` in `dir`.
fn fake_claude(dir: &Path, sessions: &[String]) -> PathBuf {
    let mut body = format!(
        "#!/bin/bash\n\
         [ -n \"$JUNEES_FAKE_PROBE\" ] && exit 0\n\
         [ \"$1\" = --version ] && echo '2.1.319 (Claude Code)' && exit 0\n\
         [ \"$1\" = auth ] && echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}' && exit 0\n\
         F='{}'\n\
         n=$(( $(cat \"$F/count\" 2>/dev/null || echo 0) + 1 ))\n\
         echo $n > \"$F/count\"\n\
         env -0 > \"$F/env-$n\"\n\
         case $n in\n",
        dir.display()
    );
    for (i, session) in sessions.iter().enumerate() {
        body.push_str(&format!("{})\n{session}\n;;\n", i + 1));
    }
    body.push_str("*) exit 9;;\nesac\n");
    let path = dir.join("claude");
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A process another test forks while the file is open for writing
    // makes exec fail with ETXTBSY until it execs itself.
    for _ in 0..200 {
        match std::process::Command::new(&path)
            .env("JUNEES_FAKE_PROBE", "1")
            .status()
        {
            Ok(_) => return path,
            Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("cannot run the fake claude: {err}"),
        }
    }
    panic!("the fake claude stayed busy");
}

/// The environment of the fake's run `n`.
fn env_of(dir: &Path, n: usize) -> Vec<(String, String)> {
    std::fs::read(dir.join(format!("env-{n}")))
        .unwrap()
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn print(events: &[Value]) -> String {
    let lines: Vec<String> = events.iter().map(Value::to_string).collect();
    format!("cat <<'JSON'\n{}\nJSON", lines.join("\n"))
}

fn answered(text: &str) -> String {
    print(&[
        json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s",
            "message": {"id": "msg_1", "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 10, "output_tokens": 2}}}),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": text,
            "num_turns": 1, "total_cost_usd": 0.01, "session_id": "s",
            "usage": {"input_tokens": 10, "output_tokens": 2}}),
    ])
}

/// The init session: one feature, `done.txt` exists.
fn plan() -> String {
    let list = json!([
        {"id": "F1", "description": "done.txt exists", "verify": "test -f done.txt", "passes": false}
    ]);
    format!(
        "cat > .junees/features.json <<'LIST'\n{list}\nLIST\n\
         echo '# Plan' > .junees/progress.md\n{}",
        answered("Planned.")
    )
}

/// Junees with the claude-code backend chosen by `JUNEES_BACKEND`, the
/// fake as `claude`, and API credentials in its environment that must not
/// reach `claude`.
fn junees(cwd: &Path, home: &Path, bin: &Path, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .args(args)
        .env("JUNEES_BACKEND", "claude-code")
        .env("JUNEES_CLAUDE_BIN", bin)
        .env("ANTHROPIC_API_KEY", "sk-ant-api-must-not-reach-claude")
        .env("ANTHROPIC_AUTH_TOKEN", "auth-must-not-reach-claude")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("CLAUDE_CODE_OAUTH_TOKEN", "the-users-own-fake-token")
        // Each would bill something other than the subscription.
        .env("CLAUDE_CODE_USE_BEDROCK", "1")
        .env("CLAUDE_CODE_USE_VERTEX", "1")
        .env("CLAUDE_CODE_USE_FOUNDRY", "1")
        .env("ANTHROPIC_PROFILE", "profile-must-not-reach-claude")
        .env("ANTHROPIC_FEDERATION_RULE_ID", "rule-must-not-reach-claude")
        .env("ANTHROPIC_ORGANIZATION_ID", "org-must-not-reach-claude")
        .env("JUNEES_HOME", home.join("junees"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    isolate_git(&mut command, home);
    command
}

async fn output(mut command: tokio::process::Command) -> (Output, String, String) {
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, stdout, stderr)
}

#[tokio::test]
async fn claude_code_runs_a_task_without_an_api_key_and_sees_none() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let bin = fake_claude(
        fake.path(),
        &[
            plan(),
            format!("echo done > done.txt\n{}", answered("Built F1.")),
        ],
    );
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;

    let mut init = junees(root, home.path(), &bin, &["task", "init", "make done.txt"]);
    // The backend needs no API key at all.
    init.env_remove("ANTHROPIC_API_KEY");
    let (out, stdout, stderr) = output(init).await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("== sessions run by Claude Code 2.1.319 (Claude Code) =="),
        "{stdout}"
    );
    assert!(stdout.contains("Planned.\n"), "{stdout}");

    // The user may turn Claude Code's credential scrub off, and is told.
    let mut run = junees(root, home.path(), &bin, &["task", "run"]);
    run.env("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB", "0");
    let (out, stdout, stderr) = output(run).await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("!! CLAUDE_CODE_SUBPROCESS_ENV_SCRUB is off"),
        "{stderr}"
    );
    assert!(stdout.contains("Built F1.\n"), "{stdout}");
    assert!(stdout.contains("verified by the harness: F1"), "{stdout}");
    assert!(
        stdout.ends_with("task stopped: every feature passes\n"),
        "{stdout}"
    );
    assert_eq!(
        git(root, home.path(), &["log", "--format=%s"]).await,
        "junees: checkpoint after session 1\njunees: initialize task\n"
    );
    assert_eq!(git(root, home.path(), &["status", "--porcelain"]).await, "");

    for (n, scrub) in [(1, "1"), (2, "0")] {
        let env = env_of(fake.path(), n);
        assert!(
            env.contains(&("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), scrub.into())),
            "run {n}"
        );
        for (key, value) in &env {
            assert!(
                !key.starts_with("ANTHROPIC_")
                    && !key.starts_with("CLAUDE_CODE_USE_")
                    && !value.contains("must-not-reach"),
                "{key} reached claude"
            );
        }
        // The user's own token is passed on untouched, not read.
        assert!(
            env.contains(&(
                "CLAUDE_CODE_OAUTH_TOKEN".into(),
                "the-users-own-fake-token".into()
            )),
            "run {n}"
        );
    }
}

#[tokio::test]
async fn a_claude_code_without_login_tells_how_to_sign_in_and_notifies() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let not_logged_in = format!(
        "{}\nexit 1",
        print(&[
            json!({"type": "result", "subtype": "success", "is_error": true,
            "result": "Not logged in · Please run /login"})
        ])
    );
    let bin = fake_claude(fake.path(), &[plan(), not_logged_in]);
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;
    let (out, stdout, stderr) = output(junees(
        root,
        home.path(),
        &bin,
        &["task", "init", "make done.txt"],
    ))
    .await;
    assert!(out.status.success(), "{stdout}\n{stderr}");

    let (hook, webhook) = fake_api(vec![(204, json!({}))]).await;
    let mut run = junees(root, home.path(), &bin, &["task", "run"]);
    run.env(
        "JUNEES_NOTIFY_WEBHOOK",
        format!("{hook}/api/webhooks/7/t0ken"),
    );
    let (out, stdout, stderr) = output(run).await;
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("Claude Code is not signed in, or its login was refused"),
        "{stderr}"
    );
    assert!(
        stderr
            .contains("run `claude` and then /login (or create a token with `claude setup-token`)"),
        "{stderr}"
    );
    let posts = tokio::time::timeout(Duration::from_secs(5), webhook)
        .await
        .expect("no notification")
        .unwrap();
    assert_eq!(posts.len(), 1);
    let content = body_of(&posts[0])["content"].as_str().unwrap().to_string();
    assert!(content.contains("stopped (error)"), "{content}");
    assert!(content.contains("/login"), "{content}");
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn sigterm_stops_claude_code_and_the_run() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let pid_file = fake.path().join("pid");
    // Like Claude Code's Bash tool, the fake starts its command in a
    // session of its own, out of reach of a signal to its group.
    let slow = format!(
        "echo wip > wip.txt\nsetsid sleep 30 &\necho $! > '{}'\nwait\n",
        pid_file.display()
    );
    let bin = fake_claude(fake.path(), &[plan(), slow]);
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;
    let (out, stdout, stderr) = output(junees(
        root,
        home.path(),
        &bin,
        &["task", "init", "make done.txt"],
    ))
    .await;
    assert!(out.status.success(), "{stdout}\n{stderr}");

    let child = junees(root, home.path(), &bin, &["task", "run"])
        .spawn()
        .unwrap();
    until("claude's command to start", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|text| text.trim().parse::<u32>().is_ok())
    })
    .await;
    let sleeper: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = libc::pid_t::try_from(child.id().unwrap()).unwrap();
    // SAFETY: kill only sends a signal to the child this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(143), "{stdout}\n{stderr}");
    assert!(stdout.ends_with("task stopped: interrupted\n"), "{stdout}");
    until("claude's command to be gone", || !alive(sleeper)).await;
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".junees/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["stop"]["reason"], "interrupted");
    assert_eq!(
        git(root, home.path(), &["log", "--format=%s"]).await,
        "junees: checkpoint after session 1\njunees: initialize task\n"
    );
    assert_eq!(
        git(root, home.path(), &["ls-files", "wip.txt"]).await,
        "wip.txt\n"
    );
}
