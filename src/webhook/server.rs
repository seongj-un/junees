//! `junees webhook serve`: receives `POST /hooks/<name>`, checks the
//! request, answers at once, and runs what it accepted one request at a
//! time per route ([`Runner`]).
//!
//! The answers: 405 for another method, 404 for another path or an unknown
//! route, 401 for a missing or wrong signature, 403 for a route without a
//! secret that was not added with `--no-secret`, or when the server listens
//! on a non-loopback address or the request may not come straight from a
//! program on this machine ([`Server::check_unsigned`]), 415 for such a
//! request that is not JSON,
//! 400 for a request it cannot use, 200 `pong` for GitHub's `ping`, 503 while
//! paused or stopping, 202 `ignored` for events the route does not want,
//! 200 `duplicate` for a delivery id it has seen (the last [`REMEMBERED`];
//! the id of a run that did not end `ok` is forgotten, so it may be sent
//! again),
//! 429 when [`MAX_WAITING`] requests already wait for the route, else 202
//! `queued`.
//!
//! At most [`MAX_CONNECTIONS`] connections are read or answered at once;
//! one more gets 503 at once and is closed.
//!
//! Stopping (SIGINT or SIGTERM): no new connections, the waiting requests
//! are dropped, the runs going on finish (within their time limit); a
//! second signal interrupts them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};

use super::http::{self, Head};
use super::run::{Ended, Incoming};
use super::signature::{self, GENERIC_HEADER, GITHUB_HEADER};
use super::store::{Kind, Route, Store, check_name};
use crate::cancel::Cancel;
use crate::cron::daemon::{KILL_AFTER, signal_group, wait_limited};
use crate::cron::deliver::Lookup;
use crate::cron::run::{Limits, Outcome};
use crate::cron::store::{RunStatus, create_private_dir, random_hex};
use crate::cron::unix_now;
use crate::gateway::Log;
use crate::prompt_text::escape_hidden;

/// Requests that may wait for one route (besides the one running).
pub const MAX_WAITING: usize = 20;
/// Delivery ids remembered to answer repeats.
pub const REMEMBERED: usize = 1000;
/// How long a client may take to send its request.
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections read or answered at once (each may take [`READ_TIMEOUT`]).
pub const MAX_CONNECTIONS: usize = 64;
/// Headers a proxy or tunnel adds to say who sent a request; a route
/// without a secret refuses requests carrying one.
pub const PROXY_HEADERS: &[&str] = &[
    "x-forwarded-for",
    "forwarded",
    "cf-connecting-ip",
    "x-real-ip",
];

/// Runs accepted requests.
#[async_trait]
pub trait Runner: Send + Sync {
    /// Runs `incoming` on route `name` to its end; whether it ended `ok`.
    async fn run(&self, name: &str, incoming: Incoming) -> bool;
    /// Asks the runs going on to stop.
    fn interrupt(&self);
}

/// Reads the server's environment (route secrets in `--secret-env`).
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// An answer to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub text: String,
}

fn answer(status: u16, text: impl Into<String>) -> Answer {
    Answer {
        status,
        text: text.into(),
    }
}

#[derive(Default)]
struct State {
    /// Route name -> requests waiting for it.
    waiting: HashMap<String, VecDeque<Incoming>>,
    /// Routes with a run going on.
    running: HashSet<String>,
    /// `<route>/<delivery id>`, oldest first.
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    closing: bool,
}

impl State {
    fn remember(&mut self, key: String) {
        if self.seen_set.insert(key.clone()) {
            self.seen.push_back(key);
            if self.seen.len() > REMEMBERED
                && let Some(old) = self.seen.pop_front()
            {
                self.seen_set.remove(&old);
            }
        }
    }

    /// Forgets `key`, so a repeat of it is taken.
    fn forget(&mut self, key: &str) {
        if self.seen_set.remove(key) {
            self.seen.retain(|seen| seen != key);
        }
    }
}

/// The server of one state folder.
pub struct Server {
    home: PathBuf,
    /// Whether it listens on a loopback address only.
    loopback: bool,
    /// The port it listens on.
    port: u16,
    runner: Arc<dyn Runner>,
    env: Env,
    log: Log,
    read_timeout: Duration,
    max_connections: usize,
    state: Mutex<State>,
    /// Routes with a run going on (their workers), for stopping.
    busy: watch::Sender<usize>,
}

