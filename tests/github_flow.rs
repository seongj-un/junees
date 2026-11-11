//! `junees webhook add-github` as a program: the route, its secret file
//! and the setup text; then `webhook serve` taking a signed
//! `pull_request.opened`, the model's review going to a fake GitHub
//! through the `github` tool (the route allows `review` only), and a second
//! delivery of the same head commit doing nothing.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use common::{body_of, fake_api, text_reply};

/// The port `webhook serve` listens on in this file's test.
const SERVE_PORT: u16 = 18170;
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const GITHUB_TOKEN: &str = "gh-flow-test-token";

fn junees(home: &Path, cwd: &Path, api: &str, github: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", api)
        .env("JUNEES_HOME", home)
        .env("HOME", home)
        .env("JUNEES_STREAM", "0")
        .env("JUNEES_TIMEZONE", "UTC")
        .env("JUNEES_GITHUB_TOKEN", GITHUB_TOKEN)
        .env("JUNEES_GITHUB_API_URL", github)
        .env("JUNEES_GITHUB_REPOS", "o/n")
        .env("JUNEES_GITHUB_ALLOW_UNATTENDED", "review")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for name in [
        "JUNEES_MODEL",
        "JUNEES_BACKEND",
        "JUNEES_NOTIFY_WEBHOOK",
        "JUNEES_WEBHOOK_MAX_TIME",
        "JUNEES_WEBHOOK_MAX_TOKENS",
        "JUNEES_GITHUB_FLOW_MERGE",
        "JUNEES_HIDDEN_ENV",
        "JUNEES_DISCORD_TOKEN",
        "TZ",
    ] {
        command.env_remove(name);
    }
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

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// A fake GitHub REST API: `GET /user`, `GET` pull request 7 (open, at
/// [`HEAD`]) and `201 {}` for anything else; records (request line, JSON
/// body).
async fn fake_github() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let record = Arc::clone(&seen);
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
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let length = head
                    .to_lowercase()
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                if buf.len() >= end + 4 + length {
                    let line = head.lines().next().unwrap_or_default().to_string();
                    let body = serde_json::from_slice(&buf[end + 4..end + 4 + length])
                        .unwrap_or(Value::Null);
                    break Some((line, head, body));
                }
            };
            let Some((line, head, body)) = request else {
                continue;
            };
            // A request without the token is recorded as such.
            let line = if head.contains(&format!("Bearer {GITHUB_TOKEN}")) {
                line
            } else {
                format!("NO TOKEN {line}")
            };
            let path = line.split(' ').nth(1).unwrap_or_default().to_string();
            let path = path.split('?').next().unwrap_or_default();
            let (status, reply) = match (line.split(' ').next().unwrap_or_default(), path) {
                ("GET", "/user") => (200, json!({"login": "junees-bot"})),
                ("GET", "/repos/o/n/pulls/7") => (
                    200,
                    json!({
                        "number": 7, "state": "open", "draft": false, "title": "Fix the parser",
                        "user": {"login": "alice"}, "mergeable": true,
                        "head": {"ref": "feature", "sha": HEAD, "repo": {"full_name": "o/n"}},
                        "base": {"ref": "main", "repo": {"full_name": "o/n"}},
                    }),
                ),
                ("GET", _) => (404, json!({"message": "Not Found"})),
                _ => (
                    201,
                    json!({"id": 99, "state": "COMMENTED", "html_url": "https://github.com/o/n/pull/7"}),
                ),
            };
            record.lock().unwrap().push((line, body));
            let text = reply.to_string();
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (url, seen)
}

/// `sha256=<hex>` of the HMAC-SHA256 of `body` with `secret`.
fn sign(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

fn routes(home: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("webhooks/routes.json")).unwrap())
        .unwrap()
}

