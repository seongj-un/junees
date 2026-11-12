//! Runs the real binary's `junees mcp login` against a fake authorization
//! server and MCP server on loopback, with the answer pasted on stdin (a
//! browser on another computer), then `junees -p` with the saved login
//! and `junees mcp logout`. No browser is opened and nothing leaves this
//! computer.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

mod common;
use common::{body_of, fake_api, text_reply};

const ACCESS: &str = "at-bin-s3cret";
const REFRESH: &str = "rt-bin-s3cret";

/// What the fake saw: method, path, `Authorization`, body.
type Seen = Arc<Mutex<Vec<(String, String, Option<String>, String)>>>;

#[derive(Clone, Default)]
struct State {
    seen: Seen,
    /// The code challenge and redirect URI of the one authorization.
    grant: Arc<Mutex<Option<(String, String)>>>,
}

async fn fake_server() -> (String, State) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = State::default();
    let shared = state.clone();
    let issuer = base.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let state = shared.clone();
            let issuer = issuer.clone();
            tokio::spawn(async move { serve(socket, state, issuer).await });
        }
    });
    (base, state)
}

fn form(body: &str) -> Vec<(String, String)> {
    let mut url = Url::parse("http://form.invalid/").unwrap();
    url.set_query(Some(body));
    url.query_pairs().into_owned().collect()
}

fn field(body: &str, name: &str) -> Option<String> {
    form(body)
        .into_iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

async fn serve(mut socket: TcpStream, state: State, issuer: String) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let (head, body) = loop {
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
        let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_string();
        let length = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
        if buffer.len() >= end + 4 + length {
            break (
                head,
                String::from_utf8_lossy(&buffer[end + 4..end + 4 + length]).to_string(),
            );
        }
    };
    let mut first = head.lines().next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let authorization = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.trim().to_string());
    state.seen.lock().unwrap().push((
        method.clone(),
        path.clone(),
        authorization.clone(),
        body.clone(),
    ));
    let (status, extra, reply) = match (method.as_str(), path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => (
            200,
            String::new(),
            json!({"resource": format!("{issuer}/mcp"), "authorization_servers": [issuer]})
                .to_string(),
        ),
        ("GET", "/.well-known/oauth-authorization-server") => (
            200,
            String::new(),
            json!({"issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "registration_endpoint": format!("{issuer}/register"),
                "code_challenge_methods_supported": ["S256"],
                "authorization_response_iss_parameter_supported": true})
            .to_string(),
        ),
        ("POST", "/register") => (
            201,
            String::new(),
            json!({"client_id": "bin-client"}).to_string(),
        ),
        ("POST", "/token") => {
            let grant = state.grant.lock().unwrap().take();
            let verifier = field(&body, "code_verifier").unwrap_or_default();
            let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()),
            );
            let good = grant.is_some_and(|(expected, redirect)| {
                expected == challenge
                    && field(&body, "code").as_deref() == Some("bin-code")
                    && field(&body, "redirect_uri") == Some(redirect)
                    && field(&body, "resource") == Some(format!("{issuer}/mcp"))
            });
            if good {
                (
                    200,
                    String::new(),
                    json!({"access_token": ACCESS, "refresh_token": REFRESH,
                        "token_type": "Bearer", "expires_in": 3600})
                    .to_string(),
                )
            } else {
                (
                    400,
                    String::new(),
                    json!({"error": "invalid_grant"}).to_string(),
                )
            }
        }
        ("POST", "/mcp") if authorization.as_deref() != Some(&format!("Bearer {ACCESS}")) => (
            401,
            format!(
                "WWW-Authenticate: Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource/mcp\"\r\n"
            ),
            "{}".to_string(),
        ),
        ("POST", "/mcp") => {
            let request: Value = serde_json::from_str(&body).unwrap();
            let result = match request["method"].as_str().unwrap_or_default() {
                "server/discover" => json!({"resultType": "complete",
                    "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}),
                "tools/list" => json!({"tools": [{"name": "whoami", "description": "who",
                    "inputSchema": {"type": "object"}}]}),
                "tools/call" => json!({"content": [{"type": "text", "text": "you are logged in"}]}),
                _ => json!({}),
            };
            (
                200,
                String::new(),
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result}).to_string(),
            )
        }
        _ => (404, String::new(), "{}".to_string()),
    };
    let response = format!(
        "HTTP/1.1 {status} Fake\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// What the browser does: checks the address, keeps the challenge, and
/// returns the address the browser ends on.
fn authorize(state: &State, issuer: &str, address: &str) -> String {
    let url = Url::parse(address).unwrap();
    let get = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    };
    assert_eq!(get("client_id"), "bin-client");
    assert_eq!(get("code_challenge_method"), "S256");
    assert_eq!(get("resource"), format!("{issuer}/mcp"));
    let redirect = get("redirect_uri");
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
    *state.grant.lock().unwrap() = Some((get("code_challenge"), redirect.clone()));
    let mut back = Url::parse(&redirect).unwrap();
    back.query_pairs_mut()
        .append_pair("code", "bin-code")
        .append_pair("state", &get("state"))
        .append_pair("iss", issuer);
    back.to_string()
}

