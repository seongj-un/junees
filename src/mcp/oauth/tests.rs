//! The whole login and the saved login in use, against a fake
//! authorization server and MCP server on loopback (one listener for
//! both): metadata, registration, the authorization step a browser would
//! do, the token endpoint with PKCE checks, refresh, and the 401
//! challenge.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::Url;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use super::login::{LoginUi, login};
use super::{Auth, LoginState, store};
use crate::mcp::config::ServerConfig;
use crate::mcp::{EnvLookup, Mcp, Server, Status, Timeouts};
use crate::tools::ToolOutput;

/// One request the fake read.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

impl Seen {
    fn form(&self) -> Vec<(String, String)> {
        let mut url = Url::parse("http://form.invalid/").unwrap();
        url.set_query(Some(&self.body));
        url.query_pairs().into_owned().collect()
    }

    fn field(&self, name: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }
}

/// An authorization code the fake handed out.
struct Grant {
    code: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    resource: String,
    scope: Option<String>,
}

/// What the fake does and knows.
pub(crate) struct Knobs {
    pkce: bool,
    registration: bool,
    iss_supported: bool,
    /// `iss` the browser step sends instead of the issuer.
    wrong_iss: Option<String>,
    expires_in: u64,
    refresh: bool,
    refresh_fails: bool,
    /// tools/call answers 403 insufficient_scope.
    forbid: bool,
    /// The `resource` of the protected resource metadata, if not the
    /// server's address.
    resource: Option<String>,
    clients: Vec<(String, Vec<String>)>,
    grants: Vec<Grant>,
    access: Vec<String>,
    refresh_tokens: Vec<String>,
    issued: u32,
    seen: Vec<Seen>,
}

impl Knobs {
    /// An authorization server that does not announce PKCE.
    pub(crate) fn without_pkce() -> Self {
        Self {
            pkce: false,
            ..Self::default()
        }
    }

