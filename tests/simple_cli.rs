//! The plain usage as real processes: `junees "<question>"` with the
//! settings file, the first-run setup on a terminal (and not off one),
//! `junees task "<goal>"`, `junees task`, `junees status`, and the slash
//! commands of the conversation.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

mod common;
use common::{body_of, fake_api, text_reply};

/// Junees with nothing from the test's environment but `PATH`: the
/// state folder is `home`, git has a fixed identity and no system config.
fn junees(home: &Path, cwd: &Path, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("JUNEES_HOME", home.join("junees"))
        .env("JUNEES_STREAM", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Junees Test")
        .env("GIT_AUTHOR_EMAIL", "test@junees.invalid")
        .env("GIT_COMMITTER_NAME", "Junees Test")
        .env("GIT_COMMITTER_EMAIL", "test@junees.invalid")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn settings_path(home: &Path) -> PathBuf {
    home.join("junees/config.toml")
}

fn write_settings(home: &Path, text: &str) {
    std::fs::create_dir_all(home.join("junees")).unwrap();
    std::fs::write(settings_path(home), text).unwrap();
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

fn api_key_of(request: &str) -> String {
    request
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("x-api-key:")
                .map(|_| line)
        })
        .and_then(|line| line.split_once(':'))
        .map(|(_, key)| key.trim().to_string())
        .expect("no x-api-key header")
}

#[tokio::test]
async fn a_bare_question_runs_once_with_the_key_from_the_settings_file() {
    let home = tempfile::tempdir().unwrap();
    let (url, server) = fake_api(vec![
        (200, text_reply("From the file key.")),
        (200, text_reply("From the env key.")),
    ])
    .await;
    write_settings(
        home.path(),
        &format!("api_key = \"sk-ant-from-file\"\nbase_url = \"{url}\"\nmodel = \"file-model\"\n"),
    );

    let (out, stdout, stderr) =
        output(junees(home.path(), home.path(), &["이 폴더에 뭐가 있어?"])).await;
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout, "From the file key.\n");

    // The environment wins over the file.
    let mut again = junees(home.path(), home.path(), &["again"]);
    again
        .env("ANTHROPIC_API_KEY", "sk-ant-from-env")
        .env("JUNEES_MODEL", "env-model");
    let (out, stdout, stderr) = output(again).await;
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout, "From the env key.\n");

    let requests = server.await.unwrap();
    assert_eq!(api_key_of(&requests[0]), "sk-ant-from-file");
    assert_eq!(body_of(&requests[0])["model"], "file-model");
    assert_eq!(
        body_of(&requests[0])["messages"][0]["content"][0]["text"],
        "이 폴더에 뭐가 있어?"
    );
    assert_eq!(api_key_of(&requests[1]), "sk-ant-from-env");
    assert_eq!(body_of(&requests[1])["model"], "env-model");
}

#[tokio::test]
async fn a_bad_settings_file_names_the_line() {
    let home = tempfile::tempdir().unwrap();
    write_settings(home.path(), "# mine\nmodel = claude-sonnet-4-6\n");
    let (out, _, stderr) = output(junees(home.path(), home.path(), &["hi"])).await;
    assert!(!out.status.success());
    let path = settings_path(home.path());
    assert!(
        stderr.contains(&format!("{} is not a valid settings file", path.display())),
        "{stderr}"
    );
    assert!(
        stderr.contains("line 2: the value needs quotes"),
        "{stderr}"
    );
}

