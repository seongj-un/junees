//! Runs the conversation mode on the `claude-code` backend as a real
//! process against fake `claude` programs: turns continue one Claude Code
//! session, `-p` and `-c` work, Ctrl-C stops a turn and the next goes on,
//! the usage limit is reported, and no API variable reaches `claude`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;

/// A fake `claude` playing `turns` in order (after answering `--version`,
/// and `auth status` as signed in with the subscription), saving each
/// run's arguments as `args-<n>`, environment as `env-<n>` and stdin (the
/// prompt) as `stdin-<n>` in `dir`.
fn fake_claude(dir: &Path, turns: &[String]) -> PathBuf {
    let mut body = format!(
        "#!/bin/bash\n\
         [ -n \"$JUNEES_FAKE_PROBE\" ] && exit 0\n\
         [ \"$1\" = --version ] && echo '2.1.319 (Claude Code)' && exit 0\n\
         [ \"$1\" = auth ] && echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}' && exit 0\n\
         F='{}'\n\
         n=$(( $(cat \"$F/count\" 2>/dev/null || echo 0) + 1 ))\n\
         echo $n > \"$F/count\"\n\
         printf '%s\\0' \"$@\" > \"$F/args-$n\"\n\
         env -0 > \"$F/env-$n\"\n\
         cat > \"$F/stdin-$n\"\n\
         case $n in\n",
        dir.display()
    );
    for (i, turn) in turns.iter().enumerate() {
        body.push_str(&format!("{})\n{turn}\n;;\n", i + 1));
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

fn split(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|item| !item.is_empty())
        .map(|item| String::from_utf8_lossy(item).into_owned())
        .collect()
}

fn args_of(dir: &Path, n: usize) -> Vec<String> {
    split(&std::fs::read(dir.join(format!("args-{n}"))).unwrap())
}

fn stdin_of(dir: &Path, n: usize) -> String {
    std::fs::read_to_string(dir.join(format!("stdin-{n}"))).unwrap()
}

fn env_of(dir: &Path, n: usize) -> Vec<(String, String)> {
    split(&std::fs::read(dir.join(format!("env-{n}"))).unwrap())
        .into_iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// The value after `flag` in `args`.
fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|arg| arg == flag)?;
    args.get(at + 1).map(String::as_str)
}

fn print(events: &[Value]) -> String {
    let lines: Vec<String> = events.iter().map(Value::to_string).collect();
    format!("cat <<'JSON'\n{}\nJSON", lines.join("\n"))
}

fn delta(id: &str, text: &str) -> Value {
    json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": text}}})
}

/// A turn in Claude Code session `id` that streams `text` in two pieces
/// and answers with it.
fn answer(id: &str, text: &str) -> Vec<Value> {
    let (first, rest) = text.split_at(text.len() / 2);
    let message = format!("m-{text}");
    vec![
        json!({"type": "system", "subtype": "init", "session_id": id}),
        json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
            "event": {"type": "message_start", "message": {"id": message}}}),
        delta(id, first),
        delta(id, rest),
        json!({"type": "stream_event", "session_id": id, "parent_tool_use_id": null,
            "event": {"type": "content_block_stop", "index": 0}}),
        json!({"type": "assistant", "session_id": id, "parent_tool_use_id": null,
            "message": {"id": message, "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 10, "output_tokens": 3}}}),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": text,
            "session_id": id, "num_turns": 1,
            "usage": {"input_tokens": 10, "output_tokens": 3}}),
    ]
}

/// A turn that runs a command, is refused another, then answers.
fn with_tools(id: &str, text: &str) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "system", "subtype": "init", "session_id": id}),
        json!({"type": "assistant", "session_id": id, "parent_tool_use_id": null,
            "message": {"id": "m-tool", "content": [{"type": "tool_use", "id": "t1",
            "name": "Bash", "input": {"command": "ls"}}],
            "usage": {"input_tokens": 5, "output_tokens": 1}}}),
        json!({"type": "user", "session_id": id, "parent_tool_use_id": null,
            "message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": "t1", "content": "a.txt"}]}}),
        json!({"type": "system", "subtype": "permission_denied", "session_id": id,
            "tool_name": "Bash", "tool_use_id": "t2", "decision_reason_type": "rule",
            "decision_reason": "Bash(sudo *) is in --disallowedTools",
            "message": "Permission to use Bash has been denied."}),
    ];
    events.extend(answer(id, text).into_iter().skip(1));
    events
}

