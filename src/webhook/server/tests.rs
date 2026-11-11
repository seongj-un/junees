use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use super::*;
use crate::cron::deliver::Delivery;
use crate::webhook::store::Secret;
use crate::webhook::store::tests::route;

const SECRET: &str = "s3cret";

/// Records each run, then waits for a permit of `gate`, then ends `ok`
/// unless `fail` is set.
struct FakeRunner {
    runs: Mutex<Vec<(String, Incoming)>>,
    gate: Semaphore,
    interrupted: AtomicBool,
    fail: AtomicBool,
    finished: AtomicUsize,
}

#[async_trait]
impl Runner for FakeRunner {
    async fn run(&self, name: &str, incoming: Incoming) -> bool {
        self.runs.lock().unwrap().push((name.to_string(), incoming));
        self.gate.acquire().await.unwrap().forget();
        let ok = !self.fail.load(Ordering::SeqCst);
        self.finished.fetch_add(1, Ordering::SeqCst);
        ok
    }

    fn interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        self.gate.add_permits(1000);
    }
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            runs: Mutex::default(),
            gate: Semaphore::new(0),
            interrupted: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            finished: AtomicUsize::new(0),
        }
    }
}

impl FakeRunner {
    fn started(&self) -> usize {
        self.runs.lock().unwrap().len()
    }