impl Server {
    /// A server for the listener bound to `listen`.
    pub fn new(
        home: &Path,
        listen: SocketAddr,
        runner: Arc<dyn Runner>,
        env: Env,
        log: Log,
    ) -> Self {
        Self {
            home: home.to_path_buf(),
            loopback: listen.ip().is_loopback(),
            port: listen.port(),
            runner,
            env,
            log,
            read_timeout: READ_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
            state: Mutex::default(),
            busy: watch::Sender::new(0),
        }
    }

    /// Gives clients `timeout` to send a request (tests).
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Reads or answers at most `max` connections at once (tests).
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Answers connections on `listener` until `stop`, then waits for the
    /// runs going on (interrupting them on `force`).
    pub async fn serve(self: Arc<Self>, listener: TcpListener, stop: Cancel, force: Cancel) {
        let places = Arc::new(Semaphore::new(self.max_connections));
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => match Arc::clone(&places).try_acquire_owned() {
                        Ok(place) => {
                            let server = Arc::clone(&self);
                            tokio::spawn(async move {
                                server.connection(stream).await;
                                drop(place);
                            });
                        }
                        // Answered without waiting (no task, no read): the
                        // connection closes even if the client never reads.
                        Err(_) => {
                            let busy = http::render(503, "too many connections; try again later");
                            let _ = stream.try_write(busy.as_bytes());
                        }
                    },
                    Err(err) => {
                        (self.log)(&format!("cannot accept a connection: {err}"));
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
        drop(listener);
        let dropped: usize = {
            let mut state = self.state.lock().unwrap();
            state.closing = true;
            state.waiting.drain().map(|(_, queue)| queue.len()).sum()
        };
        if dropped > 0 {
            (self.log)(&format!("{dropped} waiting request(s) dropped"));
        }
        let mut busy = self.busy.subscribe();
        let going = *busy.borrow();
        if going > 0 {
            (self.log)(&format!(
                "waiting for {going} run(s) to finish; a second signal interrupts them"
            ));
            let interrupted = tokio::select! {
                _ = busy.wait_for(|n| *n == 0) => false,
                _ = force.cancelled() => true,
            };
            if interrupted {
                (self.log)("interrupting the runs");
                self.runner.interrupt();
                let _ = busy.wait_for(|n| *n == 0).await;
            }
        }
    }

    async fn connection<S: AsyncRead + AsyncWrite + Unpin>(self: Arc<Self>, mut stream: S) {
        let read = tokio::time::timeout(self.read_timeout, self.read_request(&mut stream)).await;
        let (answer, what) = match read {
            Err(_) => (answer(408, "the request took too long"), String::new()),
            Ok(Err(refused)) => refused,
            Ok(Ok((route, head, body))) => {
                let what = format!("/hooks/{}", route.name);
                (self.handle(route, &head, &body).await, what)
            }
        };
        (self.log)(&format!(
            "{} {}: {}",
            if what.is_empty() { "request" } else { &what },
            answer.status,
            escape_hidden(&answer.text)
        ));
        http::respond(&mut stream, answer.status, &answer.text).await;
    }

    /// Reads a request for a route. What can be checked without the body
    /// (method, path, length, that the route exists) is checked before
    /// reading it, and before `100 Continue` asks the client to send it.
    /// A refusal comes with the path for the log (empty before it is known).
    async fn read_request<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        stream: &mut S,
    ) -> Result<(Route, Head, Vec<u8>), (Answer, String)> {
        let refused = |err: http::Error, what: &str| (answer(err.status, err.message), what.into());
        let mut buf = Vec::new();
        let head = http::read_head(stream, &mut buf)
            .await
            .map_err(|err| refused(err, ""))?;
        let name = head
            .path
            .strip_prefix("/hooks/")
            .filter(|name| check_name(name).is_ok())
            .map(str::to_string);
        if head.method != "POST" {
            return Err((answer(405, "only POST is accepted"), String::new()));
        }
        let Some(name) = name else {
            return Err((
                answer(404, "not found; webhooks are POST /hooks/<route name>"),
                String::new(),
            ));
        };
        let what = format!("/hooks/{name}");
        let length = http::body_length(&head).map_err(|err| refused(err, &what))?;
        let route = self
            .route(&name)
            .await
            .map_err(|refusal| (refusal, what.clone()))?;
        self.check_unsigned(&route, &head)
            .map_err(|refusal| (refusal, what.clone()))?;
        let body = http::read_body(stream, &head, length, buf)
            .await
            .map_err(|err| refused(err, &what))?;
        Ok((route, head, body))
    }

