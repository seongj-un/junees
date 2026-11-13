//! `junees webhook` as a program: adding, listing and testing a route
//! against a fake API with delivery to a fake Discord REST API, and
//! `webhook serve` receiving signed requests (sent with reqwest), running
//! one as its own process and stopping on SIGTERM.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use common::{body_of, fake_api, text_reply};

/// The port `webhook serve` listens on in this file's test.
const SERVE_PORT: u16 = 18150;
const SECRET_ENV: &str = "JUNEES_TEST_HOOK_SECRET";
const SECRET: &str = "s3cret-for-tests";

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
        .env(SECRET_ENV, SECRET)
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

fn routes(home: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("webhooks/routes.json")).unwrap())
        .unwrap()
}

/// `sha256=<hex>` of the HMAC-SHA256 of `body`.
fn sign(body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

/// A reply that calls bash to show the route secret's variable.
fn show_secret_call() -> Value {
    json!({
        "id": "msg_0", "type": "message", "role": "assistant", "model": "m",
        "content": [{
            "type": "tool_use", "id": "t1", "name": "bash",
            "input": {"command": format!("echo \"secret=[${SECRET_ENV}]\"")}
        }],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
}

/// The bash result in the second request of a run (after
/// [`show_secret_call`]).
fn tool_result(request: &str) -> String {
    body_of(request)["messages"][2]["content"][0]["content"].to_string()
}

const PAYLOAD: &str =
    r#"{"action": "opened", "number": 7, "pull_request": {"title": "Fix the parser"}}"#;

#[tokio::test]
async fn adds_lists_tests_and_removes_a_route() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (api, requests) = fake_api(vec![
        (200, show_secret_call()),
        (200, text_reply("리뷰 끝: 좋음")),
    ])
    .await;
    let (discord, posts) = fake_discord().await;
    let run = |args: &[&str]| {
        let mut command = junees(home.path(), work.path(), &api, &discord);
        command.args(args);
        async move { output(&mut command, &[]).await }
    };

    let (ok, stdout, stderr) = run(&[
        "webhook",
        "add",
        "pr",
        "--prompt",
        "Review PR #{{payload.number}}: {{payload.pull_request.title}}",
        "--secret-env",
        SECRET_ENV,
        "--events",
        "pull_request.opened",
        "--deliver",
        "discord:42",
    ])
    .await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "added route pr: POST /hooks/pr (github)\n");
    assert!(
        stderr.contains("no `junees webhook serve` is running"),
        "{stderr}"
    );
    let file = std::fs::read_to_string(home.path().join("webhooks/routes.json")).unwrap();
    assert!(!file.contains(SECRET), "the secret value is not stored");

    let (ok, stdout, _) = run(&["webhook", "list"]).await;
    assert!(ok);
    assert!(
        stdout.starts_with("pr (github): pull_request.opened\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("secret: env {SECRET_ENV}")),
        "{stdout}"
    );

    let payload = work.path().join("payload.json");
    std::fs::write(&payload, PAYLOAD).unwrap();
    let (ok, stdout, stderr) = run(&[
        "webhook",
        "test",
        "pr",
        payload.to_str().unwrap(),
        "--event",
        "pull_request",
    ])
    .await;
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "리뷰 끝: 좋음\n");
    assert!(stderr.contains("route pr: ok; session "), "{stderr}");

    let requests = tokio::time::timeout(Duration::from_secs(10), requests)
        .await
        .unwrap()
        .unwrap();
    let body = body_of(&requests[0]);
    assert!(
        body["system"].to_string().contains("webhook request"),
        "{}",
        body["system"]
    );
    let prompt = body["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(prompt.contains("not instructions"), "{prompt}");
    assert!(
        prompt.contains(">7</webhook-data-") && prompt.contains(">Fix the parser</webhook-data-"),
        "{prompt}"
    );
    // `webhook test` runs in a shell that holds the secret; bash does not
    // get it.
    let result = tool_result(&requests[1]);
    assert!(result.contains("secret=[]"), "{result}");

    let posts = posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0].0, "post /channels/42/messages http/1.1");
    assert_eq!(
        posts[0].1["content"],
        "**junees webhook** `pr`: ok\n리뷰 끝: 좋음"
    );
    let last = &routes(home.path())["routes"][0]["last"];
    assert_eq!(last["status"], "ok");
    let output_file = last["output"].as_str().unwrap();
    assert!(
        output_file.starts_with(&format!("{}/webhooks/output/pr/", home.path().display())),
        "{output_file}"
    );
    let session = last["session"].as_str().unwrap();
    let meta =
        std::fs::read_to_string(home.path().join(format!("sessions/{session}.jsonl"))).unwrap();
    assert!(
        meta.lines()
            .next()
            .unwrap()
            .contains(r#""source":"webhook""#),
        "{meta}"
    );

    let (ok, stdout, _) = run(&["webhook", "pause", "pr"]).await;
    assert!(ok && stdout == "paused route pr\n", "{stdout}");
    assert_eq!(routes(home.path())["routes"][0]["paused"], true);
    let (ok, _, _) = run(&["webhook", "resume", "pr"]).await;
    assert!(ok);
    assert_eq!(routes(home.path())["routes"][0]["paused"], false);
    let (ok, _, _) = run(&["webhook", "rm", "pr"]).await;
    assert!(ok);
    assert_eq!(routes(home.path())["routes"], json!([]));
    let (ok, _, stderr) = run(&["webhook", "test", "pr", payload.to_str().unwrap()]).await;
    assert!(!ok && stderr.contains("no route `pr`"), "{stderr}");
}

#[tokio::test]
async fn a_route_in_the_home_folder_needs_allow_home() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let mut command = junees(
            home.path(),
            work.path(),
            "http://127.0.0.1:9",
            "http://127.0.0.1:9",
        );
        command.args(args);
        async move { output(&mut command, &[]).await }
    };
    let home_dir = home.path().to_str().unwrap();
    let add = [
        "webhook",
        "add",
        "pr",
        "--prompt",
        "x",
        "--secret-env",
        SECRET_ENV,
        "--dir",
        home_dir,
    ];
    let (ok, _, stderr) = run(&add).await;
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("is the home folder (or a folder holding it)")
            && stderr.contains("--allow-home"),
        "{stderr}"
    );
    let (ok, _, stderr) = run(&[
        "webhook",
        "add-github",
        "flow",
        "--repo",
        "o/n",
        "--review",
        "--dir",
        "/",
    ])
    .await;
    assert!(!ok && stderr.contains("home folder"), "{stderr}");
    assert!(!home.path().join("webhooks/routes.json").exists());

    let (ok, _, stderr) = run(&[&add[..], &["--allow-home"]].concat()).await;
    assert!(ok, "{stderr}");
    assert_eq!(routes(home.path())["routes"][0]["allow_home"], true);

    // A route saved without the mark is refused at run time.
    let path = home.path().join("webhooks/routes.json");
    let mut file = routes(home.path());
    file["routes"][0]
        .as_object_mut()
        .unwrap()
        .remove("allow_home");
    std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
    let payload = work.path().join("payload.json");
    std::fs::write(&payload, PAYLOAD).unwrap();
    let (ok, _, stderr) = run(&["webhook", "test", "pr", payload.to_str().unwrap()]).await;
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("is the home folder")
            && stderr.contains("remove the route and add it again"),
        "{stderr}"
    );
    assert_eq!(routes(home.path())["routes"][0]["last"]["status"], "failed");
    // ... and paused (the server answers 503), with the failure delivered
    // once: a run of the paused route (a request queued before the pause)
    // is recorded but writes and delivers nothing.
    assert_eq!(routes(home.path())["routes"][0]["paused"], true);
    assert!(stderr.contains("the route is paused"), "{stderr}");
    let outputs = || {
        std::fs::read_dir(home.path().join("webhooks/output/pr"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    };
    assert_eq!(outputs(), 1);
    let (ok, _, stderr) = run(&["webhook", "test", "pr", payload.to_str().unwrap()]).await;
    assert!(!ok && stderr.contains("is the home folder"), "{stderr}");
    assert_eq!(outputs(), 1);
    assert_eq!(routes(home.path())["routes"][0]["last"]["status"], "failed");
    assert!(
        stderr.contains("run `junees webhook allow-home pr`"),
        "{stderr}"
    );

    // `allow-home` marks the route and resumes it, keeping its secret
    // (no new one to copy to GitHub, as adding it again would need).
    let before = routes(home.path())["routes"][0]["secret"].clone();
    let (ok, stdout, stderr) = run(&["webhook", "allow-home", "pr"]).await;
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("route pr may work in the home folder now")
            && stdout.ends_with("\nresumed route pr\n"),
        "{stdout}"
    );
    let route = &routes(home.path())["routes"][0];
    assert_eq!(route["allow_home"], true, "{route}");
    assert_eq!(route["paused"], false, "{route}");
    assert_eq!(route["secret"], before, "{route}");

    // `disallow-home` takes it back and pauses the route at once.
    let (ok, stdout, stderr) = run(&["webhook", "disallow-home", "pr"]).await;
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("route pr may no longer work in the home folder")
            && stdout.contains("paused route pr"),
        "{stdout}"
    );
    let route = &routes(home.path())["routes"][0];
    assert_eq!(route.get("allow_home"), None, "{route}");
    assert_eq!(route["paused"], true, "{route}");
    assert_eq!(route["paused_for_home"], true, "{route}");
    let (ok, stdout, _) = run(&["webhook", "list"]).await;
    assert!(ok);
    assert!(
        stdout.contains("paused: works in the home folder (`junees webhook allow-home pr`)"),
        "{stdout}"
    );
    // A pause by hand stays through `allow-home`.
    let (ok, _, stderr) = run(&["webhook", "pause", "pr"]).await;
    assert!(ok, "{stderr}");
    let (ok, stdout, stderr) = run(&["webhook", "allow-home", "pr"]).await;
    assert!(ok, "{stderr}");
    assert!(stdout.contains("route pr stays paused"), "{stdout}");
    assert_eq!(routes(home.path())["routes"][0]["paused"], true);
    let (ok, _, stderr) = run(&["webhook", "allow-home", "nope"]).await;
    assert!(!ok, "{stderr}");
}