#[tokio::test]
async fn setup_starts_on_a_broken_settings_file() {
    let home = tempfile::tempdir().unwrap();
    write_settings(home.path(), "api_key = sk-ant-api03-SECRETvalue\n");
    let (out, _, stderr) = output(junees(home.path(), home.path(), &["setup"])).await;
    assert!(!out.status.success());
    let path = settings_path(home.path());
    assert!(
        stderr.contains(&format!(
            "{} is broken (line 1: the value needs quotes",
            path.display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("Setup starts from scratch"), "{stderr}");
    // It got past the file to the setup itself.
    assert!(
        stderr.contains("`junees setup` asks its questions on a terminal"),
        "{stderr}"
    );
    assert!(!stderr.contains("SECRET"), "{stderr}");
    // Nothing was written.
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "api_key = sk-ant-api03-SECRETvalue\n"
    );
}

#[tokio::test]
async fn off_a_terminal_nothing_is_asked() {
    let home = tempfile::tempdir().unwrap();
    for args in [&[][..], &["hi"]] {
        let (out, stdout, stderr) = output(junees(home.path(), home.path(), args)).await;
        assert!(!out.status.success(), "{args:?}");
        assert!(
            stderr.contains("error: no API key: run `junees setup`, or set ANTHROPIC_API_KEY"),
            "{stderr}"
        );
        assert!(!stderr.contains("How should junees"), "{stderr}");
        assert_eq!(stdout, "");
    }
    let (out, _, stderr) = output(junees(home.path(), home.path(), &["setup"])).await;
    assert!(!out.status.success());
    assert!(
        stderr.contains("asks its questions on a terminal"),
        "{stderr}"
    );
    assert!(!settings_path(home.path()).exists());
}

/// A pseudo-terminal as a person would have it, echo on: (the side the
/// test reads and writes, the side junees gets).
fn terminal() -> (std::fs::File, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let (mut leader, mut follower) = (0, 0);
    // SAFETY: openpty writes the two descriptors it opens.
    unsafe {
        assert_eq!(
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            ),
            0
        );
        (
            std::fs::File::from_raw_fd(leader),
            std::os::fd::OwnedFd::from_raw_fd(follower),
        )
    }
}

/// Waits until the screen shows `text` after `from`; returns where it
/// ends.
async fn wait_for(screen: &Arc<Mutex<String>>, text: &str, from: usize) -> usize {
    for _ in 0..1000 {
        {
            let shown = screen.lock().unwrap();
            if let Some(at) = shown.get(from..).and_then(|rest| rest.find(text)) {
                return from + at + text.len();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("`{text}` never showed:\n{}", screen.lock().unwrap());
}

const KEY: &str = "sk-ant-api03-typed-on-the-terminal";

#[tokio::test]
async fn the_first_run_on_a_terminal_sets_up_without_showing_the_key() {
    let home = tempfile::tempdir().unwrap();
    let (url, server) = fake_api(vec![(200, text_reply("Hi from the fake API."))]).await;
    let (leader, follower) = terminal();
    let mut child = junees(home.path(), home.path(), &["hello"])
        .env("ANTHROPIC_BASE_URL", &url)
        .stdin(Stdio::from(follower.try_clone().unwrap()))
        .stdout(Stdio::from(follower.try_clone().unwrap()))
        .stderr(Stdio::from(follower))
        .spawn()
        .unwrap();
    let screen = Arc::new(Mutex::new(String::new()));
    let mut keyboard = leader.try_clone().unwrap();
    let reader = {
        let screen = Arc::clone(&screen);
        let mut leader = leader;
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            // EIO once junees closed the terminal.
            while let Ok(n) = leader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..n]).replace('\r', "");
                screen.lock().unwrap().push_str(&text);
            }
        })
    };
    let mut type_line = |text: &str| {
        use std::io::Write;
        keyboard.write_all(text.as_bytes()).unwrap();
        keyboard.write_all(b"\n").unwrap();
    };

    let at = wait_for(&screen, "choice [1/2] > ", 0).await;
    type_line("2");
    let at = wait_for(&screen, "Anthropic API key (not shown as you type", at).await;
    let at = wait_for(&screen, "> ", at).await;
    type_line(KEY);
    let at = wait_for(&screen, "API key saved.", at).await;
    let at = wait_for(
        &screen,
        "Model for the conversation [claude-sonnet-4-6] > ",
        at,
    )
    .await;
    type_line("");
    let at = wait_for(&screen, "[2h] > ", at).await;
    type_line("");
    let at = wait_for(&screen, "or none [none] > ", at).await;
    type_line("");
    wait_for(&screen, "Hi from the fake API.", at).await;

    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("junees did not exit")
        .unwrap();
    tokio::task::spawn_blocking(move || reader.join().unwrap())
        .await
        .unwrap();
    let shown = screen.lock().unwrap().clone();
    assert!(status.success(), "{shown}");
    // Plain answers show as typed; the key never does.
    assert!(shown.contains("choice [1/2] > 2\n"), "{shown}");
    assert!(!shown.contains("typed-on-the-terminal"), "{shown}");
    assert!(
        shown.contains(&format!("Saved {}.", settings_path(home.path()).display())),
        "{shown}"
    );

    let path = settings_path(home.path());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&format!("api_key = \"{KEY}\"\n")), "{text}");
    assert!(text.contains("backend = \"api\"\n"), "{text}");
    assert!(text.contains("task_max_time = \"2h\"\n"), "{text}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // Then the question ran with that key.
    let requests = server.await.unwrap();
    assert_eq!(api_key_of(&requests[0]), KEY);
}

