//! `junees dashboard`: the real binary on a loopback port, asked with raw
//! HTTP requests so every header is the test's own.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Child;

/// Fake secrets, each of which must never show in an answer.
const ANTHROPIC_KEY: &str = "sk-ant-api03-Dash9Board7Fake5Key3Anthropic1XyZ";
const OPENAI_KEY: &str = "sk-proj-Dash8Board6Fake4Key2OpenAi0AbCdEfGh";
const HOOK_SECRET: &str = "hook-secret-Dash7Board5Fake3Value1";
const HOOK_SECRET_ENV: &str = "JUNEES_TEST_HOOK_SECRET";
const DISCORD_TOKEN: &str = "MTA5ODc2NTQzMjEwOTg3NjU0.GdAsHb.Dash6Board4Fake2DiscordTokenValue0";
/// A key pasted into a conversation.
const PASTED_KEY: &str = "sk-ant-api03-Pasted1In2The3Chat4Session5Key6AbCdEf";

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    work: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, work) = (root.join("state"), root.join("work"));
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&work).unwrap();
        set_mode(&state, 0o700);
        let config = state.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "timezone = \"UTC\"\nmodel = \"claude-test-model\"\n\
                 openai_api_key = \"{OPENAI_KEY}\"\ndiscord_token = \"{DISCORD_TOKEN}\"\n"
            ),
        )
        .unwrap();
        set_mode(&config, 0o600);
        Self {
            _dir: dir,
            root,
            state,
            work,
        }
    }

    fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut command = common::junees_binary(&self.work, &self.root);
        command
            .args(args)
            .env("JUNEES_HOME", &self.state)
            .env("ANTHROPIC_API_KEY", ANTHROPIC_KEY)
            .env(HOOK_SECRET_ENV, HOOK_SECRET)
            // No claude, no bwrap: those parts of doctor are skipped.
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn run(&self, args: &[&str]) {
        let output = tokio::time::timeout(Duration::from_secs(60), self.command(args).output())
            .await
            .expect("junees did not exit")
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A session journal written by hand, as `junees` writes them.
    fn journal(&self, id: &str, lines: &[Value]) {
        let dir = self.state.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = json!({
            "type": "meta", "id": id, "format": 8, "created_at": 1_763_000_000u64,
            "cwd": self.work.to_str().unwrap(), "model": "claude-sonnet-4-6", "version": "0",
        });
        let mut text = meta.to_string() + "\n";
        for line in lines {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
    }
}

fn set_mode(path: &Path, bits: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
}

fn user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "text", "text": text}]})
}

fn assistant(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "text", "text": text}]})
}