/// Junees on the claude-code backend (`JUNEES_BACKEND`), the fake as
/// `claude`, and API credentials in its environment that must not reach
/// `claude`.
fn junees(cwd: &Path, home: &Path, bin: &Path, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("JUNEES_HOME", home.join("junees"))
        .env("JUNEES_BACKEND", "claude-code")
        .env("JUNEES_CLAUDE_BIN", bin)
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-must-not-reach-claude")
        .env("ANTHROPIC_AUTH_TOKEN", "auth-must-not-reach-claude")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("CLAUDE_CODE_USE_BEDROCK", "1")
        .env("CLAUDE_CODE_OAUTH_TOKEN", "the-users-own-fake-token")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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

/// Runs the conversation with `input` on stdin.
async fn converse(command: tokio::process::Command, input: &str) -> (Output, String, String) {
    let mut command = command;
    command.stdin(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, stdout, stderr)
}

fn assert_in_order(text: &str, parts: &[&str]) {
    let mut at = 0;
    for part in parts {
        let found = text[at..]
            .find(part)
            .unwrap_or_else(|| panic!("`{part}` missing after {at}:\n{text}"));
        at += found + part.len();
    }
}

#[tokio::test]
async fn a_conversation_continues_one_claude_code_session() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let bin = fake_claude(
        fake.path(),
        &[
            print(&with_tools("s-1", "Hello there.")),
            print(&answer("s-1", "Still here.")),
        ],
    );
    let (out, stdout, stderr) = converse(
        junees(work.path(), home.path(), &bin, &[]),
        "hello\n-v is a flag\n/usage\n/compact\n/context\n/model\n/exit\n",
    )
    .await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_in_order(
        &stdout,
        &[
            "junees ",
            "(Claude Code 2.1.319 (Claude Code), model of Claude Code's choice)",
            "session ",
            "Hello there.\n",
            "Still here.\n",
            "input 25 tokens (cache write 0, cache read 0), output 7 tokens, \
             2 Claude Code turns, Claude Code session s-1\n",
            "model: of Claude Code's choice (Claude Code runs the conversation)\n",
        ],
    );
    assert_in_order(
        &stderr,
        &[
            "(Claude Code runs commands without Junees approval questions; \
             only the command policy deny rules apply)\n",
            "  -> Bash {\"command\":\"ls\"}\n",
            "  <- Bash ok: a.txt\n",
            "  !! Claude Code refused a tool call: Bash: Bash(sudo *) is in --disallowedTools\n",
            "/compact is not available on the claude-code backend",
            "/context is not available on the claude-code backend",
        ],
    );

    let first = args_of(fake.path(), 1);
    assert_eq!(first[0], "-p");
    assert_eq!(stdin_of(fake.path(), 1), "hello");
    assert!(!first.contains(&"hello".to_string()), "{first:?}");
    assert_eq!(after(&first, "--permission-mode"), Some("default"));
    assert_eq!(after(&first, "--permission-prompts"), Some("none"));
    assert!(first.contains(&"--include-partial-messages".to_string()));
    assert_eq!(after(&first, "--resume"), None);
    // The deny rules come last: the command policy and Junees's settings.
    let deny = first
        .iter()
        .position(|arg| arg == "--disallowedTools")
        .unwrap();
    assert!(first[deny..].contains(&"Bash(sudo *)".to_string()));
    let settings = home.path().join("junees/config.toml");
    let settings = settings.canonicalize().unwrap_or(settings);
    assert!(
        first[deny..].contains(&format!("Read(/{})", settings.display())),
        "{first:?}"
    );
    let second = args_of(fake.path(), 2);
    // A prompt that looks like an option reaches claude on stdin as it is.
    assert_eq!(stdin_of(fake.path(), 2), "-v is a flag");
    assert!(!second.contains(&"-v is a flag".to_string()), "{second:?}");
    assert_eq!(after(&second, "--resume"), Some("s-1"));

    for n in [1, 2] {
        let env = env_of(fake.path(), n);
        for (key, value) in &env {
            assert!(
                !key.starts_with("ANTHROPIC_")
                    && !key.starts_with("CLAUDE_CODE_USE_")
                    && !value.contains("must-not-reach"),
                "{key} reached claude"
            );
        }
        assert!(env.contains(&(
            "CLAUDE_CODE_OAUTH_TOKEN".into(),
            "the-users-own-fake-token".into()
        )));
        assert!(env.contains(&("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB".into(), "1".into())));
    }

    // The session journal holds the Claude Code session and the turns.
    let sessions = home.path().join("junees/sessions");
    let file = std::fs::read_dir(&sessions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let lines: Vec<Value> = std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["backend"], "claude-code");
    assert_eq!(lines[1]["type"], "claude_turn");
    assert_eq!(lines[1]["claude_session"], "s-1");
    assert_eq!(lines[1]["text"], "Hello there.");
    assert_eq!(lines[2]["input"], "-v is a flag");
    assert_eq!(lines.len(), 3);
}

