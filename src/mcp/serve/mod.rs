//! `junees mcp serve`: Junees as an MCP server, so that another agent
//! (Claude Desktop, Claude Code, Hermes, Cursor) can hand it work.
//!
//! - Transport: stdio only, one JSON-RPC 2.0 message per line
//!   (`basic/transports.md`, 2025-06-18). stdout carries protocol messages
//!   and nothing else; what Junees has to say goes to stderr.
//! - Protocol: `initialize` (version [`PROTOCOL_VERSION`], or an older one
//!   of [`SUPPORTED_VERSIONS`] the client asks for), `ping`, `tools/list`,
//!   `tools/call`, `notifications/cancelled`; progress goes out as
//!   `notifications/progress` when a call carries a `progressToken`.
//! - Tools ([`tools`]): `junees_run` (one turn of Junees's agent),
//!   `junees_sessions`, `junees_session`.
//!
//! Nobody can be asked a question through MCP, so a run follows the
//! unattended rules of webhook runs ([`crate::cron::run::Trigger::Mcp`]):
//! bash gets the webhook rules (no credentials, no state folder, no
//! persistent paths in the home folder), the sandbox is required, MCP
//! servers that are not trusted are left out, GitHub is read only, and
//! there is no approval gate, so whatever would be asked is refused. Write
//! and bash tools are there only with `--allow-write` and `--allow-bash`.
//! The working folder (`--dir`) may not be the home folder unless
//! `--allow-home`, and the claude-code backend is refused. One
//! `tools/call` runs at a time; the others wait in order.

pub mod tools;

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{Notify, mpsc};

use crate::cancel::Cancel;
use crate::config_file::Settings;

/// The protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Revisions a client may ask for; the tools work the same in each.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The longest line read from the client: a prompt fits many times over.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

pub const HELP: &str = "\
Usage: junees mcp serve [--dir <folder>] [--allow-write] [--allow-bash] [--allow-home]

Serves Junees as an MCP server on stdin and stdout (JSON-RPC, MCP
2025-06-18), for another agent to hand it work. Its tools: junees_run (a
turn of Junees's agent in the folder), junees_sessions and junees_session
(the sessions of that folder).

  --dir <folder>   where the runs work (default: the current folder); its
                   sessions are the only ones served
  --allow-write    give the runs write_file and edit_file
  --allow-bash     give the runs bash
  --allow-home     accept the home folder (or one holding it) as --dir

Runs are unattended, as webhook runs are: no approval questions (what
would be asked is refused), no credentials for bash, the sandbox
required, untrusted MCP servers left out, GitHub read only. The
claude-code backend is not served. Logs go to stderr.
";

/// The command line of `junees mcp serve`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub dir: Option<PathBuf>,
    pub allow_write: bool,
    pub allow_bash: bool,
    pub allow_home: bool,
    pub help: bool,
}

/// Parses the arguments after `serve`.
pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Options> {
    let mut options = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => options.help = true,
            "--allow-write" => options.allow_write = true,
            "--allow-bash" => options.allow_bash = true,
            "--allow-home" => options.allow_home = true,
            "--dir" => {
                let Some(dir) = args.next() else {
                    bail!("--dir needs a folder");
                };
                options.dir = Some(PathBuf::from(dir));
            }
            other => match other.strip_prefix("--dir=") {
                Some(dir) => options.dir = Some(PathBuf::from(dir)),
                None => bail!("unknown option `{other}` of `junees mcp serve` (see --help)"),
            },
        }
    }
    Ok(options)
}

/// A line for stdout, in the order given.
type Out = mpsc::UnboundedSender<Value>;

fn send(out: &Out, message: Value) {
    let _ = out.send(message);
}

