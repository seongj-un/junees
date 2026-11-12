//! OAuth for `url` MCP servers (MCP 2026-07-28, `basic/authorization`;
//! the same flow in 2025-11-25 apart from the `iss` check).
//!
//! - A server that wants a login answers HTTP 401 with a `Bearer`
//!   challenge. Junees then fails that server with a hint and never starts
//!   a login by itself: only `junees mcp login <server>` and
//!   `/mcp login <server>` do ([`login`]).
//! - The login finds the authorization server through the protected
//!   resource metadata (RFC 9728) and the authorization server metadata
//!   (RFC 8414, OpenID Connect Discovery) ([`discovery`]), gets a client
//!   (a pre-registered `oauth.client_id` of mcp.json, else Dynamic Client
//!   Registration, RFC 7591; Junees hosts no Client ID Metadata Document)
//!   ([`token`]), prints the authorization address (PKCE S256, `state`,
//!   `resource` of RFC 8707) and takes the answer from a one-time loopback
//!   listener or a pasted address ([`redirect`]).
//! - The tokens are saved in `$JUNEES_HOME/secrets/mcp-oauth/<server>.json`
//!   (0600, folder 0700) ([`store`]) with the server address, resource
//!   and issuer they belong to, and sent only to that address, in
//!   `Authorization: Bearer` ([`auth`]). They are renewed with the refresh
//!   token a minute before they expire and after a 401, once.
//! - An `Authorization` header in mcp.json `headers` wins: such a server
//!   is never logged in to, and a saved login is not used for it. Its
//!   other headers are sent along with the OAuth token.
//!
//! Tokens never reach the model, tools, logs, errors or doctor output:
//! the header value is marked sensitive, nothing formats a token, and the
//! saved file is in the folder the tools may not read.

pub mod auth;
pub mod challenge;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod fetch;
pub mod login;
pub mod pkce;
pub mod redirect;
pub mod store;
pub mod token;

#[cfg(test)]
pub(crate) mod tests;

use std::path::Path;

pub use auth::Auth;
pub use config::OAuthConfig;
pub use login::{LoginUi, login, logout};

use super::config::ServerConfig;
use store::Saved;

/// What is known offline about a server's OAuth login.
pub enum LoginState {
    /// A `command` server, or one with its own `Authorization` header.
    NotUsed,
    /// No saved login.
    None,
    /// A saved login for this server's address.
    Saved(Box<Saved>),
    /// A saved login that cannot be used, and why.
    Unusable(String),
}

/// The login state of `server` in the state folder `home`.
pub fn state(home: &Path, server: &ServerConfig) -> LoginState {
    if login::oauth_target(server).is_err() {
        return LoginState::NotUsed;
    }
    match Auth::load(home, server) {
        Ok(None) => LoginState::None,
        Ok(Some(_)) => match store::read(&store::path(home, &server.name)) {
            Ok(Some(saved)) => LoginState::Saved(Box::new(saved)),
            Ok(None) => LoginState::None,
            Err(reason) => LoginState::Unusable(reason),
        },
        Err(reason) => LoginState::Unusable(reason),
    }
}

/// One phrase about the login for `/mcp`, if OAuth applies; never a
/// token.
pub fn overview(home: &Path, server: &ServerConfig) -> Option<String> {
    Some(match state(home, server) {
        LoginState::NotUsed => return None,
        LoginState::None => format!(
            "no OAuth login (`/mcp login {}` if the server asks for one)",
            server.name
        ),
        LoginState::Saved(saved) => {
            let expiry = match saved.expires_at {
                None => "no stated expiry".to_string(),
                Some(at) if at <= store::now() && saved.refresh_token.is_some() => {
                    "expired, renewed at the next request".to_string()
                }
                Some(at) if at <= store::now() => "expired".to_string(),
                Some(at) => format!("expires in {} min", (at - store::now()).div_ceil(60)),
            };
            format!(
                "OAuth login saved ({}, {expiry})",
                fetch::short(&saved.issuer, 200)
            )
        }
        LoginState::Unusable(reason) => format!("OAuth login unusable: {reason}"),
    })
}