#[tokio::test]
async fn an_input_over_the_argument_limit_reaches_claude() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let bin = fake_claude(fake.path(), &[print(&answer("s-2", "Got it all."))]);
    // One line of 200 KiB with Hangul: over the 128 KiB a single argument
    // may hold on Linux (MAX_ARG_STRLEN).
    let mut large = "\u{d55c}\u{ae00} text ".repeat(200 * 1024 / 11);
    large.truncate(large.trim_end().len());
    assert!(large.len() > 200 * 1024 - 16, "{}", large.len());
    let (out, stdout, stderr) = converse(
        junees(work.path(), home.path(), &bin, &[]),
        &format!("{large}\n/exit\n"),
    )
    .await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("Got it all.\n"), "{stdout}\n{stderr}");
    assert_eq!(stdin_of(fake.path(), 1), large);
}

#[tokio::test]
async fn one_shot_questions_continue_with_dash_c_and_need_no_api_key() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let bin = fake_claude(
        fake.path(),
        &[
            print(&answer("s-9", "First answer.")),
            print(&answer("s-9", "Second answer.")),
            print(&answer("s-new", "Fresh.")),
        ],
    );
    let mut first = junees(work.path(), home.path(), &bin, &["-p", "first"]);
    first.env_remove("ANTHROPIC_API_KEY");
    let (out, stdout, stderr) = output(first).await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // stdout carries only the answer.
    assert_eq!(stdout, "First answer.\n");
    let id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .expect(&stderr)
        .to_string();

    let (out, stdout, stderr) = output(junees(
        work.path(),
        home.path(),
        &bin,
        &["-c", "-p", "second"],
    ))
    .await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout, "Second answer.\n");
    assert!(
        stderr.contains(&format!("resumed {id} (1 Claude Code turns)")),
        "{stderr}"
    );
    assert_eq!(after(&args_of(fake.path(), 2), "--resume"), Some("s-9"));

    // A new conversation starts a new Claude Code session.
    let (out, stdout, stderr) = output(junees(work.path(), home.path(), &bin, &["third"])).await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(after(&args_of(fake.path(), 3), "--resume"), None);
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
async fn ctrl_c_stops_the_turn_and_the_next_one_continues_the_session() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let pid_file = fake.path().join("pid");
    // Like Claude Code's Bash tool, the fake starts its command in a
    // session of its own, out of reach of a signal to its group.
    let slow = format!(
        "{}\nsetsid sleep 30 &\necho $! > '{}'\nwait\nsleep 30\n",
        print(&[
            json!({"type": "system", "subtype": "init", "session_id": "s-5"}),
            delta("s-5", "Working on it"),
        ]),
        pid_file.display()
    );
    let bin = fake_claude(fake.path(), &[slow, print(&answer("s-5", "Back again."))]);
    let mut command = junees(work.path(), home.path(), &bin, &[]);
    command.stdin(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"work\n").await.unwrap();
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
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    until("claude's command to be gone", || !alive(sleeper)).await;
    stdin.write_all(b"again\n/exit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("(turn cancelled)"), "{stderr}");
    // What came before the cancel is still shown, then the next answer.
    assert_in_order(&stdout, &["Working on it\n", "Back again.\n"]);
    assert_eq!(after(&args_of(fake.path(), 2), "--resume"), Some("s-5"));
}

#[tokio::test]
async fn the_usage_limit_is_reported_with_its_reset() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let limited = print(&[
        json!({"type": "system", "subtype": "init", "session_id": "s-2"}),
        json!({"type": "rate_limit_event", "session_id": "s-2", "rate_limit_info":
            {"status": "rejected", "resetsAt": 1_794_205_024}}),
        json!({"type": "result", "subtype": "success", "is_error": true,
            "session_id": "s-2", "api_error_status": 429,
            "result": "You've hit your session limit · resets 6:17am"}),
    ]);
    let bin = fake_claude(fake.path(), &[limited]);
    let (out, stdout, stderr) = output(junees(work.path(), home.path(), &bin, &["-p", "hi"])).await;
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains(
            "error: Claude Code hit the usage limit (resets 2026-11-09 06:17 UTC): \
             You've hit your session limit · resets 6:17am"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("the next input continues the same conversation"),
        "{stderr}"
    );
}
