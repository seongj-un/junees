//! `junees cron` as a program: adding and listing jobs, `cron run` against
//! a fake API with delivery to a fake Discord REST API, and the daemon
//! starting a due job as its own process.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use common::{body_of, fake_api, text_reply};

fn junees(home: &Path, cwd: &Path, api: &str, discord: &str) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", api)
        .env("JUNEES_HOME", home)
        .env("JUNEES_STREAM", "0")
        .env("JUNEES_TIMEZONE", "UTC")
        .env("JUNEES_DISCORD_TOKEN", "fake-bot-token")
        .env("JUNEES_DISCORD_API_URL", discord)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command.env_remove("TZ");
    command
}

async fn output(command: &mut tokio::process::Command, args: &[&str]) -> (bool, String, String) {
    let out = tokio::time::timeout(Duration::from_secs(60), command.args(args).output())
        .await
        .expect("junees did not finish")
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

type Posts = Arc<Mutex<Vec<(String, Value)>>>;

/// A fake Discord REST API: answers every request with 200 `{"id": "1"}`
/// and records (request line, JSON body).
async fn fake_discord() -> (String, Posts) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let posts: Posts = Arc::default();
    let record = Arc::clone(&posts);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let request = loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                if buf.len() >= end + 4 + length {
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let body = serde_json::from_slice(&buf[end + 4..end + 4 + length])
                        .unwrap_or(Value::Null);
                    break Some((line, body));
                }
            };
            if let Some(request) = request {
                record.lock().unwrap().push(request);
            }
            let body = r#"{"id": "1"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (url, posts)
}

fn jobs(home: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("cron/jobs.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn adds_lists_runs_and_delivers_a_job() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (api, requests) = fake_api(vec![(200, text_reply("오늘은 맑음"))]).await;
    let (discord, posts) = fake_discord().await;
    let run = |args: &[&str]| {
        let mut command = junees(home.path(), work.path(), &api, &discord);
        command.args(args);
        async move { output(&mut command, &[]).await }
    };

    let (ok, stdout, stderr) = run(&[
        "cron",
        "add",
        "weekdays 09:00",
        "날씨를 알려 줘",
        "--name",
        "weather",
        "--deliver",
        "discord:42",
    ])
    .await;
    assert!(ok, "{stderr}");
    assert!(stdout.starts_with("added job "), "{stdout}");
    assert!(
        stdout.contains("(weekdays 09:00 (UTC)), next run "),
        "{stdout}"
    );
    assert!(
        stderr.contains("no `junees cron daemon` is running"),
        "{stderr}"
    );
    let id = jobs(home.path())["jobs"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(home.path().join("cron/jobs.json"))
            .unwrap()
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);

    let (ok, stdout, _) = run(&["cron", "list"]).await;
    assert!(ok);
    assert!(
        stdout.starts_with(&format!("{id} weather: weekdays 09:00 (UTC), next ")),
        "{stdout}"
    );
    assert!(stdout.contains("  prompt: 날씨를 알려 줘\n"), "{stdout}");
    assert!(stdout.contains("deliver: discord:42"), "{stdout}");

    // Run it now, by name.
    let (ok, stdout, stderr) = run(&["cron", "run", "weather"]).await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "오늘은 맑음\n");
    assert!(
        stderr.contains(&format!("job {id}: ok; session ")),
        "{stderr}"
    );

    let requests = tokio::time::timeout(Duration::from_secs(10), requests)
        .await
        .unwrap()
        .unwrap();
    let body = body_of(&requests[0]);
    let system = body["system"].to_string();
    assert!(system.contains("scheduled job"), "{system}");
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        tools.contains(&"bash") && !tools.contains(&"memory"),
        "{tools:?}"
    );
    assert!(!tools.contains(&"skill_manage"), "{tools:?}");
    assert!(tools.contains(&"session_search"), "{tools:?}");
    assert_eq!(body["messages"][0]["content"][0]["text"], "날씨를 알려 줘");

    let posts = posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0].0, "post /channels/42/messages http/1.1");
    assert_eq!(
        posts[0].1["content"],
        "**junees cron** `weather`: ok\n오늘은 맑음"
    );

    let last = &jobs(home.path())["jobs"][0]["last"];
    assert_eq!(last["status"], "ok");
    let file = last["output"].as_str().unwrap();
    assert!(
        file.starts_with(&format!("{}/cron/output/{id}/", home.path().display())),
        "{file}"
    );
    let text = std::fs::read_to_string(file).unwrap();
    assert!(text.contains("## Answer\n\n오늘은 맑음\n"), "{text}");
    let session = last["session"].as_str().unwrap();
    assert!(
        home.path()
            .join(format!("sessions/{session}.jsonl"))
            .exists()
    );

    // Pause, resume, remove.
    let (ok, stdout, _) = run(&["cron", "pause", &id[..4]]).await;
    assert!(ok && stdout == format!("paused job {id}\n"), "{stdout}");
    assert_eq!(jobs(home.path())["jobs"][0]["paused"], true);
    let (ok, stdout, _) = run(&["cron", "resume", "weather"]).await;
    assert!(
        ok && stdout.starts_with(&format!("resumed job {id}, next run ")),
        "{stdout}"
    );
    let (ok, _, _) = run(&["cron", "rm", "weather"]).await;
    assert!(ok);
    assert_eq!(jobs(home.path())["jobs"], json!([]));
    let (ok, _, stderr) = run(&["cron", "run", "weather"]).await;
    assert!(!ok && stderr.contains("no job `weather`"), "{stderr}");
}