fn usage(ts: u64, input: u64, output: u64) -> Value {
    json!({"type": "usage", "input_tokens": input, "output_tokens": output, "ts": ts, "model": "claude-sonnet-4-6"})
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The dashboard, started with `--no-open --listen 127.0.0.1:0`.
struct Dashboard {
    child: Child,
    port: u16,
    token: String,
}

impl Dashboard {
    async fn start(home: &Home) -> Self {
        let mut child = home
            .command(&["dashboard", "--no-open", "--listen", "127.0.0.1:0"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("no address printed")
            .unwrap()
            .expect("the dashboard exited");
        let url = line
            .strip_prefix("Junees dashboard: http://127.0.0.1:")
            .unwrap_or_else(|| panic!("not an address: {line}"));
        let (port, token) = url.split_once("/#token=").unwrap();
        // Read the rest, so the dashboard never writes to a closed pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Self {
            child,
            port: port.parse().unwrap(),
            token: token.to_string(),
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Sends `head` (the request line and headers, without the blank line)
    /// and reads the whole answer.
    async fn raw(&self, head: &str) -> Reply {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        stream
            .write_all(format!("{head}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
            .await
            .expect("no answer")
            .unwrap();
        Reply::parse(&bytes)
    }

    /// `GET path` with the right host and token.
    async fn get(&self, path: &str) -> Reply {
        self.raw(&format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}",
            self.host(),
            self.token
        ))
        .await
    }

    async fn json(&self, path: &str) -> Value {
        let reply = self.get(path).await;
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        assert_eq!(
            reply.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        serde_json::from_str(&reply.body).unwrap()
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn parse(bytes: &[u8]) -> Self {
        let text = String::from_utf8_lossy(bytes).to_string();
        let (head, body) = text.split_once("\r\n\r\n").expect("no head");
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").unwrap();
                (name.to_ascii_lowercase(), value.to_string())
            })
            .collect();
        Self {
            status,
            headers,
            body: body.to_string(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn error(&self) -> Value {
        let value: Value = serde_json::from_str(&self.body).unwrap();
        assert!(value["message"].is_string(), "{value}");
        value["error"].clone()
    }
}

const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
         connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cache-control", "no-store"),
];

fn has_security_headers(reply: &Reply, what: &str) {
    for (name, value) in SECURITY_HEADERS {
        assert_eq!(reply.header(name), Some(*value), "{what}: {name}");
    }
    assert!(
        !reply
            .headers
            .iter()
            .any(|(name, _)| name.starts_with("access-control-")),
        "{what}: no CORS headers"
    );
}

#[tokio::test]
async fn the_dashboard_serves_the_contract_and_guards_it() {
    let home = Home::new();
    home.run(&[
        "cron",
        "add",
        "every 2h",
        "check the build",
        "--name",
        "daily",
        "--dir",
        home.work.to_str().unwrap(),
    ])
    .await;
    home.run(&[
        "webhook",
        "add",
        "gh",
        "--prompt",
        "Review PR #{{payload.number}}",
        "--secret-env",
        HOOK_SECRET_ENV,
        "--dir",
        home.work.to_str().unwrap(),
    ])
    .await;
    let ts = now() - 60;
    home.journal(
        "20261113-010203-abcd",
        &[
            user(&format!(
                "Fix the login bug\nmy key is {PASTED_KEY}, use it"
            )),
            assistant("Done."),
            usage(ts, 1000, 200),
            user("thanks"),
            assistant("You are welcome."),
            usage(ts, 10, 5),
        ],
    );
    let dashboard = Dashboard::start(&home).await;

    // The guards.
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nHost: {}",
            dashboard.host()
        ))
        .await;
    assert_eq!((reply.status, reply.error()), (401, json!("unauthorized")));
    has_security_headers(&reply, "401");
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}x",
            dashboard.host(),
            dashboard.token
        ))
        .await;
    assert_eq!(reply.status, 401);
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nHost: evil.example:{}\r\nAuthorization: Bearer {}",
            dashboard.port, dashboard.token
        ))
        .await;
    assert_eq!(
        (reply.status, reply.error()),
        (421, json!("misdirected_request"))
    );
    has_security_headers(&reply, "421");
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nAuthorization: Bearer {}",
            dashboard.token
        ))
        .await;
    assert_eq!(reply.status, 421, "no Host");
    let reply = dashboard
        .raw(&format!(
            "POST /api/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0",
            dashboard.host(),
            dashboard.token
        ))
        .await;
    assert_eq!(
        (reply.status, reply.error()),
        (405, json!("method_not_allowed"))
    );
    assert_eq!(reply.header("allow"), Some("GET"));
    has_security_headers(&reply, "405");
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nOrigin: http://evil.example",
            dashboard.host(),
            dashboard.token
        ))
        .await;
    assert_eq!(
        (reply.status, reply.error()),
        (403, json!("forbidden_origin"))
    );
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nHost: {0}\r\nAuthorization: Bearer {1}\r\nOrigin: http://{0}",
            dashboard.host(),
            dashboard.token
        ))
        .await;
    assert_eq!(reply.status, 200, "same origin: {}", reply.body);
    let reply = dashboard
        .raw(&format!(
            "GET /api/status HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 1025",
            dashboard.host(),
            dashboard.token
        ))
        .await;
    assert_eq!(
        (reply.status, reply.error()),
        (413, json!("body_too_large"))
    );
    let reply = dashboard
        .raw(&format!(
            "GET / HTTP/1.1\r\nHost: {}\r\nX-Big: {}",
            dashboard.host(),
            "a".repeat(9 * 1024)
        ))
        .await;
    assert_eq!(reply.status, 431);
    for (path, status, code) in [
        ("/api/nothing", 404, "not_found"),
        ("/api/sessions?limit=0", 400, "bad_query"),
        ("/api/sessions?limit=201", 400, "bad_query"),
        ("/api/usage?days=366", 400, "bad_query"),
        ("/api/usage?days=x", 400, "bad_query"),
    ] {
        let reply = dashboard.get(path).await;
        assert_eq!(
            (reply.status, reply.error()),
            (status, json!(code)),
            "{path}"
        );
    }

    // The page.
    for (path, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
    ] {
        let reply = dashboard
            .raw(&format!(
                "GET {path} HTTP/1.1\r\nHost: localhost:{}",
                dashboard.port
            ))
            .await;
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(reply.header("content-type"), Some(content_type), "{path}");
        has_security_headers(&reply, path);
    }
    let reply = dashboard
        .raw(&format!(
            "GET /dev/fixtures/status.json HTTP/1.1\r\nHost: {}",
            dashboard.host()
        ))
        .await;
    assert_eq!((reply.status, reply.error()), (404, json!("not_found")));

    // The API.
    let status = dashboard.json("/api/status").await;
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["home"], home.state.to_str().unwrap());
    assert_eq!(status["backend"], "api");
    assert_eq!(status["provider"], "anthropic");
    assert_eq!(status["model"], "claude-test-model");
    assert!(status["uptime_secs"].is_u64());
    assert_eq!(
        status["keys"]["anthropic"],
        json!({"set": true, "source": "env"})
    );
    // Read from the file with the default address, as the settings do.
    assert_eq!(
        status["keys"]["openai"],
        json!({"set": true, "source": "config.toml"})
    );
    assert!(status["sandbox"]["landlock"].is_boolean());
    assert!(status["sandbox"]["abi"].is_u64() || status["sandbox"]["abi"].is_null());

    let sessions = dashboard.json("/api/sessions").await;
    let list = sessions["sessions"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{sessions}");
    let session = &list[0];
    assert_eq!(session["id"], "20261113-010203-abcd");
    assert_eq!(session["title"], "Fix the login bug");
    assert_eq!(session["cwd"], home.work.to_str().unwrap());
    assert!(session["updated_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(session["messages"], 4);
    assert_eq!(session["input_tokens"], 1010);
    assert_eq!(session["output_tokens"], 205);
    assert!(session["cost_usd"].as_f64().unwrap() > 0.0);
    let preview = session["preview"].as_str().unwrap();
    assert!(
        preview.starts_with("Fix the login bug\nmy key is "),
        "{preview}"
    );
    assert!(preview.contains("[hidden]"), "{preview}");
    assert_eq!(
        dashboard.json("/api/sessions?limit=1").await["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let usage = dashboard.json("/api/usage?days=7").await;
    let days = usage["days"].as_array().unwrap();
    assert_eq!(days.len(), 1, "{usage}");
    for field in [
        "date",
        "model",
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "cost_usd",
    ] {
        assert!(days[0].get(field).is_some(), "{field} in {usage}");
    }
    assert_eq!(days[0]["model"], "claude-sonnet-4-6");
    assert_eq!(usage["total"]["input_tokens"], 1010);
    assert_eq!(usage["total"]["output_tokens"], 205);
    assert!(usage["total"]["cost_usd"].as_f64().unwrap() > 0.0);

    let cron = dashboard.json("/api/cron").await;
    let job = &cron["jobs"][0];
    assert_eq!(job["name"], "daily", "{cron}");
    assert_eq!(job["schedule"], "every 2h");
    assert_eq!(job["dir"], home.work.to_str().unwrap());
    assert_eq!(
        (&job["paused"], &job["paused_for_home"], &job["allow_home"]),
        (&json!(false), &json!(false), &json!(false))
    );
    assert!(job["next_run"].as_str().unwrap().ends_with('Z'));
    assert_eq!(job["last_run"], Value::Null);
    assert!(job["id"].is_string());

    let webhooks = dashboard.json("/api/webhooks").await;
    let route = &webhooks["routes"][0];
    assert_eq!(route["name"], "gh", "{webhooks}");
    assert_eq!(route["kind"], "github");
    assert_eq!(route["dir"], home.work.to_str().unwrap());
    assert_eq!(route["has_secret"], true);
    assert_eq!(route["paused"], false);
    assert_eq!(route["allow_home"], false);
    assert_eq!(route["last_run"], Value::Null);

    let doctor = dashboard.json("/api/doctor").await;
    let sections = doctor["sections"].as_array().unwrap();
    assert!(!sections.is_empty(), "{doctor}");
    for section in sections {
        assert!(section["name"].is_string());
        for check in section["checks"].as_array().unwrap() {
            assert!(check["name"].is_string(), "{check}");
            assert!(check["detail"].is_string(), "{check}");
            let status = check["status"].as_str().unwrap();
            assert!(["ok", "warn", "fail"].contains(&status), "{check}");
        }
    }

    // No secret in any answer.
    for path in [
        "/api/status",
        "/api/sessions",
        "/api/usage",
        "/api/cron",
        "/api/webhooks",
        "/api/doctor",
    ] {
        let body = dashboard.get(path).await.body;
        for secret in [
            ANTHROPIC_KEY,
            OPENAI_KEY,
            HOOK_SECRET,
            DISCORD_TOKEN,
            PASTED_KEY,
        ] {
            assert!(!body.contains(secret), "{path} shows a secret: {body}");
            assert!(
                !body.contains(&secret[..16]),
                "{path} shows part of a secret: {body}"
            );
        }
        assert!(!body.contains(&dashboard.token), "{path} shows the token");
    }
    drop(dashboard.child);
}

#[tokio::test]
async fn a_closed_stdout_does_not_stop_the_dashboard() {
    let home = Home::new();
    // A pipe whose reading end is closed before the dashboard starts, so
    // its first print fails (closing it after the spawn races with the
    // print, which a full pipe buffer takes without an error).
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let mut child = home
        .command(&["dashboard", "--no-open", "--listen", "127.0.0.1:0"])
        .stdout(writer)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(30), stderr.next_line())
        .await
        .expect("no warning printed")
        .unwrap()
        .expect("the dashboard exited");
    let address = line
        .strip_prefix("Cannot print the dashboard's address; it serves on ")
        .and_then(|rest| rest.strip_suffix('.'))
        .unwrap_or_else(|| panic!("not the warning: {line}"))
        .to_string();
    let mut stream = TcpStream::connect(&address).await.unwrap();
    stream
        .write_all(format!("GET / HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(Reply::parse(&bytes).status, 200);
    assert!(child.try_wait().unwrap().is_none(), "the dashboard exited");
}

// macOS gives loopback 127.0.0.1 only; Linux routes all of 127.0.0.0/8.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn another_loopback_address_serves_its_own_host() {
    let home = Home::new();
    let mut child = home
        .command(&["dashboard", "--no-open", "--listen", "127.0.0.2:0"])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("no address printed")
        .unwrap()
        .expect("the dashboard exited");
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let rest = line
        .strip_prefix("Junees dashboard: http://")
        .unwrap_or_else(|| panic!("not an address: {line}"));
    let (address, token) = rest.split_once("/#token=").unwrap();
    assert!(address.starts_with("127.0.0.2:"), "{address}");
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET /api/status HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let reply = Reply::parse(&bytes);
    assert_eq!(reply.status, 200, "{}", reply.body);
    drop(child);
}

#[tokio::test]
async fn an_address_off_loopback_is_refused() {
    let home = Home::new();
    let output = home
        .command(&["dashboard", "--no-open", "--listen", "0.0.0.0:0"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("loopback"), "{stderr}");
}

/// `POST path` with `headers` (lines without the final blank) and `body`.
async fn post(dashboard: &Dashboard, path: &str, headers: &str, body: &str) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", dashboard.port))
        .await
        .unwrap();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n",
        dashboard.host(),
        body.len()
    );
    for line in headers.lines().filter(|line| !line.is_empty()) {
        request.push_str(line);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut bytes))
        .await
        .expect("no answer")
        .unwrap();
    Reply::parse(&bytes)
}

/// The headers of a write from the page: its Origin and the token.
fn from_page(dashboard: &Dashboard) -> String {
    format!(
        "Origin: http://{}\nAuthorization: Bearer {}",
        dashboard.host(),
        dashboard.token
    )
}

#[tokio::test]
async fn a_session_reads_page_by_page_without_secrets_or_thinking() {
    let home = Home::new();
    let id = "20261113-010203-abcd";
    home.journal(
        id,
        &[
            json!({"type": "message", "role": "user", "ts": 1_763_000_000u64,
                   "content": [{"type": "text", "text": format!("Fix the login bug\nkey {PASTED_KEY}")}]}),
            json!({"type": "message", "role": "assistant", "ts": 1_763_000_010u64, "content": [
                {"type": "thinking", "thinking": "a private plan", "signature": "sig"},
                {"type": "text", "text": "z".repeat(9_000)},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls -la"}}]}),
            json!({"type": "message", "role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "e".repeat(3_000), "is_error": true}]}),
            assistant("Done."),
        ],
    );
    let dashboard = Dashboard::start(&home).await;

    let page = dashboard.json(&format!("/api/sessions/{id}")).await;
    assert_eq!(page["id"], id);
    assert_eq!(page["title"], "Fix the login bug");
    assert_eq!(page["cwd"], home.work.to_str().unwrap());
    assert!(page["updated_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!((&page["total"], &page["offset"]), (&json!(4), &json!(0)));
    let messages = page["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["at"], "2025-11-13T02:13:20Z");
    assert_eq!(messages[3]["at"], Value::Null);
    let text = messages[0]["blocks"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Fix the login bug\nkey "), "{text}");
    assert!(text.contains("[hidden]"), "{text}");
    let blocks = messages[1]["blocks"].as_array().unwrap();
    assert_eq!(blocks[0], json!({"type": "thinking"}));
    assert_eq!(blocks[1]["text"].as_str().unwrap().chars().count(), 8_000);
    assert_eq!(blocks[1]["truncated"], true);
    assert_eq!(
        blocks[2],
        json!({"type": "tool_use", "name": "bash", "input_preview": "ls -la"})
    );
    let result = &messages[2]["blocks"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["text_preview"].as_str().unwrap().len(), 2_000);
    assert_eq!(result["truncated"], true);
    assert!(messages[3]["blocks"][0].get("truncated").is_none());
    let body = dashboard.get(&format!("/api/sessions/{id}")).await.body;
    assert!(
        !body.contains(PASTED_KEY) && !body.contains(&PASTED_KEY[..16]),
        "{body}"
    );
    assert!(!body.contains("a private plan"), "{body}");

    let page = dashboard
        .json(&format!("/api/sessions/{id}?offset=1&limit=2"))
        .await;
    let indexes: Vec<&Value> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| &message["index"])
        .collect();
    assert_eq!(indexes, [&json!(1), &json!(2)]);
    assert_eq!((&page["total"], &page["offset"]), (&json!(4), &json!(1)));
    let page = dashboard
        .json(&format!("/api/sessions/{id}?offset=9"))
        .await;
    assert_eq!(page["messages"], json!([]));

    for (path, status, code) in [
        ("/api/sessions/nope-1234", 404, "not_found"),
        ("/api/sessions/..%2Fconfig", 400, "bad_id"),
        ("/api/sessions/../config.toml", 400, "bad_id"),
        ("/api/sessions/a.jsonl", 400, "bad_id"),
        (&format!("/api/sessions/{id}?limit=201"), 400, "bad_query"),
        (&format!("/api/sessions/{id}?offset=-1"), 400, "bad_query"),
    ] {
        let reply = dashboard.get(path).await;
        assert_eq!(
            (reply.status, reply.error()),
            (status, json!(code)),
            "{path}"
        );
    }
    // A link in the sessions folder is not followed.
    std::os::unix::fs::symlink(
        home.state.join(format!("sessions/{id}.jsonl")),
        home.state.join("sessions/linked.jsonl"),
    )
    .unwrap();
    assert_eq!(dashboard.get("/api/sessions/linked").await.status, 404);
    // ... and not listed, so the list never offers a session the view refuses.
    let ids: Vec<Value> = dashboard.json("/api/sessions").await["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].clone())
        .collect();
    assert_eq!(ids, [json!(id)]);
}

#[tokio::test]
async fn pause_and_resume_need_the_page_and_change_the_stores() {
    let home = Home::new();
    let work = home.work.to_str().unwrap().to_string();
    home.run(&[
        "cron", "add", "every 2h", "check", "--name", "daily", "--dir", &work,
    ])
    .await;
    home.run(&[
        "webhook",
        "add",
        "gh",
        "--prompt",
        "x",
        "--secret-env",
        HOOK_SECRET_ENV,
        "--dir",
        &work,
    ])
    .await;
    let jobs_path = home.state.join("cron/jobs.json");
    let read_jobs = || -> Value {
        serde_json::from_str(&std::fs::read_to_string(&jobs_path).unwrap()).unwrap()
    };
    let id = read_jobs()["jobs"][0]["id"].as_str().unwrap().to_string();
    let dashboard = Dashboard::start(&home).await;
    let pause = format!("/api/cron/{id}/pause");
    let page = from_page(&dashboard);

    // The guards, none of which writes or logs.
    let token = format!("Authorization: Bearer {}", dashboard.token);
    let reply = post(&dashboard, &pause, &token, "").await;
    assert_eq!(
        (reply.status, reply.error()),
        (403, json!("forbidden_origin"))
    );
    let reply = post(
        &dashboard,
        &pause,
        &format!("Origin: http://evil.example\n{token}"),
        "",
    )
    .await;
    assert_eq!(
        (reply.status, reply.error()),
        (403, json!("forbidden_origin"))
    );
    let reply = post(
        &dashboard,
        &pause,
        &format!("Origin: http://{}", dashboard.host()),
        "",
    )
    .await;
    assert_eq!((reply.status, reply.error()), (401, json!("unauthorized")));
    let reply = post(
        &dashboard,
        &pause,
        &format!("{page}\nContent-Type: text/plain"),
        "",
    )
    .await;
    assert_eq!(
        (reply.status, reply.error()),
        (415, json!("unsupported_media_type"))
    );
    let reply = post(&dashboard, &pause, &page, "{\"paused\":true}").await;
    assert_eq!((reply.status, reply.error()), (400, json!("bad_body")));
    let reply = dashboard.get(&pause).await;
    assert_eq!((reply.status, reply.header("allow")), (405, Some("POST")));
    let reply = post(&dashboard, "/api/cron", &page, "").await;
    assert_eq!((reply.status, reply.header("allow")), (405, Some("GET")));
    assert_eq!(read_jobs()["jobs"][0]["paused"], false);
    assert!(!home.state.join("dashboard.log").exists());

    // Pause and resume.
    let reply = post(
        &dashboard,
        &pause,
        &format!("{page}\nContent-Type: application/json"),
        "{}",
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    has_security_headers(&reply, "pause");
    let job: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!((&job["id"], &job["paused"]), (&json!(id), &json!(true)));
    assert_eq!(job["schedule"], "every 2h");
    assert_eq!(read_jobs()["jobs"][0]["paused"], true);
    let reply = post(&dashboard, &format!("/api/cron/{id}/resume"), &page, "").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let job: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(job["paused"], false);
    assert!(job["next_run"].as_str().unwrap().ends_with('Z'));
    assert_eq!(read_jobs()["jobs"][0]["paused"], false);
    let reply = post(&dashboard, "/api/webhooks/gh/pause", &page, "").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let route: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(
        (&route["name"], &route["paused"], &route["has_secret"]),
        (&json!("gh"), &json!(true), &json!(true))
    );

    // Paused for the home folder: not resumed here.
    let mut jobs = read_jobs();
    jobs["jobs"][0]["paused"] = json!(true);
    jobs["jobs"][0]["paused_for_home"] = json!(true);
    std::fs::write(&jobs_path, jobs.to_string()).unwrap();
    let reply = post(&dashboard, &format!("/api/cron/{id}/resume"), &page, "").await;
    assert_eq!((reply.status, reply.error()), (409, json!("conflict")));
    let message: Value = serde_json::from_str(&reply.body).unwrap();
    assert!(
        message["message"]
            .as_str()
            .unwrap()
            .contains(&format!("junees cron allow-home {id}")),
        "{message}"
    );
    assert_eq!(read_jobs()["jobs"][0]["paused"], true);
    // A pause of what is paused changes nothing: the home folder's mark,
    // and so the 409, stay.
    let reply = post(&dashboard, &pause, &page, "").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let job: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!((&job["paused"], &job["paused_for_home"]), (&json!(true), &json!(true)));
    let reply = post(&dashboard, &format!("/api/cron/{id}/resume"), &page, "").await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    let routes_path = home.state.join("webhooks/routes.json");
    let mut routes: Value =
        serde_json::from_str(&std::fs::read_to_string(&routes_path).unwrap()).unwrap();
    routes["routes"][0]["paused_for_home"] = json!(true);
    std::fs::write(&routes_path, routes.to_string()).unwrap();
    let reply = post(&dashboard, "/api/webhooks/gh/resume", &page, "").await;
    assert_eq!((reply.status, reply.error()), (409, json!("conflict")));
    assert!(
        reply.body.contains("junees webhook allow-home gh"),
        "{}",
        reply.body
    );

    for (path, status, code) in [
        ("/api/cron/ffffffff/pause", 404, "not_found"),
        ("/api/webhooks/nope/resume", 404, "not_found"),
        ("/api/cron/a.b/pause", 400, "bad_id"),
    ] {
        let reply = post(&dashboard, path, &page, "").await;
        assert_eq!(
            (reply.status, reply.error()),
            (status, json!(code)),
            "{path}"
        );
    }

    // One line per write that reached its target, and no token.
    let log = std::fs::read_to_string(home.state.join("dashboard.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 9, "{log}");
    assert!(lines[0].ends_with(&format!(" pause cron {id} ok")), "{log}");
    assert!(lines[5].ends_with(&format!(" pause cron {id} ok")), "{log}");
    assert!(
        lines[6].ends_with(&format!(" resume cron {id} 409 conflict")),
        "{log}"
    );
    assert!(
        lines[1].ends_with(&format!(" resume cron {id} ok")),
        "{log}"
    );
    assert!(lines[2].ends_with(" pause webhook gh ok"), "{log}");
    assert!(
        lines[3].ends_with(&format!(" resume cron {id} 409 conflict")),
        "{log}"
    );
    assert!(
        lines[4].ends_with(" resume webhook gh 409 conflict"),
        "{log}"
    );
    assert!(
        lines[7].ends_with(" pause cron ffffffff 404 not_found"),
        "{log}"
    );
    assert!(lines[0].split(' ').next().unwrap().ends_with('Z'), "{log}");
    assert!(!log.contains(&dashboard.token));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(home.state.join("dashboard.log"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
