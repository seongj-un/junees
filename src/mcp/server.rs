//! Starting one MCP server: the version handshake, its tool list, and
//! calls to its tools.
//!
//! Junees speaks both eras of the protocol (MCP 2026-07-28, "Versioning
//! and Compatibility"): it probes with `server/discover` and, when the
//! server answers with anything but a discovery result or a modern error,
//! or not at all within [`Timeouts::probe`], falls back to the legacy
//! `initialize` / `notifications/initialized` handshake (2025-11-25 and
//! earlier). Over Streamable HTTP the same eras are found the way that
//! binding says ([`super::http`]).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::config::ServerConfig;
use super::conn::{CallError, Connection, RpcError};
use super::http::{HttpConfig, HttpConnection, meta};
use super::link::Link;
use crate::tools::ToolOutput;

/// Where the values of HTTP header references come from: the process
/// environment, or a map in tests.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The modern revision Junees speaks: every request carries its version
/// and the client's capabilities in `_meta`.
pub const MODERN_VERSION: &str = "2026-07-28";
/// The legacy revisions Junees accepts from `initialize`, newest first.
pub const LEGACY_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// `UnsupportedProtocolVersion`, a modern error.
const UNSUPPORTED_VERSION: i64 = -32022;
/// Pages of `tools/list` read at most.
const MAX_PAGES: usize = 100;

/// How long starting a server may take.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// For the `server/discover` probe, before falling back to
    /// `initialize` (the answer may still come during the handshake).
    pub probe: Duration,
    /// For the whole start: probe, handshake and tool list.
    pub start: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            probe: Duration::from_secs(5),
            start: Duration::from_secs(30),
        }
    }
}

/// Which protocol a server speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Era {
    /// Per-request metadata, at this version.
    Modern(String),
    /// The `initialize` session, at this version.
    Legacy(String),
}

impl Era {
    pub(super) fn version(&self) -> &str {
        match self {
            Era::Modern(version) | Era::Legacy(version) => version,
        }
    }

    /// `params` with the `_meta` a modern request needs.
    pub(super) fn params(&self, mut params: Value) -> Value {
        if let Era::Modern(version) = self {
            params["_meta"] = request_meta(version);
        }
        params
    }
}

fn client_info() -> Value {
    json!({"name": "junees", "version": env!("CARGO_PKG_VERSION")})
}

pub(super) fn request_meta(version: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": client_info(),
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A tool as the server lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One configured server and its process, if it runs.
pub struct Server {
    pub config: ServerConfig,
    hidden_env: Vec<String>,
    /// The folder Junees works in ([`ServerConfig::check_workdir`]).
    workdir: PathBuf,
    timeouts: Timeouts,
    env: EnvLookup,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    connection: Option<Arc<Link>>,
    era: Option<Era>,
    tools: Vec<RemoteTool>,
    /// Why the last start failed.
    failure: Option<String>,
    /// Notes from the last start (skipped tools).
    notes: Vec<String>,
}

/// What `/mcp` says about a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Disabled,
    Running { version: String, tools: usize },
    Failed(String),
    Stopped(String),
}

impl Server {
    pub fn new(
        config: ServerConfig,
        hidden_env: Vec<String>,
        workdir: PathBuf,
        timeouts: Timeouts,
    ) -> Self {
        Self {
            config,
            hidden_env,
            workdir,
            timeouts,
            env: Arc::new(|name: &str| std::env::var(name).ok()),
            state: Mutex::default(),
        }
    }

    /// Reads the values of HTTP header references through `env` instead of
    /// the process environment.
    pub fn with_env(mut self, env: EnvLookup) -> Self {
        self.env = env;
        self
    }