    /// An authorization server without client registration.
    pub(crate) fn without_registration() -> Self {
        Self {
            registration: false,
            ..Self::default()
        }
    }
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            pkce: true,
            registration: true,
            iss_supported: true,
            wrong_iss: None,
            expires_in: 3600,
            refresh: true,
            refresh_fails: false,
            forbid: false,
            resource: None,
            clients: Vec::new(),
            grants: Vec::new(),
            access: Vec::new(),
            refresh_tokens: Vec::new(),
            issued: 0,
            seen: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Fake {
    addr: SocketAddr,
    knobs: Arc<Mutex<Knobs>>,
}

impl Fake {
    pub(crate) async fn start(knobs: Knobs) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Self {
            addr: listener.local_addr().unwrap(),
            knobs: Arc::new(Mutex::new(knobs)),
        };
        let server = fake.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move { server.serve(stream).await });
            }
        });
        fake
    }

    fn issuer(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    fn seen(&self) -> Vec<Seen> {
        self.knobs.lock().unwrap().seen.clone()
    }

    pub(crate) fn paths(&self) -> Vec<String> {
        self.seen()
            .iter()
            .map(|seen| format!("{} {}", seen.method, seen.path))
            .collect()
    }

    fn knobs(&self) -> std::sync::MutexGuard<'_, Knobs> {
        self.knobs.lock().unwrap()
    }

    /// What a browser does with the authorization address: the fake checks
    /// it, makes a code, and gives the address the browser is sent back
    /// to.
    fn authorize(&self, address: &str) -> String {
        let url = Url::parse(address).unwrap();
        assert_eq!(
            format!("{}{}", url.origin().ascii_serialization(), url.path()),
            format!("{}/authorize", self.issuer())
        );
        let get = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        assert_eq!(get("response_type").as_deref(), Some("code"));
        assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
        let client_id = get("client_id").unwrap();
        let redirect_uri = get("redirect_uri").unwrap();
        let mut knobs = self.knobs();
        if knobs.registration {
            let registered = knobs
                .clients
                .iter()
                .find(|(id, _)| *id == client_id)
                .expect("a registered client");
            assert!(registered.1.contains(&redirect_uri), "{redirect_uri}");
        }
        let code = format!("code-{}", knobs.grants.len() + 1);
        knobs.grants.push(Grant {
            code: code.clone(),
            client_id,
            redirect_uri: redirect_uri.clone(),
            challenge: get("code_challenge").unwrap(),
            resource: get("resource").unwrap(),
            scope: get("scope"),
        });
        let iss = knobs.wrong_iss.clone().unwrap_or_else(|| self.issuer());
        let mut back = Url::parse(&redirect_uri).unwrap();
        back.query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &get("state").unwrap())
            .append_pair("iss", &iss);
        back.to_string()
    }

    fn grant_scope(&self) -> Option<String> {
        self.knobs()
            .grants
            .last()
            .and_then(|grant| grant.scope.clone())
    }

    fn revoke_access(&self) {
        self.knobs().access.clear();
    }

    async fn serve(&self, mut stream: TcpStream) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        let (head, body) = loop {
            let Ok(n) = stream.read(&mut chunk).await else {
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
                let body = String::from_utf8_lossy(&buffer[end + 4..end + 4 + length]).to_string();
                break (head, body);
            }
        };
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or_default().split(' ');
        let method = first.next().unwrap_or_default().to_string();
        let path = first.next().unwrap_or_default().to_string();
        let authorization = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.trim().to_string());
        let seen = Seen {
            method: method.clone(),
            path: path.clone(),
            authorization: authorization.clone(),
            body: body.clone(),
        };
        self.knobs().seen.push(seen.clone());
        let (status, headers, reply) = self.answer(&seen);
        let mut response = format!("HTTP/1.1 {status} Fake\r\nConnection: close\r\n");
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
            reply.len()
        ));
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    fn answer(&self, seen: &Seen) -> (u16, Vec<(String, String)>, String) {
        let issuer = self.issuer();
        let mut knobs = self.knobs();
        match (seen.method.as_str(), seen.path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource/mcp") => (
                200,
                Vec::new(),
                json!({"resource": knobs.resource.clone().unwrap_or_else(|| self.url()),
                    "authorization_servers": [issuer], "scopes_supported": ["files:read"]})
                .to_string(),
            ),
            ("GET", "/.well-known/oauth-authorization-server") => {
                let mut metadata = json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "response_types_supported": ["code"],
                    "scopes_supported": ["files:read", "files:write", "offline_access"],
                    "authorization_response_iss_parameter_supported": knobs.iss_supported,
                });
                if knobs.pkce {
                    metadata["code_challenge_methods_supported"] = json!(["S256"]);
                }
                if knobs.registration {
                    metadata["registration_endpoint"] = json!(format!("{issuer}/register"));
                }
                (200, Vec::new(), metadata.to_string())
            }
            ("POST", "/register") => {
                let request: Value = serde_json::from_str(&seen.body).unwrap();
                assert_eq!(request["application_type"], "native");
                assert_eq!(request["token_endpoint_auth_method"], "none");
                let id = format!("client-{}", knobs.clients.len() + 1);
                let uris = request["redirect_uris"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|uri| uri.as_str().unwrap().to_string())
                    .collect();
                knobs.clients.push((id.clone(), uris));
                (201, Vec::new(), json!({"client_id": id}).to_string())
            }
            ("POST", "/token") => Self::token(&mut knobs, seen),
            ("POST", "/mcp") => {
                let token = seen
                    .authorization
                    .as_deref()
                    .and_then(|value| value.strip_prefix("Bearer "));
                if !token.is_some_and(|token| knobs.access.iter().any(|known| known == token)) {
                    return (
                        401,
                        vec![(
                            "WWW-Authenticate".into(),
                            format!(
                                "Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource/mcp\", scope=\"files:read\""
                            ),
                        )],
                        json!({"error": "invalid_token"}).to_string(),
                    );
                }
                let request: Value = serde_json::from_str(&seen.body).unwrap();
                let id = request["id"].clone();
                let result = match request["method"].as_str().unwrap_or_default() {
                    "server/discover" => json!({"resultType": "complete",
                        "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}),
                    "tools/list" => json!({"tools": [{"name": "whoami",
                        "description": "says hello", "inputSchema": {"type": "object"}}]}),
                    "tools/call" if knobs.forbid => {
                        return (
                            403,
                            vec![(
                                "WWW-Authenticate".into(),
                                "Bearer error=\"insufficient_scope\", scope=\"files:write\"".into(),
                            )],
                            String::new(),
                        );
                    }
                    "tools/call" => json!({"content": [{"type": "text", "text": "hello"}]}),
                    _ => json!({}),
                };
                (
                    200,
                    Vec::new(),
                    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
                )
            }
            _ => (404, Vec::new(), "{}".into()),
        }
    }

    fn token(knobs: &mut Knobs, seen: &Seen) -> (u16, Vec<(String, String)>, String) {
        let refused = |error: &str| (400, Vec::new(), json!({"error": error}).to_string());
        let resource = seen.field("resource");
        match seen.field("grant_type").as_deref() {
            Some("authorization_code") => {
                let code = seen.field("code").unwrap_or_default();
                let Some(at) = knobs.grants.iter().position(|grant| grant.code == code) else {
                    return refused("invalid_grant");
                };
                let grant = knobs.grants.remove(at);
                let verifier = seen.field("code_verifier").unwrap_or_default();
                // The client in the form, or in HTTP Basic.
                let basic = seen
                    .authorization
                    .as_deref()
                    .and_then(|value| value.strip_prefix("Basic "))
                    .and_then(|value| {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.decode(value).ok()
                    })
                    .and_then(|pair| {
                        String::from_utf8(pair)
                            .ok()?
                            .split_once(':')
                            .map(|(id, _)| id.to_string())
                    });
                let client_id = seen.field("client_id").or(basic);
                if super::pkce::challenge(&verifier) != grant.challenge
                    || client_id.as_deref() != Some(grant.client_id.as_str())
                    || seen.field("redirect_uri").as_deref() != Some(grant.redirect_uri.as_str())
                    || resource.as_deref() != Some(grant.resource.as_str())
                {
                    return refused("invalid_grant");
                }
                knobs.grants.push(Grant {
                    code: String::new(),
                    ..grant
                });
            }
            Some("refresh_token") => {
                let token = seen.field("refresh_token").unwrap_or_default();
                if knobs.refresh_fails
                    || !knobs.refresh_tokens.contains(&token)
                    || resource.is_none()
                {
                    return refused("invalid_grant");
                }
            }
            _ => return refused("unsupported_grant_type"),
        }
        knobs.issued += 1;
        let access = format!("at-{}-s3cret", knobs.issued);
        knobs.access.push(access.clone());
        let mut answer = json!({"access_token": access, "token_type": "Bearer",
            "expires_in": knobs.expires_in, "scope": "files:read"});
        if knobs.refresh {
            let refresh = format!("rt-{}-s3cret", knobs.issued);
            knobs.refresh_tokens.push(refresh.clone());
            answer["refresh_token"] = json!(refresh);
        }
        (200, Vec::new(), answer.to_string())
    }
}