/// The model's reply: one `github` review of #7.
fn review_call() -> Value {
    json!({
        "id": "msg_0", "type": "message", "role": "assistant", "model": "m",
        "content": [{
            "type": "tool_use", "id": "t1", "name": "github",
            "input": {
                "action": "review", "repo": "o/n", "number": 7, "event": "COMMENT",
                "commit_id": HEAD,
                "body": "판정: 머지 가능\n\n### 필수\n- 없음\n\n### 권장\n- 없음\n\n### 참고\n- 없음"
            }
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

/// Waits until the route's last run is not `run_id` any more and has
/// finished; returns it.
async fn next_run(home: &Path, after: Option<&str>) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let last = routes(home)["routes"][0]["last"].clone();
        let new = last["run_id"].as_str().is_some_and(|id| Some(id) != after);
        if new && last["finished_at"].is_number() {
            return last;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no run recorded: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn add_github_sets_up_a_route_and_serve_reviews_an_opened_pull_request() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (api, requests) = fake_api(vec![
        (200, review_call()),
        (200, text_reply("리뷰를 남겼습니다.")),
    ])
    .await;
    let (github, seen) = fake_github().await;

    let mut add = junees(home.path(), work.path(), &api, &github);
    let (ok, stdout, stderr) = output(
        &mut add,
        &["webhook", "add-github", "pr", "--repo", "o/n", "--review"],
    )
    .await;
    assert!(ok, "{stderr}");
    let secret_path = home.path().join("secrets/webhook-pr");
    for line in [
        "added GitHub flow route pr: review on o/n (POST /hooks/pr)\n",
        "Content type:  application/json\n",
        &format!("Secret:        the contents of {}", secret_path.display()),
        "Events:        Let me select individual events: Pull requests\n",
        "The model's GitHub writes on this route: review (never merge",
    ] {
        assert!(stdout.contains(line), "{line}: {stdout}");
    }
    // Every setting the review needs is there.
    assert!(!stdout.contains("Still needed"), "{stdout}");
    let secret = std::fs::read_to_string(&secret_path).unwrap();
    let secret = secret.trim().to_string();
    assert_eq!(secret.len(), 64);
    assert!(!stdout.contains(&secret) && !stderr.contains(&secret));
    let mode = std::fs::metadata(&secret_path)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let stored = routes(home.path());
    assert_eq!(stored["format"], 2);
    assert_eq!(stored["routes"][0]["github_allow"], json!(["review"]));
    assert_eq!(stored["routes"][0]["flow"]["repo"], "o/n");
    assert!(home.path().join("flows/review.md").is_file());

    let listen = format!("127.0.0.1:{SERVE_PORT}");
    let mut server = junees(home.path(), work.path(), &api, &github)
        .args(["webhook", "serve", "--listen", &listen])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::net::TcpStream::connect(&listen).await.is_err() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never listened"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let payload = json!({
        "action": "opened",
        "number": 7,
        "pull_request": {"number": 7, "draft": false, "head": {"sha": HEAD, "repo": {"full_name": "o/n"}}},
        "repository": {"full_name": "o/n"},
        "sender": {"login": "alice"},
    })
    .to_string();
    let client = reqwest::Client::new();
    let url = format!("http://{listen}/hooks/pr");
    let send = |delivery: &'static str| {
        client
            .post(&url)
            .header("X-GitHub-Event", "pull_request")
            .header("X-GitHub-Delivery", delivery)
            .header("X-Hub-Signature-256", sign(&secret, payload.as_bytes()))
            .header("content-type", "application/json")
            .body(payload.clone())
            .send()
    };
    let reply = send("d-1").await.unwrap();
    assert_eq!(reply.status(), 202);
    let first = next_run(home.path(), None).await;
    assert_eq!(first["status"], "ok", "{first}");
    assert!(
        first["note"]
            .as_str()
            .unwrap()
            .contains("#7 review: reviewed 0123456"),
        "{first}"
    );

    let writes: Vec<(String, Value)> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(line, _)| !line.starts_with("GET "))
        .cloned()
        .collect();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0].0, "POST /repos/o/n/pulls/7/reviews HTTP/1.1");
    assert_eq!(writes[0].1["event"], "COMMENT");
    assert_eq!(writes[0].1["commit_id"], HEAD);
    let requests = tokio::time::timeout(Duration::from_secs(10), requests)
        .await
        .unwrap()
        .unwrap();
    let prompt = body_of(&requests[0])["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        prompt.contains(">Fix the parser</webhook-data-"),
        "{prompt}"
    );
    let system = body_of(&requests[0])["system"].to_string();
    assert!(system.contains("GitHub pull request flow"), "{system}");
    let output_files = std::fs::read_dir(home.path().join("webhooks/output/pr"))
        .unwrap()
        .count();
    assert_eq!(output_files, 1);

    // Another delivery of the same commit: no model run, no write, no
    // output file.
    let reply = send("d-2").await.unwrap();
    assert_eq!(reply.status(), 202);
    let second = next_run(home.path(), first["run_id"].as_str()).await;
    assert_eq!(second["status"], "ok", "{second}");
    assert!(
        second["note"]
            .as_str()
            .unwrap()
            .contains("skipped: done for 0123456 already"),
        "{second}"
    );
    let writes = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(line, _)| !line.starts_with("GET "))
        .count();
    assert_eq!(writes, 1);
    let output_files = std::fs::read_dir(home.path().join("webhooks/output/pr"))
        .unwrap()
        .count();
    assert_eq!(output_files, 1);

    let mut list = junees(home.path(), work.path(), &api, &github);
    let (ok, stdout, stderr) = output(&mut list, &["webhook", "list"]).await;
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("pr (github flow on o/n): review, github writes: review\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  #7 review: reviewed 0123456 ("),
        "{stdout}"
    );

    // SAFETY: the pid is the server's, which is still running.
    unsafe {
        libc::kill(server.id().unwrap() as i32, libc::SIGTERM);
    }
    let status = tokio::time::timeout(Duration::from_secs(30), server.wait())
        .await
        .expect("the server stops on SIGTERM")
        .unwrap();
    assert!(status.success());
    let mut stderr = String::new();
    server
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert!(!stderr.contains(&secret), "{stderr}");
    assert!(!stderr.contains(GITHUB_TOKEN), "{stderr}");
}
