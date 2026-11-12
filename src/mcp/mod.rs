//! MCP client: Junees starts the servers the user lists in
//! `$JUNEES_HOME/mcp.json` ([`config`]) as child processes (stdio), or
//! connects to them over Streamable HTTP ([`http`]), when a session
//! starts, and offers their tools to the model as `mcp__<server>__<tool>`.
//!
//! Protocol: MCP 2026-07-28 (stdio transport, `server/discover`,
//! per-request `_meta`), with the legacy `initialize` handshake of
//! 2025-11-25 and earlier as the fallback the specification describes for
//! dual-era clients ([`server`]). Only tools are used: no resources,
//! prompts, sampling or elicitation.

pub mod config;
pub mod conn;
pub mod http;
pub mod link;
pub mod oauth;
pub mod server;
pub mod tool;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::approval::ExternalTool;
use crate::tools::ToolRegistry;

pub use config::ServerConfig;
pub use server::{EnvLookup, Server, Status, Timeouts};
pub use tool::McpTool;

/// The MCP servers of a session and the tools they offer. Dropping it
/// kills the servers that still run; [`Mcp::shutdown`] ends them gently.
#[derive(Default)]
pub struct Mcp {
    /// Where the server list is read from, if there is a state folder.
    path: Option<PathBuf>,
    servers: Vec<Arc<Server>>,
    /// The tools, under their exposed names, in server and list order.
    tools: Vec<(String, usize, server::RemoteTool)>,
    /// Why the server list could not be read.
    problem: Option<String>,
    /// The state folder of the saved OAuth logins, if they are used.
    home: Option<PathBuf>,
}

impl Mcp {
    /// No servers.
    pub fn none() -> Self {
        Self::default()
    }

    /// Reads `<home>/mcp.json` and starts its enabled servers (all at once)
    /// with `hidden_env` hidden from them, as [`Mcp::start_servers`] does.
    /// Never fails: a broken file is kept as a problem to report.
    pub async fn start(home: Option<&Path>, hidden_env: &[String], workdir: &Path) -> Self {
        let Some(home) = home else {
            return Self::none();
        };
        let path = home.join(config::FILE_NAME);
        match config::load(&path) {
            Ok(configs) => {
                let env: EnvLookup = Arc::new(|name: &str| std::env::var(name).ok());
                let mut mcp = Self::start_in(
                    configs,
                    hidden_env,
                    workdir,
                    Timeouts::default(),
                    env,
                    Some(home),
                )
                .await;
                mcp.path = Some(path);
                mcp
            }
            Err(err) => Self {
                path: Some(path),
                problem: Some(format!("{err:#}")),
                ..Self::default()
            },
        }
    }

    /// Starts the enabled servers of `configs` and names their tools.
    /// `workdir` is the folder Junees works in: servers do not run there
    /// ([`ServerConfig::check_workdir`]).
    pub async fn start_servers(
        configs: Vec<ServerConfig>,
        hidden_env: &[String],
        workdir: &Path,
        timeouts: Timeouts,
    ) -> Self {
        let env: EnvLookup = Arc::new(|name: &str| std::env::var(name).ok());
        Self::start_servers_with_env(configs, hidden_env, workdir, timeouts, env).await
    }

    /// [`Mcp::start_servers`] with the HTTP header values read through
    /// `env`.
    pub async fn start_servers_with_env(
        configs: Vec<ServerConfig>,
        hidden_env: &[String],
        workdir: &Path,
        timeouts: Timeouts,
        env: EnvLookup,
    ) -> Self {
        Self::start_in(configs, hidden_env, workdir, timeouts, env, None).await
    }