    /// Route `name`, read from the routes file for each request.
    async fn route(&self, name: &str) -> Result<Route, Answer> {
        let store = Store::new(&self.home);
        let routes = match tokio::task::spawn_blocking(move || store.routes()).await {
            Ok(Ok(routes)) => routes,
            Ok(Err(err)) => {
                (self.log)(&format!("cannot read the routes: {err:#}"));
                return Err(answer(500, "cannot read the routes"));
            }
            Err(_) => return Err(answer(500, "cannot read the routes")),
        };
        routes
            .into_iter()
            .find(|route| route.name == name)
            .ok_or_else(|| answer(404, "no such route"))
    }

    /// The answer to a request for `route`; queues it when it runs.
    pub async fn handle(self: &Arc<Self>, route: Route, head: &Head, body: &[u8]) -> Answer {
        let name = route.name.as_str();
        if self.state.lock().unwrap().closing {
            return answer(503, "the server is stopping");
        }
        if let Err(refused) = self.check_signature(&route, head, body) {
            return refused;
        }
        let (event, delivery) = match route.kind {
            Kind::Github => match head.header("x-github-event") {
                Some(event) => (event, head.header("x-github-delivery")),
                None => return answer(400, "missing X-GitHub-Event"),
            },
            Kind::Generic => (
                head.header("x-junees-event").unwrap_or("webhook"),
                head.header("x-junees-delivery"),
            ),
        };
        if !is_word(event, 64) {
            return answer(400, "malformed event name");
        }
        if delivery.is_some_and(|id| !is_word(id, 128)) {
            return answer(400, "malformed delivery id");
        }
        let Ok(payload) = serde_json::from_slice(body) else {
            return answer(400, "the body is not JSON");
        };
        if route.kind == Kind::Github && event == "ping" {
            return answer(200, "pong");
        }
        if route.paused {
            return answer(503, "the route is paused");
        }
        let incoming = Incoming::new(event, delivery, payload, unix_now());
        if !route.wants(&incoming.event, incoming.action.as_deref()) {
            return answer(202, "ignored: the route does not run for this event");
        }
        let key = delivery.map(|id| format!("{name}/{id}"));
        let start = {
            let mut state = self.state.lock().unwrap();
            if key.as_ref().is_some_and(|key| state.seen_set.contains(key)) {
                return answer(200, "duplicate");
            }
            if state.closing {
                return answer(503, "the server is stopping");
            }
            let queue = state.waiting.entry(name.to_string()).or_default();
            if queue.len() >= MAX_WAITING {
                return answer(429, "too many requests wait for this route");
            }
            queue.push_back(incoming);
            if let Some(key) = key {
                state.remember(key);
            }
            state.running.insert(name.to_string())
        };
        if start {
            self.busy.send_modify(|n| *n += 1);
            tokio::spawn(Arc::clone(self).work(name.to_string()));
        }
        answer(202, "queued")
    }