/// Git in `repo` like junees runs it in these tests.
async fn git(repo: &Path, home: &Path, args: &[&str]) -> String {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Junees Test")
        .env("GIT_AUTHOR_EMAIL", "test@junees.invalid")
        .env("GIT_COMMITTER_NAME", "Junees Test")
        .env("GIT_COMMITTER_EMAIL", "test@junees.invalid")
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap()
}

/// A fake `claude`, signed in with the subscription, that plans one
/// feature (`done.txt` exists) in its first session and builds it in the
/// second; a third session fails.
fn fake_claude(dir: &Path) -> PathBuf {
    let answered = |text: &str| {
        let events = [
            json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s",
                "message": {"id": "msg_1", "content": [{"type": "text", "text": text}],
                "usage": {"input_tokens": 10, "output_tokens": 2}}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "result": text,
                "num_turns": 1, "total_cost_usd": 0.01, "session_id": "s",
                "usage": {"input_tokens": 10, "output_tokens": 2}}),
        ];
        let lines: Vec<String> = events.iter().map(Value::to_string).collect();
        format!("cat <<'JSON'\n{}\nJSON", lines.join("\n"))
    };
    let list = json!([
        {"id": "F1", "description": "done.txt exists", "verify": "test -f done.txt", "passes": false}
    ]);
    let body = format!(
        "#!/bin/bash\n\
         [ -n \"$JUNEES_FAKE_PROBE\" ] && exit 0\n\
         [ \"$1\" = --version ] && echo '2.1.319 (Claude Code)' && exit 0\n\
         [ \"$1\" = auth ] && echo '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}' && exit 0\n\
         F='{dir}'\n\
         n=$(( $(cat \"$F/count\" 2>/dev/null || echo 0) + 1 ))\n\
         echo $n > \"$F/count\"\n\
         case $n in\n\
         1)\ncat > .junees/features.json <<'LIST'\n{list}\nLIST\necho '# Plan' > .junees/progress.md\n{plan}\n;;\n\
         2)\necho done > done.txt\n{built}\n;;\n\
         *) exit 9;;\nesac\n",
        dir = dir.display(),
        plan = answered("Planned."),
        built = answered("Built F1."),
    );
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

fn sessions_run(fake: &Path) -> String {
    std::fs::read_to_string(fake.join("count")).unwrap_or_default()
}

#[tokio::test]
async fn task_with_a_goal_plans_and_runs_and_task_and_status_continue() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let bin = fake_claude(fake.path());
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;
    // The subscription backend from the settings file: no API key at all.
    write_settings(
        home.path(),
        &format!(
            "backend = \"claude-code\"\nclaude_bin = \"{}\"\ntask_max_time = \"2h\"\n",
            bin.display()
        ),
    );

    let (out, stdout, stderr) = output(junees(home.path(), root, &["task", "make done.txt"])).await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let planned = stdout.find("Planned.\n").expect(&stdout);
    let init = stdout
        .find("== init done: 1 features, committed ==")
        .expect(&stdout);
    let built = stdout.find("Built F1.\n").expect(&stdout);
    assert!(planned < init && init < built, "{stdout}");
    assert!(
        stdout.ends_with("task stopped: every feature passes\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("junees task run"), "{stdout}");
    assert_eq!(
        git(root, home.path(), &["log", "--format=%s"]).await,
        "junees: checkpoint after session 1\njunees: initialize task\n"
    );

    // `junees task` continues the same task: nothing is left to do.
    let (out, stdout, stderr) = output(junees(home.path(), root, &["task"])).await;
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.ends_with("task stopped: every feature passes\n"),
        "{stdout}"
    );
    assert_eq!(sessions_run(fake.path()), "2\n");

    // A second goal in the same repository is refused, pointing at `junees task`.
    let (out, _, stderr) = output(junees(home.path(), root, &["task", "another"])).await;
    assert!(!out.status.success());
    assert!(
        stderr.contains("continue it with `junees task`"),
        "{stderr}"
    );

    let (out, stdout, stderr) = output(junees(home.path(), root, &["status"])).await;
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.contains("goal: make done.txt\n"), "{stdout}");
    assert!(stdout.contains("features: 1/1 passing"), "{stdout}");
    assert!(stdout.contains("  [x] F1  done.txt exists\n"), "{stdout}");
}