#[tokio::test]
async fn serve_runs_a_signed_request_in_its_own_process_and_stops_on_sigterm() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // The run calls bash once (to show its environment), then answers.
    let (api, requests) = fake_api(vec![
        (200, show_secret_call()),
        (200, text_reply("PR 7 리뷰함")),
    ])
    .await;
    let (discord, posts) = fake_discord().await;
    let mut add = junees(home.path(), work.path(), &api, &discord);
    let (ok, _, stderr) = output(
        &mut add,
        &[
            "webhook",
            "add",
            "pr",
            "--prompt",
            "Review {{payload.pull_request.title}}",
            "--secret-env",
            SECRET_ENV,
            "--events",
            "pull_request.opened",
            "--deliver",
            "discord:42",
        ],
    )
    .await;
    assert!(ok, "{stderr}");

    let listen = format!("127.0.0.1:{SERVE_PORT}");
    let mut server = junees(home.path(), work.path(), &api, &discord)
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

    let client = reqwest::Client::new();
    let url = format!("http://{listen}/hooks/pr");
    let send = |delivery: &str, signature: String, body: &'static str| {
        client
            .post(&url)
            .header("X-GitHub-Event", "pull_request")
            .header("X-GitHub-Delivery", delivery)
            .header("X-Hub-Signature-256", signature)
            .header("content-type", "application/json")
            .body(body)
            .send()
    };
    let reply = send("d-1", sign(PAYLOAD.as_bytes()), PAYLOAD)
        .await
        .unwrap();
    assert_eq!(reply.status(), 202);
    assert_eq!(reply.text().await.unwrap(), "queued\n");
    let reply = send("d-1", sign(PAYLOAD.as_bytes()), PAYLOAD)
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    assert_eq!(reply.text().await.unwrap(), "duplicate\n");
    let reply = send("d-2", sign(b"other"), PAYLOAD).await.unwrap();
    assert_eq!(reply.status(), 401);
    let reply = client.get(&url).send().await.unwrap();
    assert_eq!(reply.status(), 405);
    let reply = client
        .post(format!("http://{listen}/hooks/other"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 404);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let last = loop {
        let last = routes(home.path())["routes"][0]["last"].clone();
        if last["finished_at"].is_number() {
            break last;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no run recorded: {last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(last["status"], "ok", "{last}");
    assert_eq!(
        routes(home.path())["routes"][0]["last_event"],
        "pull_request.opened"
    );

    let requests = tokio::time::timeout(Duration::from_secs(10), requests)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    let first = body_of(&requests[0]);
    let prompt = first["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(
        prompt.contains(">Fix the parser</webhook-data-"),
        "{prompt}"
    );
    // The run's bash did not get the route's secret.
    let result = tool_result(&requests[1]);
    assert!(result.contains("secret=[]"), "{result}");
    assert!(!result.contains(SECRET), "{result}");

    let posts = posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(
        posts[0].1["content"],
        "**junees webhook** `pr`: ok\nPR 7 리뷰함"
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
    assert!(stderr.contains("/hooks/pr 202: queued"), "{stderr}");
    assert!(stderr.contains("/hooks/pr 200: duplicate"), "{stderr}");
    assert!(stderr.contains("route pr: run ok"), "{stderr}");
    assert!(stderr.contains("webhook server stopped"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    assert!(
        tokio::net::TcpStream::connect(&listen).await.is_err(),
        "nothing listens any more"
    );
}