    /// `stdio` or `HTTP`.
    pub fn transport(&self) -> &'static str {
        if self.config.http.is_some() {
            "HTTP"
        } else {
            "stdio"
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn status(&self) -> Status {
        if !self.config.enabled {
            return Status::Disabled;
        }
        let state = self.state.lock().unwrap();
        match (&state.connection, &state.era) {
            (Some(connection), Some(era)) => match connection.closed() {
                None => Status::Running {
                    version: era.version().to_string(),
                    tools: state.tools.len(),
                },
                Some(reason) => Status::Stopped(reason),
            },
            _ => Status::Failed(
                state
                    .failure
                    .clone()
                    .unwrap_or_else(|| "not started".into()),
            ),
        }
    }

    /// The tools of the last successful start.
    pub fn tools(&self) -> Vec<RemoteTool> {
        self.state.lock().unwrap().tools.clone()
    }

    /// Notes from the last start, such as skipped tools.
    pub fn notes(&self) -> Vec<String> {
        self.state.lock().unwrap().notes.clone()
    }

    /// The server's process id while it runs (tests).
    pub fn pid(&self) -> Option<u32> {
        let state = self.state.lock().unwrap();
        state
            .connection
            .as_ref()
            .and_then(|connection| connection.pid())
    }

    /// Whether it is enabled but not running (never started, failed or
    /// exited).
    pub fn needs_start(&self) -> bool {
        matches!(self.status(), Status::Failed(_) | Status::Stopped(_))
    }

    /// Starts the server (stopping an earlier process first): handshake
    /// and tool list within [`Timeouts::start`]. A failure is kept for
    /// [`Server::status`] and returned.
    pub async fn start(&self) -> Result<(), String> {
        if let Err(refused) = self.config.check_workdir(&self.workdir) {
            self.state.lock().unwrap().failure = Some(refused.clone());
            return Err(refused);
        }
        let old = self.state.lock().unwrap().connection.take();
        if let Some(old) = old {
            old.shutdown().await;
        }
        let started = tokio::time::timeout(self.timeouts.start, self.connect()).await;
        let result = match started {
            Ok(Ok((connection, era, tools, notes))) => {
                let mut state = self.state.lock().unwrap();
                *state = State {
                    connection: Some(connection),
                    era: Some(era),
                    tools,
                    failure: None,
                    notes,
                };
                return Ok(());
            }
            Ok(Err(err)) => err,
            Err(_) => format!(
                "it did not finish starting within {}",
                seconds(self.timeouts.start)
            ),
        };
        self.state.lock().unwrap().failure = Some(result.clone());
        Err(result)
    }

    /// A server that answered the handshake, with its tools. Dropping the
    /// future (the start timeout) kills a server process.
    async fn connect(&self) -> Result<(Arc<Link>, Era, Vec<RemoteTool>, Vec<String>), String> {
        let (link, era, capabilities) = match &self.config.http {
            Some(http) => self.connect_http(http).await?,
            None => self.connect_stdio().await?,
        };
        let (tools, notes) = if capabilities.get("tools").is_some() {
            list_tools(&link, &era)
                .await
                .map_err(|err| explain(&link, err))?
        } else {
            (
                Vec::new(),
                vec!["it offers no tools (no `tools` capability)".into()],
            )
        };
        Ok((Arc::new(link), era, tools, notes))
    }

    async fn connect_http(&self, http: &HttpConfig) -> Result<(Link, Era, Value), String> {
        let connection = HttpConnection::new(http, &*self.env, super::conn::MAX_MESSAGE_BYTES)?;
        let (era, capabilities) = connection.handshake().await?;
        Ok((Link::Http(connection), era, capabilities))
    }

    async fn connect_stdio(&self) -> Result<(Link, Era, Value), String> {
        let connection = self.spawn()?;
        let (connection, era, capabilities) = match self.handshake(&connection).await {
            Ok((era, capabilities)) => (connection, era, capabilities),
            // A legacy server may exit on a request before `initialize`:
            // start it again and go straight to the handshake.
            Err(Handshake::ExitedOnProbe) => {
                let connection = self.spawn()?;
                let (era, capabilities) = legacy(&connection, None)
                    .await
                    .map_err(|err| self.explain(&connection, err))?;
                (connection, era, capabilities)
            }
            Err(Handshake::Failed(err)) => return Err(self.explain(&connection, err)),
        };
        Ok((Link::Stdio(connection), era, capabilities))
    }

    fn spawn(&self) -> Result<Connection, String> {
        Connection::spawn(&self.config, &self.hidden_env)
            .map_err(|err| format!("cannot run `{}`: {err}", self.config.command))
    }

    /// The error, with the server's last stderr line if it left one.
    fn explain(&self, connection: &Connection, err: String) -> String {
        match connection.stderr_line() {
            Some(line) => format!("{err} (its stderr: {line})"),
            None => err,
        }
    }

    async fn handshake(&self, connection: &Connection) -> Result<(Era, Value), Handshake> {
        let mut probe = connection.start(
            "server/discover",
            json!({"_meta": request_meta(MODERN_VERSION)}),
            true,
        );
        let answer = tokio::time::timeout(self.timeouts.probe, probe.wait()).await;
        match answer {
            Ok(Ok(result)) => modern(&result).map_err(Handshake::Failed),
            Ok(Err(CallError::Rpc(error))) if error.code == UNSUPPORTED_VERSION => {
                Err(Handshake::Failed(unsupported(&error)))
            }
            Ok(Err(CallError::Closed(_))) => Err(Handshake::ExitedOnProbe),
            Ok(Err(_)) => legacy(connection, None).await.map_err(Handshake::Failed),
            // Silent: a legacy server ignoring an early request, or a slow
            // modern one. Whichever handshake is answered first decides.
            Err(_) => legacy(connection, Some(probe))
                .await
                .map_err(Handshake::Failed),
        }
    }

    /// Runs the tool `tool` with `arguments`.
    pub async fn call(&self, tool: &str, arguments: Value) -> ToolOutput {
        let name = &self.config.name;
        let (connection, era, failure, schema) = {
            let state = self.state.lock().unwrap();
            (
                state.connection.clone(),
                state.era.clone(),
                state.failure.clone(),
                state
                    .tools
                    .iter()
                    .find(|known| known.name == tool)
                    .map(|known| known.input_schema.clone()),
            )
        };
        let (Some(connection), Some(era)) = (connection, era) else {
            return ToolOutput::error(format!(
                "the MCP server `{name}` is not running ({}); junees starts it again in the \
                 next session",
                failure.unwrap_or_else(|| "not started".into())
            ));
        };
        if let Some(reason) = connection.closed() {
            return stopped(name, &reason);
        }
        // A modern HTTP server reads marked parameters from headers too.
        let headers = match (&*connection, &era, schema) {
            (Link::Http(_), Era::Modern(_), Some(schema)) => meta::param_headers(&schema)
                .map(|marked| meta::param_values(&marked, &arguments))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let params = era.params(json!({"name": tool, "arguments": arguments}));
        match connection
            .request("tools/call", params, self.config.call_timeout, &headers)
            .await
        {
            Ok(result) => super::tool::tool_output(&result),
            Err(CallError::Rpc(error)) => ToolOutput::error(format!(
                "the MCP server `{name}` refused the call: {} (error {})",
                crate::prompt_text::escape_hidden(&error.message),
                error.code
            )),
            Err(CallError::Timeout(timeout)) => ToolOutput::error(format!(
                "no answer from the MCP server `{name}` within {}; the call was cancelled",
                seconds(timeout)
            )),
            Err(CallError::Closed(reason)) => stopped(name, &reason),
            Err(CallError::Failed(reason)) => ToolOutput::error(format!(
                "the call to the MCP server `{name}` failed: {reason}"
            )),
        }
    }

    /// Stops the process, if one runs, or ends the HTTP session.
    pub async fn shutdown(&self) {
        let connection = self.state.lock().unwrap().connection.clone();
        if let Some(connection) = connection {
            connection.shutdown().await;
        }
    }
}

fn stopped(name: &str, reason: &str) -> ToolOutput {
    ToolOutput::error(format!(
        "the MCP server `{name}` stopped ({reason}); its tools fail until junees starts it \
         again in the next session"
    ))
}

/// `120 s`, or `0.5 s` below a whole second.
pub fn seconds(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

enum Handshake {
    ExitedOnProbe,
    Failed(String),
}

/// The error, with the server process's last stderr line if it left one.
fn explain(link: &Link, err: String) -> String {
    match link.stderr_line() {
        Some(line) => format!("{err} (its stderr: {line})"),
        None => err,
    }
}

pub(super) fn describe(err: CallError) -> String {
    match err {
        CallError::Rpc(error) => format!(
            "error {}: {}",
            error.code,
            crate::prompt_text::escape_hidden(&error.message)
        ),
        CallError::Timeout(timeout) => format!("no answer within {}", seconds(timeout)),
        CallError::Closed(reason) | CallError::Failed(reason) => reason,
    }
}

pub(super) fn unsupported(error: &RpcError) -> String {
    let supported = error
        .data
        .as_ref()
        .and_then(|data| data.get("supported"))
        .map_or_else(|| "?".into(), Value::to_string);
    format!(
        "it does not speak MCP {MODERN_VERSION} (it supports {}), and a modern server must \
         not be asked to fall back to `initialize`",
        crate::prompt_text::escape_hidden(&supported)
    )
}

/// The era and capabilities of a `server/discover` result.
pub(super) fn modern(result: &Value) -> Result<(Era, Value), String> {
    let versions = result
        .get("supportedVersions")
        .and_then(Value::as_array)
        .ok_or("its `server/discover` result has no `supportedVersions`")?;
    if !versions.iter().any(|version| version == MODERN_VERSION) {
        return Err(format!(
            "it does not speak MCP {MODERN_VERSION} (it supports {})",
            crate::prompt_text::escape_hidden(&Value::Array(versions.clone()).to_string())
        ));
    }
    let capabilities = result.get("capabilities").cloned().unwrap_or(json!({}));
    Ok((Era::Modern(MODERN_VERSION.into()), capabilities))
}

/// The legacy handshake. With a `probe` still waiting, a late discovery
/// result wins over `initialize`.
async fn legacy(
    connection: &Connection,
    probe: Option<super::conn::Call<'_>>,
) -> Result<(Era, Value), String> {
    let mut initialize = connection.start("initialize", initialize_params(), false);
    let result = match probe {
        None => initialize.wait().await,
        Some(mut probe) => {
            let mut probe_open = true;
            loop {
                tokio::select! {
                    answer = probe.wait(), if probe_open => match answer {
                        Ok(result) => return modern(&result),
                        Err(CallError::Rpc(error)) if error.code == UNSUPPORTED_VERSION => {
                            return Err(unsupported(&error));
                        }
                        Err(_) => probe_open = false,
                    },
                    answer = initialize.wait() => break answer,
                }
            }
        }
    }
    .map_err(|err| format!("`initialize` failed: {}", describe(err)))?;
    let (era, capabilities) = legacy_result(&result)?;
    connection.notify("notifications/initialized", json!({}));
    Ok((era, capabilities))
}

/// The params of `initialize`: the newest legacy version.
pub(super) fn initialize_params() -> Value {
    json!({
        "protocolVersion": LEGACY_VERSIONS[0],
        "capabilities": {},
        "clientInfo": client_info(),
    })
}

/// The era and capabilities of an `initialize` result, if Junees speaks
/// its version.
pub(super) fn legacy_result(result: &Value) -> Result<(Era, Value), String> {
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(version) = LEGACY_VERSIONS.iter().find(|known| **known == version) else {
        return Err(format!(
            "it answered `initialize` with protocol version `{}`, which junees does not speak \
             (it speaks {MODERN_VERSION} and {})",
            crate::prompt_text::escape_hidden(version),
            LEGACY_VERSIONS.join(", ")
        ));
    };
    let capabilities = result.get("capabilities").cloned().unwrap_or(json!({}));
    Ok((Era::Legacy((*version).to_string()), capabilities))
}

/// Every page of `tools/list`, and notes about tools it skipped.
async fn list_tools(link: &Link, era: &Era) -> Result<(Vec<RemoteTool>, Vec<String>), String> {
    // The binding: an HTTP client leaves out tools whose `x-mcp-header`
    // marks break its rules.
    let check_headers = matches!((link, era), (Link::Http(_), Era::Modern(_)));
    let mut tools: Vec<RemoteTool> = Vec::new();
    let mut notes = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = Vec::new();
    for _ in 0..MAX_PAGES {
        let mut params = json!({});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let result = link
            .request(
                "tools/list",
                era.params(params),
                Duration::from_secs(3600),
                &[],
            )
            .await
            .map_err(|err| format!("`tools/list` failed: {}", describe(err)))?;
        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or("its `tools/list` result has no `tools` list")?;
        for tool in listed {
            match remote_tool(tool) {
                Ok(tool) if tools.iter().any(|known| known.name == tool.name) => {
                    notes.push(format!(
                        "skipped a second tool named `{}`",
                        crate::prompt_text::escape_hidden(&tool.name)
                    ))
                }
                Ok(tool) if check_headers => match meta::param_headers(&tool.input_schema) {
                    Ok(_) => tools.push(tool),
                    Err(reason) => notes.push(format!(
                        "skipped `{}`: {reason}",
                        crate::prompt_text::escape_hidden(&tool.name)
                    )),
                },
                Ok(tool) => tools.push(tool),
                Err(note) => notes.push(note),
            }
        }
        match result.get("nextCursor").and_then(Value::as_str) {
            Some(next) if seen_cursors.iter().any(|seen| seen == next) => {
                notes.push("its tool list repeats a page; stopped reading it".into());
                return Ok((tools, notes));
            }
            Some(next) => {
                seen_cursors.push(next.to_string());
                cursor = Some(next.to_string());
            }
            None => return Ok((tools, notes)),
        }
    }
    notes.push(format!(
        "read only the first {MAX_PAGES} pages of its tool list"
    ));
    Ok((tools, notes))
}

fn remote_tool(tool: &Value) -> Result<RemoteTool, String> {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or("skipped a tool without a name")?;
    let input_schema = match tool.get("inputSchema") {
        Some(schema @ Value::Object(_)) => schema.clone(),
        _ => {
            return Err(format!(
                "skipped `{}`: its `inputSchema` is not an object",
                crate::prompt_text::escape_hidden(name)
            ));
        }
    };
    Ok(RemoteTool {
        name: name.to_string(),
        description: tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        input_schema,
    })
}
