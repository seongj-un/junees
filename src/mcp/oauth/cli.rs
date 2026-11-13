//! `junees mcp login <server>` and `junees mcp logout <server>`, and the
//! terminal side of a login (also used by `/mcp login`).

use std::path::Path;

use anyhow::{Result, bail};

use super::login::{LoginUi, WAIT, login, logout};
use crate::approval::SharedLines;
use crate::mcp::config;

pub const HELP: &str = "\
junees mcp login <server>   log in to a `url` MCP server of $JUNEES_HOME/mcp.json
                            that asks for OAuth: prints an address to open in a
                            browser and waits for the answer (or a pasted address)
junees mcp logout <server>  remove the server's saved login
junees mcp serve [--dir <folder>] [--allow-write] [--allow-bash]
                            serve Junees as an MCP server on stdin and stdout, for
                            another agent (see `junees mcp serve --help`)

The tokens are kept in $JUNEES_HOME/secrets/mcp-oauth/<server>.json (0600),
sent only to that server, and renewed by themselves when the server gave a
refresh token. Scheduled jobs and webhook runs use a saved login; they never
start one. In the conversation: /mcp login <server>, /mcp logout <server>.
";

/// `junees mcp ...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    Login(String),
    Logout(String),
    /// `junees mcp serve`: Junees as an MCP server ([`crate::mcp::serve`]).
    Serve(crate::mcp::serve::Options),
    Help,
}

/// Parses the arguments after `mcp`.
pub fn parse(mut args: impl Iterator<Item = String>) -> Result<McpCommand> {
    let command = match args.next().as_deref() {
        None | Some("-h" | "--help" | "help") => return Ok(McpCommand::Help),
        Some("serve") => return crate::mcp::serve::parse(args).map(McpCommand::Serve),
        Some(word @ ("login" | "logout")) => {
            let Some(name) = args.next() else {
                bail!("usage: junees mcp {word} <server>");
            };
            if name == "-h" || name == "--help" {
                return Ok(McpCommand::Help);
            }
            config::check_name(&name)?;
            if word == "login" {
                McpCommand::Login(name)
            } else {
                McpCommand::Logout(name)
            }
        }
        Some(other) => bail!("unknown `junees mcp` command `{other}` (login, logout, serve)"),
    };
    if let Some(extra) = args.next() {
        bail!("`junees mcp` takes one server name (got `{extra}` too)");
    }
    Ok(command)
}

/// Runs `command` for the state folder `home`; the text to print.
pub async fn run(
    command: &McpCommand,
    home: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
    ui: &mut dyn LoginUi,
) -> Result<String, String> {
    let home = || {
        home.ok_or_else(|| {
            "neither JUNEES_HOME nor HOME is set, so there is no mcp.json".to_string()
        })
    };
    match command {
        McpCommand::Help => Ok(HELP.to_string()),
        McpCommand::Serve(_) => {
            Err("junees mcp serve runs on its own (crate::mcp::serve::main)".into())
        }
        McpCommand::Logout(name) => logout(home()?, name).await,
        McpCommand::Login(name) => {
            let home = home()?;
            let path = home.join(config::FILE_NAME);
            let servers = config::load(&path).map_err(|err| format!("{err:#}"))?;
            let server = servers
                .into_iter()
                .find(|server| &server.name == name)
                .ok_or_else(|| format!("{} has no server `{name}`", path.display()))?;
            login(home, &server, env, ui, WAIT)
                .await
                .map_err(|reason| format!("login to `{name}` failed: {reason}"))
        }
    }
}

/// The login on a terminal: lines go to stdout, pasted answers come from
/// `input`.
pub struct Terminal<'a> {
    pub input: &'a SharedLines,
}

#[async_trait::async_trait(?Send)]
impl LoginUi for Terminal<'_> {
    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    async fn line(&mut self) -> Option<String> {
        self.input.next_line().await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_words(words: &[&str]) -> Result<McpCommand> {
        parse(words.iter().map(|word| word.to_string()))
    }

    #[test]
    fn parses_login_and_logout() {
        assert_eq!(
            parse_words(&["login", "docs"]).unwrap(),
            McpCommand::Login("docs".into())
        );
        assert_eq!(
            parse_words(&["logout", "docs"]).unwrap(),
            McpCommand::Logout("docs".into())
        );
        assert_eq!(parse_words(&[]).unwrap(), McpCommand::Help);
        assert_eq!(parse_words(&["--help"]).unwrap(), McpCommand::Help);
        assert_eq!(parse_words(&["login", "-h"]).unwrap(), McpCommand::Help);
        assert!(parse_words(&["login"]).is_err());
        assert!(parse_words(&["login", "a/b"]).is_err());
        assert!(parse_words(&["login", "a", "b"]).is_err());
        assert!(parse_words(&["list"]).is_err());
    }
}