#[tokio::test]
async fn a_job_in_the_home_folder_needs_allow_home() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (api, requests) = fake_api(vec![(200, text_reply("홈에서 끝"))]).await;
    let run = |args: &[&str]| {
        let mut command = junees(home.path(), work.path(), &api, "http://127.0.0.1:9");
        command.args(args);
        async move { output(&mut command, &[]).await }
    };
    let home_dir = home.path().to_str().unwrap();
    for dir in [home_dir, "/"] {
        let (ok, _, stderr) = run(&["cron", "add", "every 2h", "hi", "--dir", dir]).await;
        assert!(!ok, "{stderr}");
        assert!(
            stderr.contains("is the home folder (or a folder holding it)")
                && stderr.contains("--allow-home"),
            "{stderr}"
        );
    }
    assert!(!home.path().join("cron/jobs.json").exists());

    let (ok, _, stderr) = run(&[
        "cron",
        "add",
        "every 2h",
        "hi",
        "--name",
        "home",
        "--dir",
        home_dir,
        "--allow-home",
    ])
    .await;
    assert!(ok, "{stderr}");
    assert_eq!(jobs(home.path())["jobs"][0]["allow_home"], true);
    let (ok, stdout, stderr) = run(&["cron", "run", "home"]).await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "홈에서 끝\n");
    assert!(stderr.contains("(the home folder)"), "{stderr}");

    // A job saved without the mark (added before it existed) is refused
    // at run time, before the model is asked.
    let path = home.path().join("cron/jobs.json");
    let mut file = jobs(home.path());
    file["jobs"][0]
        .as_object_mut()
        .unwrap()
        .remove("allow_home");
    std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
    let (ok, _, stderr) = run(&["cron", "run", "home"]).await;
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("is the home folder") && stderr.contains("remove the job and add it again"),
        "{stderr}"
    );
    let last = &jobs(home.path())["jobs"][0]["last"];
    assert_eq!(last["status"], "failed");
    assert!(
        last["note"].as_str().unwrap().contains("--allow-home"),
        "{last}"
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), requests)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn jobs_refuse_the_claude_code_backend_unless_allowed() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // Never reached: the refusal comes before Claude Code is asked.
    let missing = home.path().join("no-claude");
    let run = |args: &[&str], extra: &[(&str, &str)]| {
        let mut command = junees(
            home.path(),
            work.path(),
            "http://127.0.0.1:9",
            "http://127.0.0.1:9",
        );
        command
            .env("JUNEES_BACKEND", "claude-code")
            .env("JUNEES_CLAUDE_BIN", &missing)
            .env_remove("JUNEES_CLAUDE_CODE_UNATTENDED")
            .env_remove("JUNEES_SANDBOX");
        command.envs(extra.iter().copied()).args(args);
        async move { output(&mut command, &[]).await }
    };
    let (ok, _, stderr) = run(&["cron", "add", "every 2h", "hi", "--name", "cc"], &[]).await;
    assert!(ok, "{stderr}");

    let (ok, _, stderr) = run(&["cron", "run", "cc"], &[]).await;
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("refuse the claude-code backend")
            && stderr.contains("claude_code_unattended"),
        "{stderr}"
    );
    let last = &jobs(home.path())["jobs"][0]["last"];
    assert_eq!(last["status"], "failed");

    // An explicit `require` wins over the permission.
    let (ok, _, stderr) = run(
        &["cron", "run", "cc"],
        &[
            ("JUNEES_CLAUDE_CODE_UNATTENDED", "1"),
            ("JUNEES_SANDBOX", "require"),
        ],
    )
    .await;
    assert!(!ok && stderr.contains("sandbox = require"), "{stderr}");

    // Allowed: it goes on to Claude Code (missing here) and says the
    // commands run unconfined.
    let (ok, _, stderr) = run(
        &["cron", "run", "cc"],
        &[("JUNEES_CLAUDE_CODE_UNATTENDED", "1")],
    )
    .await;
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("without Junees's sandbox"), "{stderr}");
    assert!(
        !stderr.contains("refuse the claude-code backend"),
        "{stderr}"
    );
}

