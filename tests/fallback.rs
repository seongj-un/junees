//! The fallback model with a real binary against loopback fake APIs: a
//! blocked main model hands the turn to the fallback (here an
//! OpenAI-compatible server), usage is stamped with the model that
//! answered, errors another model would get too stay errors, and an
//! unattended provider gives up at once on a wait it may not make.

mod common;

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use common::junees_binary;

const WAIT: Duration = Duration::from_secs(30);

/// Serves `replies` (status, `retry-after`, body) to consecutive
/// connections, then 500s; records every request.
async fn serve(replies: Vec<(u16, Option<u64>, Value)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_request(&mut socket).await;
            record.lock().unwrap().push(request);
            let (status, retry_after, body) = replies.next().unwrap_or((
                500,
                None,
                json!({"error": {"type": "api_error", "message": "x"}}),
            ));
            let body = body.to_string();
            let retry_after = retry_after
                .map(|secs| format!("retry-after: {secs}\r\n"))
                .unwrap_or_default();
            let response = format!(
                "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n{retry_after}\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.ok();
            socket.shutdown().await.ok();
        }
    });
    (url, seen)
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            if buf.len() >= end + 4 + length {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn limited() -> Value {
    json!({"type": "error", "error": {"type": "rate_limit_error", "message": "usage limit"}})
}

fn openai_reply(text: &str) -> Value {
    json!({
        "id": "c1", "object": "chat.completion", "model": "small-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3}
    })
}

async fn run(home: &Path, main: &str, env: &[(&str, &str)]) -> std::process::Output {
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut command = junees_binary(&work, home);
    command
        .args(["-p", "hello"])
        .env("JUNEES_HOME", home.join("state"))
        .env("ANTHROPIC_API_KEY", "sk-fake-main")
        .env("ANTHROPIC_BASE_URL", main)
        .env("JUNEES_STREAM", "0")
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    tokio::time::timeout(WAIT, command.output())
        .await
        .expect("junees did not exit")
        .unwrap()
}

#[tokio::test]
async fn a_blocked_main_model_hands_the_turn_to_an_openai_fallback() {
    let home = tempfile::tempdir().unwrap();
    let (main, main_seen) = serve(vec![(429, Some(900), limited())]).await;
    let (fallback, fallback_seen) =
        serve(vec![(200, None, openai_reply("from the fallback"))]).await;
    let fallback_url = format!("{fallback}/v1");
    let output = run(
        home.path(),
        &main,
        &[
            ("JUNEES_MODEL", "big-model"),
            ("JUNEES_FALLBACK_MODEL", "small-model"),
            ("JUNEES_FALLBACK_PROVIDER", "openai"),
            ("JUNEES_FALLBACK_BASE_URL", &fallback_url),
            // A key from elsewhere is not sent to the fallback's address.
            ("OPENAI_API_KEY", "sk-fake-openai"),
        ],
    )
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("from the fallback"), "{stdout}");
    assert!(
        stderr.contains(
            "big-model 모델이 사용 한도로 막혀 small-model 모델로 이어 갑니다 (다시 시도: 15분 뒤)"
        ),
        "{stderr}"
    );
    // retry-after 900 is longer than fallback_after: no retries first.
    assert_eq!(main_seen.lock().unwrap().len(), 1);
    let requests = fallback_seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(
        request.starts_with("POST /v1/chat/completions"),
        "{request}"
    );
    assert!(request.contains(r#""model":"small-model""#), "{request}");
    assert!(!request.contains("sk-fake"), "{request}");

    // The usage entry names the model that answered.
    let sessions = home.path().join("state/sessions");
    let journal: String = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default())
        .collect();
    let usage: Vec<Value> = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "usage")
        .collect();
    assert_eq!(usage.len(), 1, "{journal}");
    assert_eq!(usage[0]["model"], "small-model", "{journal}");
    assert_eq!(usage[0]["input_tokens"], 7);
}

#[tokio::test]
async fn errors_another_model_would_get_too_do_not_reach_the_fallback() {
    for status in [400u16, 401] {
        let home = tempfile::tempdir().unwrap();
        let error =
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": "no"}});
        let (main, _) = serve(vec![(status, None, error)]).await;
        let (fallback, fallback_seen) = serve(vec![]).await;
        let output = run(
            home.path(),
            &main,
            &[
                ("JUNEES_FALLBACK_MODEL", "claude-small"),
                ("JUNEES_FALLBACK_BASE_URL", &fallback),
                ("JUNEES_FALLBACK_API_KEY", "sk-fake-fallback"),
            ],
        )
        .await;
        assert!(!output.status.success());
        assert!(fallback_seen.lock().unwrap().is_empty(), "{status}");
    }
}