/// The login's terminal: what it said, the authorization address, and
/// lines to "paste".
struct Ui {
    said: Vec<String>,
    address: Option<oneshot::Sender<String>>,
    lines: mpsc::UnboundedReceiver<String>,
}

#[async_trait::async_trait(?Send)]
impl LoginUi for Ui {
    fn say(&mut self, text: &str) {
        self.said.push(text.to_string());
        if let Some(address) = text
            .split_whitespace()
            .find(|word| word.contains("/authorize?"))
            && let Some(sender) = self.address.take()
        {
            let _ = sender.send(address.to_string());
        }
    }

    async fn line(&mut self) -> Option<String> {
        self.lines.recv().await
    }
}

fn new_ui() -> (Ui, oneshot::Receiver<String>, mpsc::UnboundedSender<String>) {
    let (address, address_rx) = oneshot::channel();
    let (paste, lines) = mpsc::unbounded_channel();
    (
        Ui {
            said: Vec::new(),
            address: Some(address),
            lines,
        },
        address_rx,
        paste,
    )
}

fn server_config(json: Value) -> ServerConfig {
    crate::mcp::config::parse(&json!({"servers": {"docs": json}}).to_string())
        .unwrap()
        .remove(0)
}

fn no_env(_: &str) -> Option<String> {
    None
}

