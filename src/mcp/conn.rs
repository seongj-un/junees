//! One MCP server process and the JSON-RPC 2.0 messages on its standard
//! streams (MCP stdio transport: one message per line, UTF-8, no embedded
//! newlines; the server may log on stderr).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, watch};

use super::config::ServerConfig;
use crate::tools::{CLAUDE_CODE_SECRET_ENV_VARS, SECRET_ENV_VARS};

/// A longer message from the server ends the connection: it would have to
/// sit in memory whole.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// Bytes of the server's stderr kept for error messages.
const STDERR_TAIL_BYTES: usize = 4096;
/// How long a server may take to exit after its input closed (and again
/// after SIGTERM) before the next step.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// An error response of the server.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// Why a request got no result.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Rpc(RpcError),
    /// No answer in time; the request was cancelled.
    Timeout(Duration),
    /// The server is gone, with why.
    Closed(String),
}

type Reply = Result<Value, RpcError>;
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Reply>>>>;
type Outgoing = Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>;

/// A running server process. Dropping it kills the server's process group
/// if it is still there.
pub struct Connection {
    pid: u32,
    outgoing: Outgoing,
    waiting: Waiting,
    next_id: AtomicU64,
    /// Why the server's output ended, once it did.
    closed: watch::Receiver<Option<String>>,
    /// Set once the server process was reaped.
    exited: watch::Receiver<bool>,
    reaped: Arc<AtomicBool>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl Connection {
    /// Starts the server of `config` in its own process group, in its run
    /// folder ([`ServerConfig::run_folder`], never the folder Junees works
    /// in unless `cwd` says so). Its environment is Junees's without
    /// [`SECRET_ENV_VARS`], [`CLAUDE_CODE_SECRET_ENV_VARS`] and
    /// `hidden_env`, plus the server's `env`.
    pub fn spawn(config: &ServerConfig, hidden_env: &[String]) -> std::io::Result<Self> {
        let mut command = Command::new(config.program());
        command
            .args(&config.args)
            .current_dir(config.run_folder())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        for name in SECRET_ENV_VARS
            .iter()
            .chain(CLAUDE_CODE_SECRET_ENV_VARS)
            .copied()
            .chain(hidden_env.iter().map(String::as_str))
        {
            command.env_remove(name);
        }
        command.envs(&config.env);
        let mut child = command.spawn()?;
        let pid = child.id().unwrap_or_default();
        let (stdin, stdout, stderr) = (
            child.stdin.take().expect("piped stdin"),
            child.stdout.take().expect("piped stdout"),
            child.stderr.take().expect("piped stderr"),
        );

        let (sender, mut lines) = mpsc::unbounded_channel::<String>();
        let outgoing: Outgoing = Arc::new(Mutex::new(Some(sender)));
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = lines.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            // Dropping stdin closes it: the server's signal to exit.
        });

        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn(keep_tail(stderr, Arc::clone(&stderr_tail)));

