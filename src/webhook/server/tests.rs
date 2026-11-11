use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use super::*;
use crate::cron::deliver::Delivery;
use crate::webhook::store::Secret;
use crate::webhook::store::tests::route;

const SECRET: &str = "s3cret";

/// Records each run, then waits for a permit of `gate`.
struct FakeRunner {
    runs: Mutex<Vec<(String, Incoming)>>,
    gate: Semaphore,
    interrupted: AtomicBool,
}

#[async_trait]
impl Runner for FakeRunner {
    async fn run(&self, name: &str, incoming: Incoming) {
        self.runs.lock().unwrap().push((name.to_string(), incoming));
        self.gate.acquire().await.unwrap().forget();
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
    let server = Arc::new(
        Server::new(
            home.path(),
            loopback,
            Arc::clone(&runner) as Arc<dyn Runner>,
            env,
            Arc::new(|_: &str| {}),
        )
        .with_read_timeout(read_timeout),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
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

/// POSTs `body` to `/hooks/<name>` with `headers` (and a length).
async fn post(url: &str, name: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut request = format!("POST /hooks/{name} HTTP/1.1\r\nHost: x\r\n");
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
async fn a_slow_client_gets_408() {
    let server = start(true, Duration::from_millis(300)).await;
    let mut stream = tokio::net::TcpStream::connect(server.url.trim_start_matches("http://"))
        .await
        .unwrap();
    stream
        .write_all(b"POST /hooks/open HTTP/1.1\r\nContent-Length: 10\r\n\r\n{")
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
    process_runner(home.path(), "/bin/true")
        .run("pr", incoming.clone())
        .await;
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
    runner
        .conclude("pr", &incoming, "r2", 10, "gone".into())
        .await;
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
    runner
        .conclude("pr", &incoming, "r2", 10, "again".into())
        .await;
    let again = Store::new(home.path()).routes().unwrap()[0]
        .last
        .clone()
        .unwrap();
    assert_eq!(again.note, "gone");
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