fn home_with_server(dir: &Path, url: &str) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("mcp.json");
    std::fs::write(
        &path,
        json!({"servers": {"docs": {"url": url}}}).to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    home
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn login_by_paste_then_the_saved_token_reaches_only_its_server() {
    let (issuer, state) = fake_server().await;
    let dir = tempfile::tempdir().unwrap();
    let home = home_with_server(dir.path(), &format!("{issuer}/mcp"));
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();

    let mut child = common::junees_binary(&work, &home)
        .args(["mcp", "login", "docs"])
        .env("JUNEES_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut printed = Vec::new();
    let address = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = stdout.next_line().await.unwrap() {
            printed.push(line.clone());
            if let Some(address) = line
                .split_whitespace()
                .find(|word| word.contains("/authorize?"))
            {
                return address.to_string();
            }
        }
        panic!("no address in {printed:?}");
    })
    .await
    .expect("the login printed no address");
    let back = authorize(&state, &issuer, &address);
    stdin
        .write_all(format!("{back}\n").as_bytes())
        .await
        .unwrap();
    let rest = tokio::time::timeout(Duration::from_secs(30), async {
        let mut rest = Vec::new();
        while let Some(line) = stdout.next_line().await.unwrap() {
            rest.push(line);
        }
        rest
    })
    .await
    .expect("the login did not finish");
    let status = child.wait().await.unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    let stdout_text = format!("{}\n{}", printed.join("\n"), rest.join("\n"));
    assert!(status.success(), "{stdout_text}\n{stderr}");
    assert!(stdout_text.contains("paste it here"), "{stdout_text}");
    assert!(stdout_text.contains("Logged in to `docs`"), "{stdout_text}");
    for secret in [ACCESS, REFRESH, "bin-code"] {
        assert!(!stdout_text.contains(secret), "{stdout_text}");
        assert!(!stderr.contains(secret), "{stderr}");
    }
    let saved_path = home.join("secrets/mcp-oauth/docs.json");
    assert_eq!(mode(&saved_path), 0o600);
    assert_eq!(mode(&home.join("secrets/mcp-oauth")), 0o700);
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&saved_path).unwrap()).unwrap();
    assert_eq!(saved["access_token"], ACCESS);
    assert_eq!(saved["issuer"], issuer.as_str());

    // A run with the saved login: the model calls the server's tool.
    let (url, api) = fake_api(vec![
        (
            200,
            json!({"id": "msg_1", "type": "message", "role": "assistant", "model": "m",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "mcp__docs__whoami",
                    "input": {}}],
                "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 5}}),
        ),
        (200, text_reply("done")),
    ])
    .await;
    let output = common::junees_binary(&work, &home)
        .args(["-p", "who am I?", "--no-session"])
        .env("ANTHROPIC_API_KEY", "sk-test")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("mcp: docs started (1 tool, over HTTP)"),
        "{stderr}"
    );
    let requests = tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    let result = body_of(&requests[1])["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"][0]["content"]
        .clone();
    assert_eq!(result, "you are logged in");
    for request in &requests {
        assert!(!request.contains(ACCESS), "the model saw the token");
    }
    let seen = state.seen.lock().unwrap().clone();
    for (method, path, authorization, body) in &seen {
        if path != "/mcp" {
            assert!(authorization.is_none(), "{method} {path}");
            assert!(!body.contains(ACCESS), "{method} {path}");
        }
    }
    assert!(
        seen.iter()
            .filter(|(_, path, _, _)| path == "/mcp")
            .any(|(_, _, authorization, _)| authorization.is_some())
    );

    // Logging out removes the file.
    let output = common::junees_binary(&work, &home)
        .args(["mcp", "logout", "docs"])
        .env("JUNEES_HOME", &home)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Logged out of `docs`"));
    assert!(!saved_path.exists());
}

#[tokio::test]
async fn a_run_without_a_login_never_starts_one() {
    let (issuer, state) = fake_server().await;
    let dir = tempfile::tempdir().unwrap();
    let home = home_with_server(dir.path(), &format!("{issuer}/mcp"));
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (url, api) = fake_api(vec![(200, text_reply("hi"))]).await;
    let output = common::junees_binary(&work, &home)
        .args(["-p", "hello", "--no-session"])
        .env("ANTHROPIC_API_KEY", "sk-test")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", &home)
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("mcp: docs failed to start: it asks for an OAuth login (HTTP 401); run `junees mcp login docs`"),
        "{stderr}"
    );
    tokio::time::timeout(Duration::from_secs(10), api)
        .await
        .unwrap()
        .unwrap();
    let paths: Vec<String> = state
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(method, path, _, _)| format!("{method} {path}"))
        .collect();
    assert_eq!(paths, ["POST /mcp"]);
    assert!(!home.join("secrets").exists());
}