fn reply(out: &Out, id: &Value, result: Value) {
    send(out, json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn error(out: &Out, id: &Value, code: i64, message: &str) {
    send(
        out,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    );
}

/// A request id as a key (`1` and `"1"` are different ids).
fn key(id: &Value) -> String {
    id.to_string()
}

/// A `tools/call` waiting for its turn or running.
struct Call {
    id: Value,
    params: Value,
}

/// What the reader and the worker share.
#[derive(Default)]
struct Queue {
    waiting: VecDeque<Call>,
    /// Cancelled before they ran: dropped without an answer.
    cancelled: HashSet<String>,
    /// The call that runs, and how to stop it.
    running: Option<(String, Cancel)>,
    closed: bool,
}

/// Runs the server on stdin and stdout until stdin ends.
pub async fn main(options: &Options, settings: Settings) -> Result<ExitCode> {
    if options.help {
        print!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    }
    let server = tools::Server::start(options, settings).await?;
    eprintln!(
        "junees {} mcp serve: working in {}; tools: {}",
        env!("CARGO_PKG_VERSION"),
        server.dir.display(),
        server
            .list()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let local = tokio::task::LocalSet::new();
    local
        .run_until(serve(server, tokio::io::stdin(), std::io::stdout()))
        .await;
    eprintln!("junees mcp serve: stdin closed, stopped");
    Ok(ExitCode::SUCCESS)
}

/// Answers the messages read from `input` on `output` until `input` ends;
/// then stops the running call and the MCP servers. Runs on a
/// [`tokio::task::LocalSet`] (the agent's futures are not `Send`).
pub async fn serve(
    server: tools::Server,
    input: impl AsyncRead + Unpin,
    output: impl Write + Send + 'static,
) {
    let (out, mut lines) = mpsc::unbounded_channel::<Value>();
    // The only writer of stdout.
    let writer = tokio::task::spawn_blocking(move || {
        let mut output = output;
        while let Some(message) = lines.blocking_recv() {
            let line = message.to_string();
            if output
                .write_all(line.as_bytes())
                .and_then(|()| output.write_all(b"\n"))
                .and_then(|()| output.flush())
                .is_err()
            {
                break;
            }
        }
    });
    let server = Rc::new(server);
    let queue = Rc::new(RefCell::new(Queue::default()));
    let wake = Rc::new(Notify::new());
    let worker = tokio::task::spawn_local(work(
        Rc::clone(&server),
        Rc::clone(&queue),
        Rc::clone(&wake),
        out.clone(),
    ));
    let mut reader = BufReader::new(input);
    let mut initialized = false;
    loop {
        let line = match read_line(&mut reader).await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(why) => {
                eprintln!("junees mcp serve: {why}");
                error(&out, &Value::Null, -32700, &why);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                error(&out, &Value::Null, -32700, "parse error: not JSON");
                continue;
            }
        };
        handle(&server, &queue, &wake, &out, &mut initialized, message);
    }
    {
        let mut queue = queue.borrow_mut();
        queue.closed = true;
        queue.waiting.clear();
        if let Some((_, cancel)) = &queue.running {
            cancel.cancel();
        }
    }
    wake.notify_one();
    let _ = worker.await;
    server.shutdown().await;
    drop(out);
    let _ = writer.await;
}

/// One message from the client.
fn handle(
    server: &tools::Server,
    queue: &Rc<RefCell<Queue>>,
    wake: &Notify,
    out: &Out,
    initialized: &mut bool,
    message: Value,
) {
    if message.is_array() {
        // Batches were taken out of the protocol in 2025-06-18.
        error(out, &Value::Null, -32600, "batches are not supported");
        return;
    }
    let method = message.get("method").and_then(Value::as_str);
    let id = message
        .get("id")
        .filter(|id| id.is_string() || id.is_number());
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(method) = method else {
        // A response to a request of ours: the server sends none.
        if id.is_none() {
            error(out, &Value::Null, -32600, "not a JSON-RPC request");
        }
        return;
    };
    let Some(id) = id.cloned() else {
        notification(queue, method, &params);
        return;
    };
    match method {
        "initialize" => {
            *initialized = true;
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked
                .filter(|asked| SUPPORTED_VERSIONS.contains(asked))
                .unwrap_or(PROTOCOL_VERSION);
            reply(
                out,
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "junees",
                        "title": "Junees",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": tools::INSTRUCTIONS,
                }),
            );
        }
        "ping" => reply(out, &id, json!({})),
        _ if !*initialized => error(out, &id, -32002, "the server is not initialized yet"),
        "tools/list" => reply(out, &id, json!({ "tools": server.list() })),
        "tools/call" => {
            queue.borrow_mut().waiting.push_back(Call { id, params });
            wake.notify_one();
        }
        _ => error(out, &id, -32601, &format!("method not found: {method}")),
    }
}