        let waiting: Waiting = Arc::default();
        let (closed_tx, closed) = watch::channel(None);
        let (exited_tx, exited) = watch::channel(false);
        let reaped = Arc::new(AtomicBool::new(false));
        tokio::spawn(read_messages(
            child,
            pid,
            stdout,
            Reader {
                waiting: Arc::clone(&waiting),
                outgoing: Arc::clone(&outgoing),
            },
            closed_tx,
            exited_tx,
            Arc::clone(&reaped),
        ));
        Ok(Self {
            pid,
            outgoing,
            waiting,
            next_id: AtomicU64::new(1),
            closed,
            exited,
            reaped,
            stderr: stderr_tail,
        })
    }

    /// The server's process id (and process group).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Why the server is gone, `None` while it runs.
    pub fn closed(&self) -> Option<String> {
        self.closed.borrow().clone()
    }

    /// The last line the server wrote on stderr, shortened and with
    /// hidden characters escaped, if any.
    pub fn stderr_line(&self) -> Option<String> {
        let tail = self.stderr.lock().unwrap();
        let text = String::from_utf8_lossy(&tail);
        let line = text
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())?;
        let line: String = line.chars().take(300).collect();
        Some(crate::prompt_text::escape_hidden(&line))
    }

    /// Sends a notification; quietly does nothing once the server is gone.
    pub fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn send(&self, message: &Value) -> bool {
        let mut line = message.to_string();
        line.push('\n');
        match &*self.outgoing.lock().unwrap() {
            Some(sender) => sender.send(line).is_ok(),
            None => false,
        }
    }

    /// Sends a request. The answer comes through [`Call::wait`]; dropping
    /// the call before that cancels the request (`notifications/cancelled`),
    /// unless `cancellable` is false (`initialize` must not be cancelled).
    pub fn start(&self, method: &str, params: Value, cancellable: bool) -> Call<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, sender);
        let sent =
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        if !sent {
            self.waiting.lock().unwrap().remove(&id);
        }
        Call {
            connection: self,
            id,
            receiver,
            cancellable,
            done: !sent,
        }
    }

    /// Sends a request and waits up to `timeout` for its answer.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let mut call = self.start(method, params, true);
        match tokio::time::timeout(timeout, call.wait()).await {
            Ok(result) => result,
            Err(_) => Err(CallError::Timeout(timeout)),
        }
    }

    /// Ends the server the way the stdio transport says: close its input,
    /// wait, then SIGTERM, wait, then SIGKILL, each to its process group.
    pub async fn shutdown(&self) {
        self.outgoing.lock().unwrap().take();
        let mut exited = self.exited.clone();
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            if tokio::time::timeout(EXIT_GRACE, exited.wait_for(|done| *done))
                .await
                .is_ok()
            {
                return;
            }
            self.kill(signal);
        }
        let _ = tokio::time::timeout(EXIT_GRACE, exited.wait_for(|done| *done)).await;
    }

    fn kill(&self, signal: i32) {
        if self.pid == 0 || self.reaped.load(Ordering::SeqCst) {
            return;
        }
        kill_group(self.pid, signal);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.outgoing.lock().unwrap().take();
        self.kill(libc::SIGKILL);
    }
}

fn kill_group(pid: u32, signal: i32) {
    if let Ok(pid) = i32::try_from(pid)
        && pid > 0
    {
        // SAFETY: killpg only sends a signal. The group was created for the
        // server with process_group(0), so its id equals the server's pid.
        unsafe {
            libc::killpg(pid, signal);
        }
    }
}

/// A request in flight.
pub struct Call<'a> {
    connection: &'a Connection,
    id: u64,
    receiver: oneshot::Receiver<Reply>,
    cancellable: bool,
    done: bool,
}

impl Call<'_> {
    /// The answer. Cancel-safe: dropping the future keeps the call waiting.
    pub async fn wait(&mut self) -> Result<Value, CallError> {
        if self.done {
            return Err(CallError::Closed(self.reason()));
        }
        let reply = (&mut self.receiver).await;
        self.done = true;
        match reply {
            Ok(reply) => reply.map_err(CallError::Rpc),
            Err(_) => Err(CallError::Closed(self.reason())),
        }
    }

    fn reason(&self) -> String {
        self.connection
            .closed()
            .unwrap_or_else(|| "the server closed its output".into())
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let waiting = self.connection.waiting.lock().unwrap().remove(&self.id);
        if waiting.is_some() && self.cancellable {
            self.connection.notify(
                "notifications/cancelled",
                json!({"requestId": self.id, "reason": "junees stopped waiting"}),
            );
        }
    }
}

/// What the message reader needs to answer.
struct Reader {
    waiting: Waiting,
    outgoing: Outgoing,
}