    /// [`Mcp::start_servers_with_env`] with the saved OAuth logins of the
    /// state folder `home` ([`oauth`]); they are never started here.
    pub async fn start_in(
        configs: Vec<ServerConfig>,
        hidden_env: &[String],
        workdir: &Path,
        timeouts: Timeouts,
        env: EnvLookup,
        home: Option<&Path>,
    ) -> Self {
        let servers: Vec<Arc<Server>> = configs
            .into_iter()
            .map(|config| {
                Arc::new(
                    Server::new(config, hidden_env.to_vec(), workdir.to_path_buf(), timeouts)
                        .with_env(Arc::clone(&env))
                        .with_oauth_home(home.map(Path::to_path_buf)),
                )
            })
            .collect();
        let starts: Vec<_> = servers
            .iter()
            .filter(|server| server.config.enabled)
            .map(|server| {
                let server = Arc::clone(server);
                tokio::spawn(async move {
                    let _ = server.start().await;
                })
            })
            .collect();
        for start in starts {
            let _ = start.await;
        }
        let mut taken = HashSet::new();
        let mut tools = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            for remote in server.tools() {
                let name = tool::exposed_name(server.name(), &remote.name, &mut taken);
                tools.push((name, index, remote));
            }
        }
        Self {
            path: None,
            servers,
            tools,
            problem: None,
            home: home.map(Path::to_path_buf),
        }
    }

    pub fn servers(&self) -> &[Arc<Server>] {
        &self.servers
    }

    /// The exposed tool names, in order.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|(name, _, _)| name.clone()).collect()
    }

    /// Adds the tools of the servers that started to `registry`.
    pub fn register(&self, registry: &mut ToolRegistry) {
        for (name, index, remote) in &self.tools {
            registry.register(McpTool::new(
                name.clone(),
                remote,
                Arc::clone(&self.servers[*index]),
            ));
        }
    }

    /// The tools for the approval questions ([`crate::approval::Approval::with_external_tools`]).
    pub fn external_tools(&self) -> Vec<(String, ExternalTool)> {
        self.tools
            .iter()
            .map(|(name, index, remote)| {
                let server = &self.servers[*index];
                (
                    name.clone(),
                    ExternalTool {
                        kind: "MCP server",
                        server: server.name().to_string(),
                        tool: remote.name.clone(),
                        trusted: server.config.trust,
                    },
                )
            })
            .collect()
    }

    /// One line per server for the start of a session, and the problem
    /// with the file if there is one; empty without servers.
    pub fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(problem) = &self.problem {
            lines.push(format!("mcp: servers not started: {problem}"));
        }
        for server in &self.servers {
            match server.status() {
                Status::Disabled => {}
                Status::Running { tools, .. } => lines.push(format!(
                    "mcp: {} started ({tools} {}{})",
                    server.name(),
                    if tools == 1 { "tool" } else { "tools" },
                    if server.config.http.is_some() {
                        ", over HTTP"
                    } else {
                        ""
                    }
                )),
                Status::Failed(reason) | Status::Stopped(reason) => {
                    lines.push(format!("mcp: {} failed to start: {reason}", server.name()));
                }
            }
            for note in server.notes() {
                lines.push(format!("mcp: {}: {note}", server.name()));
            }
        }
        lines
    }

    /// What `/mcp` shows: each server's state, transport and tool count.
    /// Never the arguments, `env` or header values, or more of a URL than
    /// its origin: they may hold tokens.
    pub fn overview(&self) -> String {
        let mut text = String::new();
        if let Some(problem) = &self.problem {
            text.push_str(&format!("servers not started: {problem}\n"));
        }
        if self.servers.is_empty() {
            if self.problem.is_none() {
                text.push_str(&match &self.path {
                    Some(path) => format!("(no MCP servers in {})\n", path.display()),
                    None => "(no MCP servers: neither JUNEES_HOME nor HOME is set)\n".into(),
                });
            }
            return text;
        }
        for server in &self.servers {
            let exposed = self
                .tools
                .iter()
                .filter(|(_, index, _)| Arc::ptr_eq(&self.servers[*index], server))
                .count();
            let state = match server.status() {
                Status::Disabled => "disabled".to_string(),
                Status::Running { version, tools } => {
                    format!("running, MCP {version}, {tools} tools")
                }
                Status::Failed(reason) => format!("not running: {reason}"),
                Status::Stopped(reason) => format!("stopped: {reason}"),
            };
            let env: Vec<&str> = server.config.env.keys().map(String::as_str).collect();
            let login = self
                .home
                .as_deref()
                .and_then(|home| oauth::overview(home, &server.config))
                .map(|login| format!(", {login}"))
                .unwrap_or_default();
            let place = match &server.config.http {
                Some(http) => format!(
                    "Streamable HTTP `{}`{}{login}",
                    http.origin(),
                    if http.headers.is_empty() {
                        String::new()
                    } else {
                        format!(", headers {}", http.describe_headers())
                    }
                ),
                None => format!(
                    "stdio, command `{}`{}",
                    crate::prompt_text::escape_hidden(&server.config.command),
                    if env.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", env {}",
                            crate::prompt_text::escape_hidden(&env.join(", "))
                        )
                    }
                ),
            };
            text.push_str(&format!(
                "  {}: {state}\n     {place}; {exposed} tools offered to the model{}{}\n",
                server.name(),
                if server.config.trust {
                    ", trusted (no approval questions)"
                } else {
                    ""
                },
                if server.config.call_timeout == config::DEFAULT_CALL_TIMEOUT {
                    String::new()
                } else {
                    format!(
                        ", calls time out after {} s",
                        server.config.call_timeout.as_secs()
                    )
                }
            ));
        }
        text
    }

    /// Starts the enabled servers that are not running again (for a new
    /// session). Their tools keep the names they had; tools a server did
    /// not offer when the process started stay unknown until the next run.
    /// Returns a line per server it tried.
    pub async fn restart_stopped(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for server in &self.servers {
            if !server.config.enabled || !server.needs_start() {
                continue;
            }
            let had_tools = self
                .tools
                .iter()
                .any(|(_, index, _)| Arc::ptr_eq(&self.servers[*index], server));
            lines.push(match server.start().await {
                Ok(()) if had_tools => format!("mcp: {} started again", server.name()),
                Ok(()) => format!(
                    "mcp: {} started; its tools reach the model from the next junees run",
                    server.name()
                ),
                Err(reason) => format!("mcp: {} failed to start: {reason}", server.name()),
            });
        }
        lines
    }

    /// The server named `name`, if the list has one.
    pub fn server(&self, name: &str) -> Option<&Arc<Server>> {
        self.servers.iter().find(|server| server.name() == name)
    }

    /// Starts the server `name` again (after a login); a line saying how
    /// it went.
    pub async fn restart(&self, name: &str) -> Option<String> {
        let server = self.server(name)?;
        if !server.config.enabled {
            return Some(format!("mcp: {name} is disabled in mcp.json"));
        }
        let had_tools = self
            .tools
            .iter()
            .any(|(_, index, _)| Arc::ptr_eq(&self.servers[*index], server));
        Some(match server.start().await {
            Ok(()) if had_tools => format!("mcp: {name} started again"),
            Ok(()) => {
                format!("mcp: {name} started; its tools reach the model from the next junees run")
            }
            Err(reason) => format!("mcp: {name} failed to start: {reason}"),
        })
    }

    /// Ends every server: closes its input, then SIGTERM and SIGKILL to
    /// its process group if it does not exit.
    pub async fn shutdown(&self) {
        let ends: Vec<_> = self
            .servers
            .iter()
            .map(|server| {
                let server = Arc::clone(server);
                tokio::spawn(async move { server.shutdown().await })
            })
            .collect();
        for end in ends {
            let _ = end.await;
        }
    }
}