fn config(main: &str, wait_limit: &str) -> junees::config::Config {
    junees::config::Config::from_lookup(|key| match key {
        "ANTHROPIC_API_KEY" => Some("sk-fake".into()),
        "ANTHROPIC_BASE_URL" => Some(main.into()),
        "JUNEES_WAIT_LIMIT" => Some(wait_limit.into()),
        "JUNEES_STREAM" => Some("0".into()),
        _ => None,
    })
    .unwrap()
}

async fn ask(
    provider: &dyn junees::provider::Provider,
) -> Result<(), junees::provider::ProviderError> {
    let messages = [junees::message::Message::user_text("hi")];
    let (events, _received) = tokio::sync::mpsc::unbounded_channel();
    provider
        .complete_streaming(
            junees::provider::CompletionRequest {
                system: "",
                messages: &messages,
                tools: &[],
                max_tokens: 10,
                cache: false,
                thinking: false,
                web_search: false,
            },
            &events,
        )
        .await
        .map(drop)
}

#[tokio::test]
async fn an_unattended_run_does_not_start_a_wait_it_may_not_finish() {
    // Longer than wait_limit: one request, no retries, no wait.
    let (main, seen) = serve(vec![(429, Some(3600), limited())]).await;
    let provider = junees::provider::unattended_from_config(&config(&main, "30m"), None).unwrap();
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(WAIT, ask(provider.as_ref()))
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(junees::provider::ProviderError::Api {
            status: 429,
            retry_after: Some(3600),
            ..
        })
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(seen.lock().unwrap().len(), 1);

    // Within wait_limit but past the run's time limit.
    let (main, seen) = serve(vec![(429, Some(100), limited())]).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let provider =
        junees::provider::unattended_from_config(&config(&main, "30m"), Some(deadline)).unwrap();
    let started = std::time::Instant::now();
    assert!(
        tokio::time::timeout(WAIT, ask(provider.as_ref()))
            .await
            .unwrap()
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A short wait is made, and the same request goes again.
    let (main, seen) = serve(vec![
        (429, Some(1), limited()),
        (
            200,
            None,
            json!({"id": "m", "type": "message", "role": "assistant", "model": "m",
                   "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
                   "usage": {"input_tokens": 1, "output_tokens": 1}}),
        ),
    ])
    .await;
    let provider = junees::provider::unattended_from_config(&config(&main, "30m"), None).unwrap();
    tokio::time::timeout(WAIT, ask(provider.as_ref()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_cron_run_does_not_wait_past_wait_limit() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (main, seen) = serve(vec![(429, Some(3600), limited())]).await;
    let cron = |args: &[&str]| {
        let mut command = junees_binary(&work, home.path());
        command
            .args(args)
            .env("JUNEES_HOME", home.path().join("state"))
            .env("ANTHROPIC_API_KEY", "sk-fake-main")
            .env("ANTHROPIC_BASE_URL", &main)
            .env("JUNEES_STREAM", "0")
            .env("JUNEES_WAIT_LIMIT", "30m")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null());
        async move {
            tokio::time::timeout(WAIT, command.output())
                .await
                .expect("junees did not exit")
                .unwrap()
        }
    };
    let added = cron(&["cron", "add", "every 2h", "hi", "--name", "limited"]).await;
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let started = std::time::Instant::now();
    let run = cron(&["cron", "run", "limited"]).await;
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    // The provider of an unattended run: one request, no retries (a plain
    // one would retry for minutes), and the reason it did not wait.
    assert!(started.elapsed() < Duration::from_secs(10), "{all}");
    assert_eq!(seen.lock().unwrap().len(), 1, "{all}");
    assert!(all.contains("failed (API error 429"), "{all}");
    // The run's output file says why it did not wait.
    let outputs = home.path().join("state/cron/output");
    let written: String = std::fs::read_dir(&outputs)
        .unwrap()
        .flat_map(|job| std::fs::read_dir(job.unwrap().path()).unwrap())
        .map(|file| std::fs::read_to_string(file.unwrap().path()).unwrap())
        .collect();
    assert!(written.contains("wait_limit 30분"), "{written}");
}