impl Reader {
    fn handle(&self, message: Value) {
        match message {
            // Batches were allowed in 2025-03-26.
            Value::Array(items) => items.into_iter().for_each(|item| self.handle(item)),
            Value::Object(object) => {
                let id = object.get("id");
                match (object.get("method").and_then(Value::as_str), id) {
                    // A request of the server (legacy revisions allow
                    // them): answer `ping`, refuse the rest.
                    (Some(method), Some(id)) => {
                        let reply = if method == "ping" {
                            json!({"jsonrpc": "2.0", "id": id, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": id, "error": {
                                "code": -32601,
                                "message": format!("junees does not support `{method}`")
                            }})
                        };
                        let mut line = reply.to_string();
                        line.push('\n');
                        if let Some(sender) = &*self.outgoing.lock().unwrap() {
                            let _ = sender.send(line);
                        }
                    }
                    // Notifications (progress, logs, list changes) are
                    // not used.
                    (Some(_), None) => {}
                    (None, Some(id)) => {
                        let Some(id) = id.as_u64() else { return };
                        let Some(sender) = self.waiting.lock().unwrap().remove(&id) else {
                            return;
                        };
                        let reply = match object.get("error") {
                            Some(error) => Err(RpcError {
                                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                                message: error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                data: error.get("data").cloned(),
                            }),
                            None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = sender.send(reply);
                    }
                    (None, None) => {}
                }
            }
            _ => {}
        }
    }
}

/// Reads the server's messages until its output ends, then reaps it.
async fn read_messages(
    mut child: Child,
    pid: u32,
    stdout: impl AsyncRead + Unpin,
    reader: Reader,
    closed: watch::Sender<Option<String>>,
    exited: watch::Sender<bool>,
    reaped: Arc<AtomicBool>,
) {
    let mut stdout = BufReader::new(stdout);
    let mut line = Vec::new();
    let reason = loop {
        line.clear();
        match read_line(&mut stdout, &mut line).await {
            Ok(0) => break "the server closed its output".to_string(),
            Ok(_) => {
                // Anything else on stdout breaks the transport's rules;
                // skip it rather than give up on the server.
                if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                    reader.handle(message);
                }
            }
            Err(err) => break format!("reading the server's output failed: {err}"),
        }
    };
    let too_long = reason.contains("longer than");
    let _ = closed.send(Some(reason.clone()));
    // Wakes everyone waiting for an answer.
    reader.waiting.lock().unwrap().clear();
    reader.outgoing.lock().unwrap().take();
    if too_long {
        kill_group(pid, libc::SIGKILL);
    }
    let status = match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            kill_group(pid, libc::SIGKILL);
            child.wait().await
        }
    };
    reaped.store(true, Ordering::SeqCst);
    // What the server left running in its group goes with it.
    kill_group(pid, libc::SIGKILL);
    if let Ok(status) = status {
        let _ = closed.send(Some(format!("{reason}; it exited ({status})")));
    }
    let _ = exited.send(true);
}

/// Reads one line (without its `\n`) into `line`; 0 at the end of output.
/// A line over [`MAX_MESSAGE_BYTES`] is an error.
async fn read_line(
    stdout: &mut (impl AsyncBufReadExt + Unpin),
    line: &mut Vec<u8>,
) -> std::io::Result<usize> {
    let mut read = 0;
    loop {
        let buffer = stdout.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(read);
        }
        let (part, found) = match buffer.iter().position(|&b| b == b'\n') {
            Some(end) => (&buffer[..end], true),
            None => (buffer, false),
        };
        if line.len() + part.len() > MAX_MESSAGE_BYTES {
            return Err(std::io::Error::other(format!(
                "the server sent a message longer than {} MiB",
                MAX_MESSAGE_BYTES / (1024 * 1024)
            )));
        }
        line.extend_from_slice(part);
        let used = part.len() + usize::from(found);
        read += used;
        stdout.consume(used);
        if found {
            return Ok(read);
        }
    }
}

/// Keeps the last [`STDERR_TAIL_BYTES`] of the server's stderr.
async fn keep_tail(mut stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let mut tail = tail.lock().unwrap();
        tail.extend_from_slice(&chunk[..n]);
        if tail.len() > STDERR_TAIL_BYTES {
            let cut = tail.len() - STDERR_TAIL_BYTES;
            tail.drain(..cut);
        }
    }
}