#[tokio::test]
async fn the_daemon_starts_a_due_job_and_a_failed_run_is_delivered() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // The API fails for good (a 400 is not retried).
    let error =
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}});
    let (api, _requests) = fake_api(vec![(400, error)]).await;
    let (discord, posts) = fake_discord().await;
    let mut add = junees(home.path(), work.path(), &api, &discord);
    let (ok, _, stderr) = output(
        &mut add,
        &["cron", "add", "every 1h", "check", "--deliver", "discord:7"],
    )
    .await;
    assert!(ok, "{stderr}");
    // Due a moment ago, as if the hour had passed.
    let mut file = jobs(home.path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    file["jobs"][0]["next_run"] = json!(now - 5);
    std::fs::write(home.path().join("cron/jobs.json"), file.to_string()).unwrap();

    let mut daemon = junees(home.path(), work.path(), &api, &discord)
        .args(["cron", "daemon"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let last = loop {
        let last = jobs(home.path())["jobs"][0]["last"].clone();
        if last["finished_at"].is_number() {
            break last;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no run recorded: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // SAFETY: the pid is the daemon's, which is still running.
    unsafe {
        libc::kill(daemon.id().unwrap() as i32, libc::SIGTERM);
    }
    let out = tokio::time::timeout(Duration::from_secs(30), daemon.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(out.success());
    let mut stderr = String::new();
    daemon
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert!(stderr.contains("started (due "), "{stderr}");
    assert!(stderr.contains("ended: failed"), "{stderr}");
    assert!(stderr.contains("cron daemon stopped"), "{stderr}");

    assert_eq!(last["status"], "failed");
    assert!(last["note"].as_str().unwrap().contains("400"), "{last}");
    assert_eq!(last["due"], now - 5);
    // Moved to the next hour, not run again.
    let next = jobs(home.path())["jobs"][0]["next_run"].as_i64().unwrap();
    assert!(next > now, "{next}");
    let posts = posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let content = posts[0].1["content"].as_str().unwrap();
    assert!(content.contains(": failed (`"), "{content}");
}