    /// Checks a request for a route without a secret, from its head. Such
    /// a route is served only on a loopback listener, and only to requests
    /// a program on this machine sends straight to it: the `Host` is a
    /// loopback literal with the server's port, no header a browser adds
    /// (`Origin`, `Sec-Fetch-*`) or a proxy or tunnel adds
    /// ([`PROXY_HEADERS`]), and a `Content-Type` of `application/json`.
    ///
    /// A loopback listener alone does not tell who sent a request: a web
    /// page open in the user's browser can POST to it (a `text/plain`
    /// request needs no CORS preflight), with its own name as `Host` after
    /// DNS rebinding, and a tunnel delivers outside requests from loopback.
    /// Browsers always send `Origin` or `Sec-Fetch-*` on such requests, and
    /// need a preflight the server never answers before sending JSON.
    fn check_unsigned(&self, route: &Route, head: &Head) -> Result<(), Answer> {
        if route.secret.is_some() {
            return Ok(());
        }
        if !route.no_secret {
            return Err(answer(
                403,
                "this route has no secret; add it again with one (or with --no-secret to accept \
                 unsigned requests)",
            ));
        }
        if !self.loopback {
            return Err(answer(
                403,
                "this route has no secret; it is only served on a loopback address",
            ));
        }
        let port = self.port;
        let local = [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ];
        let hosts: Vec<&str> = head
            .headers
            .iter()
            .filter(|(name, _)| name == "host")
            .map(|(_, value)| value.as_str())
            .collect();
        if !matches!(hosts[..], [host] if local.iter().any(|name| name.eq_ignore_ascii_case(host)))
        {
            return Err(answer(
                403,
                "this route has no secret; it only takes requests sent to this machine \
                 (Host 127.0.0.1, localhost or [::1] with the port)",
            ));
        }
        if head
            .headers
            .iter()
            .any(|(name, _)| name == "origin" || name.starts_with("sec-fetch-"))
        {
            return Err(answer(
                403,
                "this route has no secret; it does not take requests from a browser",
            ));
        }
        if head
            .headers
            .iter()
            .any(|(name, _)| PROXY_HEADERS.contains(&name.as_str()))
        {
            return Err(answer(
                403,
                "this route has no secret; it does not take requests through a proxy or tunnel",
            ));
        }
        let json = head
            .header("content-type")
            .and_then(|value| value.split(';').next())
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"));
        if !json {
            return Err(answer(
                415,
                "this route has no secret; send Content-Type: application/json",
            ));
        }
        Ok(())
    }

    /// Checks the signature `route` asks for. A route without one was
    /// checked by [`Self::check_unsigned`].
    fn check_signature(&self, route: &Route, head: &Head, body: &[u8]) -> Result<(), Answer> {
        let Some(secret) = &route.secret else {
            return Ok(());
        };
        let key = secret.read(&*self.env).map_err(|err| {
            (self.log)(&format!("route {}: {err:#}", route.name));
            answer(500, "the route's secret cannot be read")
        })?;
        let header = match route.kind {
            Kind::Github => GITHUB_HEADER,
            Kind::Generic => GENERIC_HEADER,
        };
        signature::verify(&key, body, head.header(header))
            .map_err(|refused| answer(401, refused.message()))
    }

    /// Runs the requests waiting for route `name`, one at a time, until
    /// none is left.
    async fn work(self: Arc<Self>, name: String) {
        loop {
            let next = {
                let mut state = self.state.lock().unwrap();
                match state.waiting.get_mut(&name).and_then(VecDeque::pop_front) {
                    Some(next) => next,
                    None => {
                        state.waiting.remove(&name);
                        state.running.remove(&name);
                        break;
                    }
                }
            };
            let key = next.delivery.as_ref().map(|id| format!("{name}/{id}"));
            // A failed run may be sent again (GitHub's "Redeliver" keeps
            // the delivery id): forget it.
            if !self.runner.run(&name, next).await
                && let Some(key) = key
            {
                self.state.lock().unwrap().forget(&key);
            }
        }
        self.busy.send_modify(|n| *n -= 1);
    }
}

/// Letters, digits, `_`, `-` and `.`, 1 to `max` of them.
fn is_word(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}