const WAIT: Duration = Duration::from_secs(20);

/// Logs in through the loopback listener, as a browser on this computer
/// would come back.
async fn login_by_browser(
    home: &Path,
    fake: &Fake,
    config: &ServerConfig,
) -> (Result<String, String>, Ui) {
    let (mut ui, address, _paste) = new_ui();
    let browser = async {
        let Ok(address) = address.await else {
            return;
        };
        let back = fake.authorize(&address);
        let page = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(back)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
    };
    let (result, ()) = tokio::join!(login(home, config, &no_env, &mut ui, WAIT), browser);
    (result, ui)
}

fn start_server(config: ServerConfig, home: &Path) -> Server {
    let env: EnvLookup = Arc::new(|_: &str| None);
    Server::new(
        config,
        Vec::new(),
        std::path::PathBuf::from("/"),
        Timeouts::default(),
    )
    .with_env(env)
    .with_oauth_home(Some(home.to_path_buf()))
}

fn text(output: &ToolOutput) -> String {
    output.content.clone()
}

/// How many refresh requests the token endpoint got.
fn refreshes(fake: &Fake) -> usize {
    fake.seen()
        .iter()
        .filter(|seen| seen.field("grant_type").as_deref() == Some("refresh_token"))
        .count()
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn a_login_by_browser_saves_tokens_that_only_its_server_gets() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let (result, ui) = login_by_browser(home.path(), &fake, &config).await;
    let summary = result.unwrap();
    assert!(summary.contains("Logged in to `docs`"), "{summary}");
    assert!(
        summary.contains("valid for 1 h; junees renews it by itself"),
        "{summary}"
    );

    // Discovery, registration, then the token request; the probe came first.
    assert_eq!(
        fake.paths(),
        [
            "POST /mcp",
            "GET /.well-known/oauth-protected-resource/mcp",
            "GET /.well-known/oauth-authorization-server",
            "POST /register",
            "POST /token",
        ]
    );
    // The probe carried no token.
    assert_eq!(fake.seen()[0].authorization, None);
    let token_request = &fake.seen()[4];
    assert_eq!(token_request.field("resource"), Some(fake.url()));
    assert_eq!(
        token_request.field("client_id").as_deref(),
        Some("client-1")
    );
    assert!(token_request.field("code_verifier").unwrap().len() >= 43);
    // The challenge's scope, with offline_access the server lists.
    assert_eq!(
        fake.grant_scope().as_deref(),
        Some("files:read offline_access")
    );

    let path = store::path(home.path(), "docs");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&store::folder(home.path())), 0o700);
    let saved = store::read(&path).unwrap().unwrap();
    assert_eq!(saved.access_token, "at-1-s3cret");
    assert_eq!(saved.refresh_token.as_deref(), Some("rt-1-s3cret"));
    assert_eq!(saved.resource, fake.url());
    assert_eq!(saved.issuer, fake.issuer());

    // The server starts with it, and its tool runs.
    let server = start_server(config.clone(), home.path());
    server.start().await.unwrap();
    let output = server.call("whoami", json!({})).await;
    assert!(!output.is_error, "{}", text(&output));
    let to_server: Vec<Seen> = fake.seen().into_iter().skip(5).collect();
    assert!(to_server.len() >= 3);
    for seen in &to_server {
        assert_eq!(seen.path, "/mcp");
        assert_eq!(seen.authorization.as_deref(), Some("Bearer at-1-s3cret"));
    }
    // Nothing but the server's own address ever got the token.
    for seen in fake.seen() {
        if seen.path != "/mcp" {
            assert!(seen.authorization.is_none(), "{seen:?}");
            assert!(!seen.body.contains("at-1-s3cret"), "{seen:?}");
        }
    }
    // Neither the conversation nor `/mcp` shows a token.
    let shown = format!(
        "{}{}{}",
        ui.said.join("\n"),
        summary,
        super::overview(home.path(), &config).unwrap()
    );
    assert!(shown.contains("OAuth login saved"), "{shown}");
    for secret in ["at-1-s3cret", "rt-1-s3cret", "code-1"] {
        assert!(!shown.contains(secret), "{secret} in {shown}");
    }
}