/// A notification: only `notifications/cancelled` does anything.
fn notification(queue: &Rc<RefCell<Queue>>, method: &str, params: &Value) {
    if method != "notifications/cancelled" {
        return;
    }
    let Some(id) = params.get("requestId") else {
        return;
    };
    let id = key(id);
    let mut queue = queue.borrow_mut();
    match &queue.running {
        Some((running, cancel)) if *running == id => cancel.cancel(),
        _ => {
            if queue.waiting.iter().any(|call| key(&call.id) == id) {
                queue.cancelled.insert(id);
            }
        }
    }
}

/// Runs the calls one after the other.
async fn work(server: Rc<tools::Server>, queue: Rc<RefCell<Queue>>, wake: Rc<Notify>, out: Out) {
    loop {
        let next = {
            let mut queue = queue.borrow_mut();
            if queue.closed {
                return;
            }
            let next = queue.waiting.pop_front();
            if let Some(call) = &next {
                let id = key(&call.id);
                if queue.cancelled.remove(&id) {
                    continue;
                }
                queue.running = Some((id, Cancel::new()));
            }
            next.map(|call| {
                let cancel = queue.running.as_ref().map(|(_, cancel)| cancel.clone());
                (call, cancel.unwrap_or_default())
            })
        };
        let Some((call, cancel)) = next else {
            wake.notified().await;
            continue;
        };
        let answer = server.call(&call.params, &cancel, &out).await;
        queue.borrow_mut().running = None;
        match answer {
            tools::Answer::Result(result) => reply(&out, &call.id, result),
            tools::Answer::Error(code, message) => error(&out, &call.id, code, &message),
            // A cancelled request gets no answer (`utilities/cancellation`).
            tools::Answer::Cancelled => {}
        }
    }
}

/// The next line of `reader` without its line end; `None` at the end. A
/// line over [`MAX_LINE_BYTES`] is read to its end and refused.
async fn read_line(reader: &mut (impl AsyncBufReadExt + Unpin)) -> Result<Option<String>, String> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|err| format!("cannot read stdin: {err}"))?;
        if buffer.is_empty() {
            if line.is_empty() && !too_long {
                return Ok(None);
            }
            break;
        }
        let (taken, done) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (buffer.len(), false),
        };
        if !too_long {
            line.extend_from_slice(&buffer[..taken]);
            if line.len() > MAX_LINE_BYTES {
                too_long = true;
                line = Vec::new();
            }
        }
        reader.consume(taken);
        if done {
            break;
        }
    }
    if too_long {
        return Err(format!("a message over {MAX_LINE_BYTES} bytes was dropped"));
    }
    String::from_utf8(line)
        .map(|line| Some(line.trim_end_matches(['\n', '\r']).to_string()))
        .map_err(|_| "a message that is not UTF-8 was dropped".to_string())
}

/// Where the server works: `--dir` or the current folder, which must be a
/// folder, made absolute.
pub fn work_dir(options: &Options) -> Result<PathBuf> {
    let dir = match &options.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().context("cannot read the current folder")?,
    };
    let dir = dir
        .canonicalize()
        .with_context(|| format!("--dir {}: no such folder", dir.display()))?;
    if !dir.is_dir() {
        bail!("--dir {} is not a folder", dir.display());
    }
    Ok(dir)
}

#[cfg(test)]
mod tests;