/// Takes the server lock of `home` (held while the file stays open).
pub fn lock(home: &Path) -> Result<File> {
    let dir = super::dir(home);
    create_private_dir(&dir)?;
    let path = dir.join(super::LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    if !crate::flock::try_exclusive(&file)
        .with_context(|| format!("cannot lock {}", path.display()))?
    {
        bail!(
            "another `junees webhook serve` is running for {}",
            home.display()
        );
    }
    Ok(file)
}

/// Starts each run as `<program> webhook run <name> --run-id <id>` in `/`,
/// in a process group of its own, with the request on its standard input
/// and without the routes' secret variables; stops it from outside when it
/// goes past its time limit by [`KILL_AFTER`].
pub struct ProcessRunner {
    program: PathBuf,
    home: PathBuf,
    lookup: Lookup,
    limits: Limits,
    log: Log,
    /// Run id -> process group of the runs going on.
    groups: Mutex<HashMap<String, i32>>,
}

impl ProcessRunner {
    pub fn new(program: PathBuf, home: &Path, lookup: Lookup, limits: Limits, log: Log) -> Self {
        Self {
            program,
            home: home.to_path_buf(),
            lookup,
            limits,
            log,
            groups: Mutex::default(),
        }
    }

    /// The command of run `run_id` of route `name`, without the secret
    /// variables of every route.
    fn command(&self, name: &str, run_id: &str) -> tokio::process::Command {
        let hidden: Vec<String> = Store::new(&self.home)
            .routes()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|route| match route.secret {
                Some(super::store::Secret::Env(name)) => Some(name),
                _ => None,
            })
            .collect();
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(["webhook", "run", name, "--run-id", run_id])
            .current_dir("/")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .process_group(0);
        for variable in &hidden {
            command.env_remove(variable);
        }
        command
    }

    /// Records a failure for a run that did not record its end. The run's
    /// status in the end.
    async fn conclude(
        &self,
        name: &str,
        incoming: &Incoming,
        run_id: &str,
        started: i64,
        note: String,
    ) -> RunStatus {
        let route = match Store::new(&self.home).routes() {
            Ok(routes) => routes.into_iter().find(|route| route.name == name),
            Err(err) => {
                (self.log)(&format!("cannot read the routes: {err:#}"));
                return RunStatus::Failed;
            }
        };
        let Some(route) = route else {
            (self.log)(&format!("route {name}: {note}; it was removed"));
            return RunStatus::Failed;
        };
        let last = route.last.as_ref().filter(|last| last.run_id == run_id);
        if let Some(last) = last
            && last.status != RunStatus::Running
        {
            (self.log)(&format!("route {name}: run {}", last.status.word()));
            return last.status;
        }
        (self.log)(&format!("route {name}: {note}"));
        let outcome = Outcome {
            session: last.and_then(|last| last.session.clone()),
            ..Outcome::failed(note)
        };
        let tz = crate::cron::timezone(&*self.lookup)
            .unwrap_or_else(|_| crate::cron::tz::TimeZone::utc());
        super::run::finish(
            &self.home,
            &tz,
            &self.lookup,
            &route,
            incoming,
            &Ended {
                run_id,
                started_at: last.map_or(started, |last| last.started_at),
                prompt: None,
                outcome: &outcome,
                quiet: false,
            },
        )
        .await;
        RunStatus::Failed
    }
}

#[async_trait]
impl Runner for ProcessRunner {
    async fn run(&self, name: &str, incoming: Incoming) -> bool {
        let run_id = random_hex(8);
        let started = unix_now();
        // The run does not need any route's secret.
        let mut child = match self.command(name, &run_id).spawn() {
            Ok(child) => child,
            Err(err) => {
                let note = format!("cannot start the run: {err}");
                return self.conclude(name, &incoming, &run_id, started, note).await
                    == RunStatus::Ok;
            }
        };
        let Some(pid) = child.id().map(|pid| pid as i32) else {
            let note = "the run ended at once".to_string();
            return self.conclude(name, &incoming, &run_id, started, note).await == RunStatus::Ok;
        };
        (self.log)(&format!(
            "route {name}: run {run_id} started ({})",
            escape_hidden(&incoming.label())
        ));
        self.groups.lock().unwrap().insert(run_id.clone(), pid);
        let input = serde_json::to_vec(&incoming).unwrap_or_default();
        let writer = child.stdin.take().map(|mut stdin| {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(&input).await;
            })
        });
        let exit = wait_limited(&mut child, pid, self.limits.max_time + KILL_AFTER).await;
        if let Some(writer) = writer {
            writer.abort();
        }
        self.groups.lock().unwrap().remove(&run_id);
        let note = format!("the run ended without a result ({exit})");
        self.conclude(name, &incoming, &run_id, started, note).await == RunStatus::Ok
    }

    fn interrupt(&self) {
        for &pid in self.groups.lock().unwrap().values() {
            signal_group(pid, libc::SIGINT);
        }
    }
}

#[cfg(test)]
mod tests;