#[tokio::test]
async fn a_pasted_address_finishes_the_login_and_strays_are_refused() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let (mut ui, address, paste) = new_ui();
    let browser = async {
        let address = address.await.unwrap();
        let back = fake.authorize(&address);
        paste.send("".into()).unwrap();
        paste.send("not an address".into()).unwrap();
        let stray = back.replace("state=", "state=x");
        paste.send(stray).unwrap();
        paste.send(back).unwrap();
    };
    let (result, ()) = tokio::join!(login(home.path(), &config, &no_env, &mut ui, WAIT), browser);
    let summary = result.unwrap();
    assert!(summary.contains("Logged in"), "{summary}");
    let said = ui.said.join("\n");
    assert!(said.contains("paste the whole address"), "{said}");
    assert!(said.contains("belongs to another login"), "{said}");
    assert!(said.contains("http://127.0.0.1:"), "{said}");
    assert!(store::path(home.path(), "docs").exists());
}

#[tokio::test]
async fn an_answer_from_another_issuer_never_reaches_the_token_endpoint() {
    let fake = Fake::start(Knobs {
        wrong_iss: Some("http://127.0.0.1:1".into()),
        ..Knobs::default()
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let (result, _) = login_by_browser(home.path(), &fake, &config).await;
    let message = result.unwrap_err();
    assert!(
        message.contains("another authorization server"),
        "{message}"
    );
    assert!(!fake.paths().contains(&"POST /token".to_string()));
    assert!(!store::path(home.path(), "docs").exists());
}

#[tokio::test]
async fn metadata_for_another_resource_stops_the_login() {
    let home = tempfile::tempdir().unwrap();
    for resource in ["http://127.0.0.1:1/mcp", "{}/mcp/other", "{}/mc"] {
        let fake = Fake::start(Knobs::default()).await;
        fake.knobs().resource = Some(resource.replace("{}", &fake.issuer()));
        let config = server_config(json!({"url": fake.url()}));
        let (mut ui, _address, _paste) = new_ui();
        let message = login(home.path(), &config, &no_env, &mut ui, WAIT)
            .await
            .unwrap_err();
        assert!(message.contains("for another resource"), "{message}");
        assert_eq!(
            fake.paths(),
            ["POST /mcp", "GET /.well-known/oauth-protected-resource/mcp"]
        );
    }
}

#[tokio::test]
async fn without_pkce_or_a_client_the_login_does_not_start() {
    let fake = Fake::start(Knobs {
        pkce: false,
        ..Knobs::default()
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let (mut ui, _address, _paste) = new_ui();
    let message = login(home.path(), &config, &no_env, &mut ui, WAIT)
        .await
        .unwrap_err();
    assert!(message.contains("PKCE"), "{message}");
    assert!(!fake.paths().contains(&"POST /register".to_string()));

    let fake = Fake::start(Knobs {
        registration: false,
        ..Knobs::default()
    })
    .await;
    let config = server_config(json!({"url": fake.url()}));
    let message = login(home.path(), &config, &no_env, &mut ui, WAIT)
        .await
        .unwrap_err();
    assert!(message.contains("`oauth.client_id`"), "{message}");

    // A pre-registered client needs no registration; its secret comes
    // from the environment and is sent in HTTP Basic.
    let config = server_config(json!({"url": fake.url(),
        "oauth": {"client_id": "mine", "client_secret": "env:DOCS_SECRET"}}));
    let (mut ui, address, _paste) = new_ui();
    let env = |name: &str| (name == "DOCS_SECRET").then(|| "pw-s3cret".to_string());
    let browser = async {
        let back = fake.authorize(&address.await.unwrap());
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(back)
            .send()
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(login(home.path(), &config, &env, &mut ui, WAIT), browser);
    result.unwrap();
    let token = fake
        .seen()
        .into_iter()
        .find(|seen| seen.path == "/token")
        .unwrap();
    assert!(
        token
            .authorization
            .as_deref()
            .unwrap()
            .starts_with("Basic ")
    );
    assert_eq!(token.field("client_id"), None);
}

#[tokio::test]
async fn tokens_are_renewed_before_they_expire_and_after_a_401_once() {
    let fake = Fake::start(Knobs {
        // Less than the minute junees renews ahead.
        expires_in: 30,
        ..Knobs::default()
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    fake.knobs().expires_in = 3600;

    let server = start_server(config.clone(), home.path());
    server.start().await.unwrap();
    // Renewed before the first request went out.
    assert_eq!(refreshes(&fake), 1);
    let saved = store::read(&store::path(home.path(), "docs"))
        .unwrap()
        .unwrap();
    assert_eq!(saved.access_token, "at-2-s3cret");
    assert_eq!(saved.refresh_token.as_deref(), Some("rt-2-s3cret"));
    let last = fake.seen().last().unwrap().clone();
    assert_eq!(last.authorization.as_deref(), Some("Bearer at-2-s3cret"));

    // The server forgets the token: two calls at once renew it once.
    fake.revoke_access();
    let (one, two) = tokio::join!(
        server.call("whoami", json!({})),
        server.call("whoami", json!({}))
    );
    assert!(
        !one.is_error && !two.is_error,
        "{} {}",
        text(&one),
        text(&two)
    );
    assert_eq!(refreshes(&fake), 2);
    assert_eq!(
        store::read(&store::path(home.path(), "docs"))
            .unwrap()
            .unwrap()
            .access_token,
        "at-3-s3cret"
    );

    // When the renewal is refused, the call fails with what to do, once.
    fake.revoke_access();
    fake.knobs().refresh_fails = true;
    let before = fake.seen().len();
    let output = server.call("whoami", json!({})).await;
    assert!(output.is_error);
    let message = text(&output);
    assert!(message.contains("junees mcp login docs"), "{message}");
    assert!(message.contains("invalid_grant"), "{message}");
    assert!(!message.contains("s3cret"), "{message}");
    let after: Vec<String> = fake.seen()[before..]
        .iter()
        .map(|seen| format!("{} {}", seen.method, seen.path))
        .collect();
    assert_eq!(after, ["POST /mcp", "POST /token"]);
}

#[tokio::test]
async fn a_scope_challenge_is_remembered_for_the_next_login() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    let server = start_server(config.clone(), home.path());
    server.start().await.unwrap();
    fake.knobs().forbid = true;
    let output = server.call("whoami", json!({})).await;
    let message = text(&output);
    assert!(output.is_error);
    assert!(
        message.contains("insufficient scope `files:write`"),
        "{message}"
    );
    assert!(message.contains("junees mcp login docs"), "{message}");
    let saved = store::read(&store::path(home.path(), "docs"))
        .unwrap()
        .unwrap();
    assert_eq!(saved.wanted_scope.as_deref(), Some("files:write"));
    // Logging in again asks for the old and the new scopes.
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    assert_eq!(
        fake.grant_scope().as_deref(),
        Some("files:read files:write offline_access")
    );
    let saved = store::read(&store::path(home.path(), "docs"))
        .unwrap()
        .unwrap();
    assert_eq!(saved.wanted_scope, None);
}

#[tokio::test]
async fn unattended_starts_use_a_saved_login_only() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let env: EnvLookup = Arc::new(|_: &str| None);
    // No login: that server fails with the hint, and nothing more is
    // asked of the authorization server.
    let mcp = Mcp::start_in(
        vec![config.clone()],
        &[],
        Path::new("/"),
        Timeouts::default(),
        Arc::clone(&env),
        Some(home.path()),
    )
    .await;
    match mcp.servers()[0].status() {
        Status::Failed(reason) => {
            assert!(
                reason.contains("it asks for an OAuth login (HTTP 401)"),
                "{reason}"
            );
            assert!(reason.contains("junees mcp login docs"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fake.paths(), ["POST /mcp"]);

    // With a saved login it starts.
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    let mcp = Mcp::start_in(
        vec![config.clone()],
        &[],
        Path::new("/"),
        Timeouts::default(),
        env,
        Some(home.path()),
    )
    .await;
    assert!(
        matches!(mcp.servers()[0].status(), Status::Running { .. }),
        "{:?}",
        mcp.servers()[0].status()
    );
    let overview = mcp.overview();
    assert!(overview.contains("OAuth login saved"), "{overview}");
    assert!(!overview.contains("s3cret"), "{overview}");
}

#[tokio::test]
async fn a_login_for_another_address_or_an_own_header_is_not_used() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();

    // The url changed: the token is not sent there.
    let moved = server_config(json!({"url": format!("{}/other", fake.url())}));
    let before = fake.seen().len();
    let server = start_server(moved.clone(), home.path());
    let message = server.start().await.unwrap_err();
    assert!(message.contains("for another address"), "{message}");
    assert_eq!(fake.seen().len(), before);
    assert!(matches!(
        super::state(home.path(), &moved),
        LoginState::Unusable(_)
    ));

    // An Authorization header in mcp.json wins: no login, no saved token.
    let own = server_config(json!({"url": fake.url(),
        "headers": {"Authorization": "Bearer env:DOCS_TOKEN"}}));
    assert!(Auth::load(home.path(), &own).unwrap().is_none());
    assert!(matches!(
        super::state(home.path(), &own),
        LoginState::NotUsed
    ));
    let (mut ui, _address, _paste) = new_ui();
    let message = login(home.path(), &own, &no_env, &mut ui, WAIT)
        .await
        .unwrap_err();
    assert!(message.contains("wins over an OAuth login"), "{message}");
    let env: EnvLookup = Arc::new(|name: &str| (name == "DOCS_TOKEN").then(|| "env-tok".into()));
    let server = Server::new(own, Vec::new(), "/".into(), Timeouts::default())
        .with_env(env)
        .with_oauth_home(Some(home.path().to_path_buf()));
    let message = server.start().await.unwrap_err();
    assert!(
        message.contains("refused the `Authorization` header"),
        "{message}"
    );
    assert_eq!(
        fake.seen().last().unwrap().authorization.as_deref(),
        Some("Bearer env-tok")
    );

    // A command server has no login.
    let stdio = server_config(json!({"command": "true"}));
    assert!(matches!(
        super::state(home.path(), &stdio),
        LoginState::NotUsed
    ));
}

#[tokio::test]
async fn the_wait_for_the_browser_has_a_limit() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let (mut ui, _address, paste) = new_ui();
    drop(paste);
    let message = login(
        home.path(),
        &config,
        &no_env,
        &mut ui,
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert!(message.contains("no answer from the browser"), "{message}");
    assert!(!fake.paths().contains(&"POST /token".to_string()));
}

#[tokio::test]
async fn logout_removes_the_saved_login() {
    let home = tempfile::tempdir().unwrap();
    store::write(home.path(), "docs", &store::sample()).unwrap();
    let text = super::logout(home.path(), "docs").await.unwrap();
    assert!(text.contains("Logged out of `docs`"), "{text}");
    assert!(!store::path(home.path(), "docs").exists());
    assert!(
        super::logout(home.path(), "docs")
            .await
            .unwrap()
            .contains("no saved login")
    );
    assert!(super::logout(home.path(), "../x").await.is_err());
}

#[tokio::test]
async fn a_running_renewal_does_not_undo_a_logout() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    let server = start_server(config.clone(), home.path());
    server.start().await.unwrap();
    let path = store::path(home.path(), "docs");

    // `junees mcp logout docs` in another terminal; the server then
    // refuses the token the running junees still holds.
    super::logout(home.path(), "docs").await.unwrap();
    fake.revoke_access();
    let output = server.call("whoami", json!({})).await;
    assert!(output.is_error);
    let message = text(&output);
    assert!(
        message.contains("its saved login was removed (`junees mcp logout docs`)"),
        "{message}"
    );
    assert!(message.contains("junees mcp login docs"), "{message}");
    assert!(!message.contains("s3cret"), "{message}");
    assert!(!path.exists());
    assert_eq!(refreshes(&fake), 0);

    // A new login for the same address is taken up by the running junees.
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    let output = server.call("whoami", json!({})).await;
    assert!(!output.is_error, "{}", text(&output));
    assert_eq!(refreshes(&fake), 0);
    assert_eq!(
        fake.seen().last().unwrap().authorization.as_deref(),
        Some("Bearer at-2-s3cret")
    );
}

#[tokio::test]
async fn a_running_renewal_does_not_overwrite_a_login_for_another_address() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    login_by_browser(home.path(), &fake, &config)
        .await
        .0
        .unwrap();
    let server = start_server(config.clone(), home.path());
    server.start().await.unwrap();

    // mcp.json's `url` changed and `docs` was logged in again there,
    // while this junees still runs with the old address.
    let path = store::path(home.path(), "docs");
    let mut newer = store::read(&path).unwrap().unwrap();
    newer.server_url = format!("{}/other", fake.url());
    newer.resource = newer.server_url.clone();
    newer.access_token = "newer-s3cret".into();
    newer.refresh_token = Some("newer-rt-s3cret".into());
    store::write(home.path(), "docs", &newer).unwrap();
    let before = std::fs::read(&path).unwrap();

    fake.revoke_access();
    let output = server.call("whoami", json!({})).await;
    assert!(output.is_error);
    let message = text(&output);
    assert!(
        message.contains("replaced by a login for another address"),
        "{message}"
    );
    assert!(!message.contains("s3cret"), "{message}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(refreshes(&fake), 0);
}

#[tokio::test]
async fn logins_and_logouts_wait_for_a_renewal_in_progress() {
    let fake = Fake::start(Knobs::default()).await;
    let home = tempfile::tempdir().unwrap();
    let config = server_config(json!({"url": fake.url()}));
    let path = store::path(home.path(), "docs");

    // Another junees renews meanwhile: it holds the lock.
    let held = store::lock(home.path()).unwrap();
    let login = login_by_browser(home.path(), &fake, &config);
    tokio::pin!(login);
    let deadline = std::time::Instant::now() + WAIT;
    while !fake.paths().contains(&"POST /token".to_string()) {
        assert!(std::time::Instant::now() < deadline, "{:?}", fake.paths());
        tokio::select! {
            _ = &mut login => panic!("the login ended while the lock was held"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    tokio::select! {
        _ = &mut login => panic!("the login was saved while the lock was held"),
        () = tokio::time::sleep(Duration::from_millis(500)) => {}
    }
    assert!(!path.exists());
    drop(held);
    login.await.0.unwrap();
    assert!(path.exists());

    let held = store::lock(home.path()).unwrap();
    let logout = super::logout(home.path(), "docs");
    tokio::pin!(logout);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut logout)
            .await
            .is_err()
    );
    assert!(path.exists());
    drop(held);
    assert!(logout.await.unwrap().contains("Logged out of `docs`"));
    assert!(!path.exists());
}