    async fn wait_started(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.started() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} run(s) started",
                self.started()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_finished(&self, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.finished.load(Ordering::SeqCst) < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "runs did not finish"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct Running {
    url: String,
    runner: Arc<FakeRunner>,
    stop: Cancel,
    force: Cancel,
    served: tokio::task::JoinHandle<()>,
    _home: tempfile::TempDir,
}

/// A server on a free loopback port with routes `pr` (github, secret,
/// pull_request.opened and issue_comment), `open` (generic, no secret)
/// and `off` (paused).
async fn start(loopback: bool, read_timeout: Duration) -> Running {
    start_with(loopback, move |server| {
        server.with_read_timeout(read_timeout)
    })
    .await
}

/// [`start`] with the server changed by `configure`.
async fn start_with(loopback: bool, configure: impl FnOnce(Server) -> Server) -> Running {
    let home = tempfile::tempdir().unwrap();
    let mut pr = route("pr");
    pr.events = vec!["pull_request.opened".into(), "issue_comment".into()];
    pr.secret = Some(Secret::Env("HOOK_SECRET".into()));
    let mut open = route("open");
    open.kind = Kind::Generic;
    open.secret = None;
    let mut off = route("off");
    off.paused = true;
    off.secret = Some(Secret::Env("HOOK_SECRET".into()));
    off.deliver = Delivery::Local;
    let store = Store::new(home.path());
    let mut locked = store.lock().unwrap();
    locked.routes = vec![pr, open, off];
    locked.save().unwrap();
    drop(locked);
    let runner = Arc::new(FakeRunner::default());
    let env: Env = Arc::new(|key: &str| (key == "HOOK_SECRET").then(|| SECRET.to_string()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    let url = format!("http://{local}");
    // Bound to loopback either way; the server is told it listens wider.
    let listen = if loopback {
        local
    } else {
        SocketAddr::from(([0, 0, 0, 0], local.port()))
    };
    let server = Arc::new(configure(Server::new(
        home.path(),
        listen,
        Arc::clone(&runner) as Arc<dyn Runner>,
        env,
        Arc::new(|_: &str| {}),
    )));
    let (stop, force) = (Cancel::new(), Cancel::new());
    let served = tokio::spawn(server.serve(listener, stop.clone(), force.clone()));
    Running {
        url,
        runner,
        stop,
        force,
        served,
        _home: home,
    }
}

/// Sends `request` as written and returns the status and body.
async fn raw(url: &str, request: &[u8]) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(url.trim_start_matches("http://"))
        .await
        .unwrap();
    stream.write_all(request).await.unwrap();
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut text))
        .await
        .expect("an answer")
        .unwrap();
    let status = text[9..12].parse().unwrap();
    let body = text
        .split_once("\r\n\r\n")
        .unwrap()
        .1
        .trim_end()
        .to_string();
    (status, body)
}

/// POSTs `body` to `/hooks/<name>` with `headers` (and a length), and
/// `Host: <the server's address>` and `Content-Type: application/json`
/// unless `headers` name them.
async fn post(url: &str, name: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut request = format!("POST /hooks/{name} HTTP/1.1\r\n");
    let host = url.trim_start_matches("http://");
    for (key, value) in [("Host", host), ("Content-Type", "application/json")] {
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(key))
        {
            request.push_str(&format!("{key}: {value}\r\n"));
        }
    }
    for (key, value) in headers {
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    raw(url, request.as_bytes()).await
}

/// A signed GitHub request to `pr`.
async fn github(url: &str, event: &str, delivery: &str, body: &str) -> (u16, String) {
    let signature = signature::sign(SECRET.as_bytes(), body.as_bytes());
    post(
        url,
        "pr",
        &[
            ("X-GitHub-Event", event),
            ("X-GitHub-Delivery", delivery),
            ("X-Hub-Signature-256", &signature),
        ],
        body,
    )
    .await
}

#[tokio::test]
async fn answers_by_method_path_signature_and_event() {
    let server = start(true, READ_TIMEOUT).await;
    let url = &server.url;
    let opened = r#"{"action":"opened","number":7}"#;
    assert_eq!(
        raw(url, b"GET /hooks/pr HTTP/1.1\r\n\r\n").await.0,
        405,
        "another method"
    );
    assert_eq!(post(url, "../pr", &[], "{}").await.0, 404);
    assert_eq!(
        raw(url, b"POST /other HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await
            .0,
        404
    );
    assert_eq!(
        post(url, "nope", &[], "{}").await,
        (404, "no such route".into())
    );
    assert_eq!(
        post(url, "pr", &[("X-GitHub-Event", "pull_request")], opened).await,
        (401, "missing signature".into())
    );
    let wrong = signature::sign(b"guess", opened.as_bytes());
    assert_eq!(
        post(
            url,
            "pr",
            &[
                ("X-GitHub-Event", "pull_request"),
                ("X-Hub-Signature-256", &wrong)
            ],
            opened
        )
        .await,
        (401, "wrong signature".into())
    );
    // The generic header does not sign a GitHub route.
    let signed = signature::sign(SECRET.as_bytes(), opened.as_bytes());
    assert_eq!(
        post(
            url,
            "pr",
            &[
                ("X-GitHub-Event", "pull_request"),
                ("X-Junees-Signature", &signed)
            ],
            opened
        )
        .await
        .0,
        401
    );
    assert_eq!(
        github(url, "ping", "d0", r#"{"zen":"hi"}"#).await,
        (200, "pong".into())
    );
    assert_eq!(github(url, "pull_request", "d1", "not json").await.0, 400);
    assert_eq!(github(url, "pull`request", "d1", opened).await.0, 400);
    assert_eq!(
        github(url, "pull_request", "d1", r#"{"action":"closed"}"#)
            .await
            .0,
        202
    );
    assert_eq!(server.runner.started(), 0, "closed is not wanted");
    assert_eq!(github(url, "push", "d2", "{}").await.0, 202);
    assert_eq!(server.runner.started(), 0, "push is not wanted");
    assert_eq!(
        github(url, "pull_request", "d3", opened).await,
        (202, "queued".into())
    );
    server.runner.wait_started(1).await;
    let (name, incoming) = server.runner.runs.lock().unwrap()[0].clone();
    assert_eq!(name, "pr");
    assert_eq!(incoming.event, "pull_request");
    assert_eq!(incoming.action.as_deref(), Some("opened"));
    assert_eq!(incoming.delivery.as_deref(), Some("d3"));
    assert_eq!(incoming.payload, json!({"action": "opened", "number": 7}));
    // A signed request without the event header.
    let signature = signature::sign(SECRET.as_bytes(), opened.as_bytes());
    assert_eq!(
        post(url, "pr", &[("X-Hub-Signature-256", &signature)], opened).await,
        (400, "missing X-GitHub-Event".into())
    );
    // Without a secret, on a loopback listener: no signature needed.
    assert_eq!(
        post(url, "open", &[("X-Junees-Event", "deploy")], "[1]").await,
        (202, "queued".into())
    );
    server.runner.wait_started(2).await;
    assert_eq!(server.runner.runs.lock().unwrap()[1].1.event, "deploy");
    assert_eq!(
        post(url, "off", &[], "{}").await.0,
        401,
        "a paused route still checks the signature first"
    );
    let body = "{}";
    let signature = signature::sign(SECRET.as_bytes(), body.as_bytes());
    assert_eq!(
        post(
            url,
            "off",
            &[
                ("X-GitHub-Event", "push"),
                ("X-Hub-Signature-256", &signature)
            ],
            body
        )
        .await,
        (503, "the route is paused".into())
    );
    server.runner.gate.add_permits(10);
}

#[tokio::test]
async fn routes_without_a_secret_need_a_loopback_listener() {
    let server = start(false, READ_TIMEOUT).await;
    assert_eq!(
        post(&server.url, "open", &[], "{}").await.0,
        403,
        "a server listening beyond loopback"
    );
    assert_eq!(server.runner.started(), 0);
    // Signed routes still work.
    let opened = r#"{"action":"opened"}"#;
    assert_eq!(
        github(&server.url, "pull_request", "a", opened).await.0,
        202
    );
    server.runner.gate.add_permits(10);
}

#[tokio::test]
async fn routes_without_a_secret_refuse_browsers_and_proxies() {
    let server = start(true, READ_TIMEOUT).await;
    let url = &server.url;
    let port = url.rsplit(':').next().unwrap();
    // What a web page in the user's browser sends with
    // `fetch(url, {method: "POST", mode: "no-cors", body})`: no preflight.
    let probe = format!(
        "POST /hooks/open HTTP/1.1\r\nHost: evil.example:{port}\r\n\
         Origin: https://evil.example\r\nSec-Fetch-Site: cross-site\r\n\
         Sec-Fetch-Mode: no-cors\r\nContent-Type: text/plain;charset=UTF-8\r\n\
         Content-Length: 2\r\n\r\n{{}}"
    );
    assert_eq!(raw(url, probe.as_bytes()).await.0, 403);
    // Each mark alone, on an otherwise local JSON request.
    let local = format!("127.0.0.1:{port}");
    let other_port = format!("127.0.0.1:{}", port.parse::<u16>().unwrap() ^ 1);
    let evil = format!("evil.example:{port}");
    let cases: Vec<(Vec<(&str, &str)>, u16)> = vec![
        // DNS rebinding: the page's own name resolves to 127.0.0.1.
        (vec![("Host", evil.as_str())], 403),
        (vec![("Host", "127.0.0.1")], 403),
        (vec![("Host", &other_port)], 403),
        (vec![("Host", "127.0.0.2:1")], 403),
        (vec![("Host", &local), ("Host", &local)], 403),
        (vec![("Origin", "null")], 403),
        (vec![("Sec-Fetch-Site", "same-origin")], 403),
        (vec![("Sec-Fetch-Mode", "navigate")], 403),
        (vec![("X-Forwarded-For", "203.0.113.5")], 403),
        (vec![("Forwarded", "for=203.0.113.5")], 403),
        (vec![("CF-Connecting-IP", "203.0.113.5")], 403),
        (vec![("X-Real-IP", "203.0.113.5")], 403),
        (vec![("Content-Type", "text/plain")], 415),
        (
            vec![("Content-Type", "application/x-www-form-urlencoded")],
            415,
        ),
        (vec![("Content-Type", "application/jsonx")], 415),
        (vec![("Content-Type", "")], 415),
    ];
    for (headers, wanted) in cases {
        let (status, text) = post(url, "open", &headers, "{}").await;
        assert_eq!(status, wanted, "{headers:?}: {text}");
    }
    let no_type =
        format!("POST /hooks/open HTTP/1.1\r\nHost: {local}\r\nContent-Length: 2\r\n\r\n{{}}");
    assert_eq!(raw(url, no_type.as_bytes()).await.0, 415);
    let no_host = "POST /hooks/open HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
    assert_eq!(raw(url, no_host.as_bytes()).await.0, 403);
    assert_eq!(server.runner.started(), 0, "nothing ran");
    // What a program on this machine sends (curl, a local tool).
    let localhost = format!("LocalHost:{port}");
    let ipv6 = format!("[::1]:{port}");
    for headers in [
        vec![],
        vec![("Host", localhost.as_str())],
        vec![("Host", ipv6.as_str())],
        vec![("Content-Type", "Application/JSON; charset=utf-8")],
    ] {
        let (status, text) = post(url, "open", &headers, "{}").await;
        assert_eq!(status, 202, "{headers:?}: {text}");
    }
    // A route with a secret relies on its signature alone.
    let opened = r#"{"action":"opened"}"#;
    let signature = signature::sign(SECRET.as_bytes(), opened.as_bytes());
    let (status, text) = post(
        url,
        "pr",
        &[
            ("Host", "hooks.example.com"),
            ("X-Forwarded-For", "203.0.113.5"),
            ("Origin", "https://github.com"),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("X-GitHub-Event", "pull_request"),
            ("X-Hub-Signature-256", &signature),
        ],
        opened,
    )
    .await;
    assert_eq!((status, text.as_str()), (202, "queued"));
    server.runner.gate.add_permits(10);
    server.runner.wait_started(5).await;
}

#[tokio::test]
async fn a_repeated_delivery_runs_once() {
    let server = start(true, READ_TIMEOUT).await;
    let url = &server.url;
    let opened = r#"{"action":"opened"}"#;
    assert_eq!(github(url, "pull_request", "same", opened).await.0, 202);
    assert_eq!(
        github(url, "pull_request", "same", opened).await,
        (200, "duplicate".into())
    );
    // The same id on another route is another delivery.
    assert_eq!(
        post(url, "open", &[("X-Junees-Delivery", "same")], "{}")
            .await
            .0,
        202
    );
    assert_eq!(
        post(url, "open", &[("X-Junees-Delivery", "same")], "{}")
            .await
            .1,
        "duplicate"
    );
    server.runner.wait_started(2).await;
    server.runner.gate.add_permits(10);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(server.runner.started(), 2);

    // Only the last REMEMBERED ids are kept.
    let mut state = State::default();
    for n in 0..=REMEMBERED {
        state.remember(format!("pr/{n}"));
    }
    state.remember("pr/5".into());
    assert_eq!(state.seen.len(), REMEMBERED);
    assert!(!state.seen_set.contains("pr/0"));
    assert!(state.seen_set.contains("pr/1") && state.seen_set.contains("pr/1000"));
}

#[tokio::test]
async fn a_failed_run_may_be_delivered_again() {
    let server = start(true, READ_TIMEOUT).await;
    let url = &server.url;
    let runner = &server.runner;
    let opened = r#"{"action":"opened"}"#;
    runner.fail.store(true, Ordering::SeqCst);
    assert_eq!(github(url, "pull_request", "again", opened).await.0, 202);
    runner.wait_started(1).await;
    assert_eq!(
        github(url, "pull_request", "again", opened).await,
        (200, "duplicate".into()),
        "while it runs"
    );
    runner.gate.add_permits(1);
    runner.wait_finished(1).await;
    // GitHub's "Redeliver" sends the same delivery id. The id is
    // forgotten right after the run returns.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, text) = github(url, "pull_request", "again", opened).await;
        if status == 202 {
            assert_eq!(text, "queued");
            break;
        }
        assert_eq!((status, text.as_str()), (200, "duplicate"));
        assert!(
            tokio::time::Instant::now() < deadline,
            "the failed delivery is still remembered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    runner.wait_started(2).await;
    // This time it ends ok: the id stays remembered.
    runner.fail.store(false, Ordering::SeqCst);
    runner.gate.add_permits(1);
    runner.wait_finished(2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        github(url, "pull_request", "again", opened).await,
        (200, "duplicate".into())
    );
    assert_eq!(runner.started(), 2);

    let mut state = State::default();
    for key in ["a", "b", "c"] {
        state.remember(key.into());
    }
    state.forget("b");
    state.forget("x");
    assert_eq!(state.seen, ["a", "c"]);
    assert!(!state.seen_set.contains("b"));
}

#[tokio::test]
async fn runs_one_request_per_route_at_a_time_and_refuses_past_the_queue() {
    let server = start(true, READ_TIMEOUT).await;
    let url = &server.url;
    let opened = r#"{"action":"opened"}"#;
    assert_eq!(github(url, "pull_request", "r0", opened).await.0, 202);
    server.runner.wait_started(1).await;
    for n in 1..=MAX_WAITING {
        let (status, text) = github(url, "pull_request", &format!("r{n}"), opened).await;
        assert_eq!((status, text.as_str()), (202, "queued"), "{n}");
    }
    assert_eq!(
        github(url, "pull_request", "r-full", opened).await,
        (429, "too many requests wait for this route".into())
    );
    // A refused request was not remembered: it may come again.
    assert_eq!(github(url, "pull_request", "r-full", opened).await.0, 429);
    // Another route has its own queue and runs meanwhile.
    assert_eq!(post(url, "open", &[], "{}").await.0, 202);
    server.runner.wait_started(2).await;
    assert_eq!(server.runner.runs.lock().unwrap()[1].0, "open");
    server.runner.gate.add_permits(1);
    server.runner.wait_started(3).await;
    // The queue frees a place once a run starts.
    assert_eq!(github(url, "pull_request", "r-full", opened).await.0, 202);
    server.runner.gate.add_permits(100);
    server.runner.wait_started(MAX_WAITING + 3).await;
    let order: Vec<String> = server
        .runner
        .runs
        .lock()
        .unwrap()
        .iter()
        .filter(|(name, _)| name == "pr")
        .map(|(_, incoming)| incoming.delivery.clone().unwrap())
        .collect();
    let mut wanted: Vec<String> = (0..=MAX_WAITING).map(|n| format!("r{n}")).collect();
    wanted.push("r-full".into());
    assert_eq!(order, wanted, "in the order received");
}

#[tokio::test]
async fn stopping_drops_the_waiting_and_waits_for_the_running() {
    let server = start(true, READ_TIMEOUT).await;
    let url = server.url.clone();
    let opened = r#"{"action":"opened"}"#;
    for n in 0..3 {
        assert_eq!(
            github(&url, "pull_request", &format!("s{n}"), opened)
                .await
                .0,
            202
        );
    }
    server.runner.wait_started(1).await;
    server.stop.cancel();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!server.served.is_finished(), "a run is still going");
    assert!(
        tokio::net::TcpStream::connect(url.trim_start_matches("http://"))
            .await
            .is_err(),
        "no new connections"
    );
    server.runner.gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), server.served)
        .await
        .expect("the server stops once the run ends")
        .unwrap();
    assert_eq!(
        server.runner.started(),
        1,
        "the waiting requests were dropped"
    );
    assert!(!server.runner.interrupted.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_second_signal_interrupts_the_runs() {
    let server = start(true, READ_TIMEOUT).await;
    assert_eq!(post(&server.url, "open", &[], "{}").await.0, 202);
    server.runner.wait_started(1).await;
    server.stop.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.force.cancel();
    tokio::time::timeout(Duration::from_secs(10), server.served)
        .await
        .expect("the server stops")
        .unwrap();
    assert!(server.runner.interrupted.load(Ordering::SeqCst));
}

#[tokio::test]
async fn refuses_before_asking_for_the_body() {
    // A short read limit: a server waiting for the body answers 408.
    let server = start(true, Duration::from_secs(3)).await;
    let url = &server.url;
    for (path, length, wanted) in [
        ("/hooks/nope", 1000, 404),
        ("/hooks/open", http::MAX_BODY + 1, 413),
        ("/other", 1000, 404),
    ] {
        let request = format!(
            "POST {path} HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: {length}\r\n\r\n"
        );
        // Without `100 Continue` and without waiting for a body.
        assert_eq!(raw(url, request.as_bytes()).await.0, wanted, "{path}");
    }
    // A route that exists still gets `100 Continue`, then its answer.
    let mut stream = tokio::net::TcpStream::connect(url.trim_start_matches("http://"))
        .await
        .unwrap();
    stream
        .write_all(b"POST /hooks/pr HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n")
        .await
        .unwrap();
    let mut reply = [0u8; 25];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"HTTP/1.1 100 Continue\r\n\r\n");
    stream.write_all(b"{}").await.unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).await.unwrap();
    assert!(text.starts_with("HTTP/1.1 401 "), "{text}");
    assert_eq!(server.runner.started(), 0);
}

#[tokio::test]
async fn refuses_connections_past_the_limit() {
    let server = start_with(true, |server| server.with_max_connections(2)).await;
    let url = &server.url;
    let addr = url.trim_start_matches("http://");
    // Two clients that connect and send nothing hold both places.
    let first = tokio::net::TcpStream::connect(addr).await.unwrap();
    let second = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut third = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(3), third.read_to_string(&mut text))
        .await
        .expect("an answer without waiting for the others")
        .unwrap();
    assert!(text.starts_with("HTTP/1.1 503 "), "{text}");
    assert!(
        text.ends_with("too many connections; try again later\n"),
        "{text}"
    );
    // A connection that ends frees its place.
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, text) = post(url, "open", &[], "{}").await;
        if status == 202 {
            break;
        }
        assert_eq!(status, 503, "{text}");
        assert!(tokio::time::Instant::now() < deadline, "no place was freed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(second);
    server.runner.wait_started(1).await;
    server.runner.gate.add_permits(10);
}

#[tokio::test]
async fn a_slow_client_gets_408() {
    let server = start(true, Duration::from_millis(300)).await;
    let mut stream = tokio::net::TcpStream::connect(server.url.trim_start_matches("http://"))
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "POST /hooks/open HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
                 Content-Length: 10\r\n\r\n{{",
                server.url.trim_start_matches("http://")
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut text = String::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut text))
        .await
        .expect("an answer")
        .unwrap();
    assert!(text.starts_with("HTTP/1.1 408 "), "{text}");
    assert_eq!(server.runner.started(), 0);
}

fn process_runner(home: &Path, program: &str) -> ProcessRunner {
    let limits = Limits {
        max_time: Duration::from_secs(30),
        max_tokens: 0,
    };
    ProcessRunner::new(
        PathBuf::from(program),
        home,
        crate::cron::deliver::tests::lookup(&[]),
        limits,
        Arc::new(|_: &str| {}),
    )
}

fn save_routes(home: &Path, routes: Vec<Route>) {
    let store = Store::new(home);
    let mut locked = store.lock().unwrap();
    locked.routes = routes;
    locked.save().unwrap();
}

#[test]
fn runs_start_without_any_route_secret() {
    let home = tempfile::tempdir().unwrap();
    let mut pr = route("pr");
    pr.secret = Some(Secret::Env("PR_SECRET".into()));
    let mut other = route("other");
    other.secret = Some(Secret::Env("OTHER_SECRET".into()));
    let mut file = route("file");
    file.secret = Some(Secret::File("/x".into()));
    save_routes(home.path(), vec![pr, other, file]);
    let command = process_runner(home.path(), "/bin/true").command("pr", "r1");
    let command = command.as_std();
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(args, ["webhook", "run", "pr", "--run-id", "r1"]);
    assert_eq!(command.get_current_dir(), Some(Path::new("/")));
    let mut removed: Vec<_> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    removed.sort();
    assert_eq!(removed, ["OTHER_SECRET", "PR_SECRET"]);
}

#[tokio::test]
async fn a_run_that_records_nothing_is_recorded_as_failed() {
    let home = tempfile::tempdir().unwrap();
    save_routes(home.path(), vec![route("pr")]);
    let incoming = Incoming::new("push", Some("d1"), json!({}), 5);
    // `true` ends at once without recording the run.
    let ok = process_runner(home.path(), "/bin/true")
        .run("pr", incoming.clone())
        .await;
    assert!(!ok);
    let stored = Store::new(home.path()).routes().unwrap().remove(0);
    let last = stored.last.unwrap();
    assert_eq!(last.status, RunStatus::Failed);
    assert_eq!(last.note, "the run ended without a result (exit status: 0)");
    assert_eq!(last.due, 5);
    assert_eq!(stored.last_event.as_deref(), Some("push"));
    let text = std::fs::read_to_string(last.output.unwrap()).unwrap();
    assert!(
        text.contains("(none: the run ended before it was made)"),
        "{text}"
    );

    // A run that crashed after recording its start is concluded too...
    let runner = process_runner(home.path(), "/bin/true");
    crate::webhook::run::start(home.path(), "pr", "r2", 10, 5).unwrap();
    let status = runner
        .conclude("pr", &incoming, "r2", 10, "gone".into())
        .await;
    assert_eq!(status, RunStatus::Failed);
    let last = Store::new(home.path()).routes().unwrap()[0]
        .last
        .clone()
        .unwrap();
    assert_eq!(
        (last.run_id.as_str(), last.status),
        ("r2", RunStatus::Failed)
    );
    assert_eq!(last.started_at, 10);
    // ... but one that recorded its end is left alone.
    let status = runner
        .conclude("pr", &incoming, "r2", 10, "again".into())
        .await;
    assert_eq!(status, RunStatus::Failed);
    let again = Store::new(home.path()).routes().unwrap()[0]
        .last
        .clone()
        .unwrap();
    assert_eq!(again.note, "gone");
    // A run that recorded `ok` ended ok.
    let store = Store::new(home.path());
    let mut locked = store.lock().unwrap();
    locked.routes[0].last.as_mut().unwrap().status = RunStatus::Ok;
    locked.save().unwrap();
    drop(locked);
    let status = runner
        .conclude("pr", &incoming, "r2", 10, "again".into())
        .await;
    assert_eq!(status, RunStatus::Ok);
}

#[test]
fn only_one_server_per_state_folder() {
    let home = tempfile::tempdir().unwrap();
    let held = lock(home.path()).unwrap();
    assert!(crate::webhook::is_serving(home.path()));
    let err = lock(home.path()).unwrap_err().to_string();
    assert!(err.contains("another `junees webhook serve`"), "{err}");
    drop(held);
    assert!(!crate::webhook::is_serving(home.path()));
    lock(home.path()).unwrap();
}