#[tokio::test]
async fn slash_commands_run_tasks_switch_backends_and_sessions() {
    let home = tempfile::tempdir().unwrap();
    let fake = tempfile::tempdir().unwrap();
    let bin = fake_claude(fake.path());
    let elsewhere = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    git(root, home.path(), &["init", "-q"]).await;

    // An earlier conversation in another folder, to pick from /sessions.
    let (url, server) = fake_api(vec![(200, text_reply("Earlier answer."))]).await;
    let mut earlier = junees(home.path(), elsewhere.path(), &["earlier question"]);
    earlier
        .env("ANTHROPIC_API_KEY", "sk-ant-env")
        .env("ANTHROPIC_BASE_URL", &url);
    let (out, _, stderr) = output(earlier).await;
    assert!(out.status.success(), "{stderr}");
    server.await.unwrap();
    let id = stderr
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .expect(&stderr)
        .to_string();

    // The conversation reads the commands from stdin; no model call is made.
    let mut conversation = junees(home.path(), root, &[]);
    conversation
        .env("ANTHROPIC_API_KEY", "sk-ant-env")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .env("JUNEES_CLAUDE_BIN", &bin)
        .stdin(Stdio::piped());
    let mut child = conversation.spawn().unwrap();
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(
                b"/backend\n/backend claude-code\n/task make done.txt\n/status\n\
                  /sessions\n7\n1\n/model\n/model claude-haiku-4-5\n/new\n\
                  /backend api\n/sessions\n1\n/model\n/wat\n/exit\n",
            )
            .await
            .unwrap();
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");

    let order = [
        "the conversation and /task run on api\n",
        // The conversation moves to Claude Code, in a new session.
        "Claude Code 2.1.319 (Claude Code)\n",
        "(new conversation) session ",
        "the conversation and /task now run on claude-code (this session; \
         `junees setup` changes the default)\n",
        "Planned.\n",
        "Built F1.\n",
        "task stopped: every feature passes\n",
        "features: 1/1 passing",
        &format!("  1   {id}  "),
        "pick a number from 1 to 1\n",
        "model: of Claude Code's choice (Claude Code runs the conversation)\n",
        "model: claude-haiku-4-5 (this session, passed to Claude Code with --model",
        "(new conversation) session ",
        // Back on the API, the API conversation can be continued.
        "(new conversation) session ",
        "the conversation and /task now run on api",
        &format!("  1   {id}  "),
        &format!(
            "resumed {id} (2 messages, started in {})\n",
            elsewhere.path().display()
        ),
        "model: claude-sonnet-4-6\n",
    ];
    let mut at = 0;
    for text in order {
        let found = stdout[at..]
            .find(text)
            .unwrap_or_else(|| panic!("`{text}` missing after {at}:\n{stdout}\n{stderr}"));
        at += found + text.len();
    }
    assert!(
        stderr.contains("unknown command /wat; /help lists the commands"),
        "{stderr}"
    );
    // Claude Code cannot continue a conversation Junees's agent held.
    assert!(
        stderr.contains(&format!(
            "error: session {id} was run on the API, and this conversation runs on \
             claude-code; continue it with `/backend api`"
        )),
        "{stderr}"
    );
    // The task ran in the repository, on the backend /backend picked.
    assert_eq!(sessions_run(fake.path()), "2\n");
    assert!(root.join("done.txt").exists());
    // Session-only: nothing was written to the settings.
    assert!(!settings_path(home.path()).exists());
}

#[tokio::test]
async fn help_starts_with_the_plain_usage() {
    let home = tempfile::tempdir().unwrap();
    let (out, stdout, _) = output(junees(home.path(), home.path(), &["--help"])).await;
    assert!(out.status.success());
    assert!(
        stdout.starts_with(
            "junees - an AI agent in your terminal\n\nUsage:\n  junees                  talk"
        ),
        "{stdout}"
    );
    let advanced = stdout.find("\nAdvanced:\n").expect(&stdout);
    assert!(stdout[advanced..].contains("junees task init"), "{stdout}");
}
