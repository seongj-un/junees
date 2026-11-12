//! The `oauth` entry of a `url` server in `mcp.json`: what Junees needs to
//! know about the login that the server cannot tell it.
//!
//! ```json
//! { "servers": { "docs": { "url": "https://mcp.example.com/mcp",
//!   "oauth": { "client_id": "junees-at-example",
//!              "client_secret": "env:DOCS_CLIENT_SECRET",
//!              "scope": "files:read", "issuer": "https://auth.example.com" } } } }
//! ```
//!
//! Every key is optional, and the entry itself too: a server that asks for
//! a login (HTTP 401 with `WWW-Authenticate`) can be logged in to without
//! it. `client_id` is a client the user registered at the authorization
//! server beforehand (MCP 2026-07-28, client-registration.md,
//! "Pre-registration"; the URL of a Client ID Metadata Document the user
//! hosts works the same way); without one Junees registers itself
//! (Dynamic Client Registration). The secret is an environment reference
//! like the header values, so the file holds none.

use anyhow::{Result, bail};
use serde::Deserialize;

use super::super::http::target::{env_reference, is_loopback};

/// The `oauth` entry. Deliberately not `Debug`, like the server entry.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct OAuthConfig {
    /// A pre-registered client id.
    pub client_id: Option<String>,
    /// The variable holding that client's secret.
    pub client_secret_var: Option<String>,
    /// The scopes to ask for, space-separated; replaces the server's
    /// choice.
    pub scope: Option<String>,
    /// The authorization server to use (its issuer identifier); the server
    /// must list it.
    pub issuer: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOAuth {
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
    issuer: Option<String>,
}

impl OAuthConfig {
    /// Checks the entry; errors never quote a value that may be a secret.
    pub fn parse(raw: RawOAuth) -> Result<Self> {
        if let Some(id) = &raw.client_id
            && (id.trim().is_empty()
                || id.len() > 1024
                || id.chars().any(|c| c.is_control() || c == ' '))
        {
            bail!("`oauth.client_id` must be 1 to 1024 characters without spaces");
        }
        let client_secret_var = match &raw.client_secret {
            None => None,
            Some(value) => match env_reference(value) {
                Some((None, var)) => Some(var.to_string()),
                _ => bail!(
                    "`oauth.client_secret` must be a reference to an environment variable \
                     (`env:NAME`), so the file holds no secret"
                ),
            },
        };
        if client_secret_var.is_some() && raw.client_id.is_none() {
            bail!("`oauth.client_secret` needs `oauth.client_id`");
        }
        if let Some(scope) = &raw.scope
            && (scope.split(' ').any(str::is_empty)
                || !scope
                    .bytes()
                    .all(|b| b == b' ' || (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\'))
        {
            bail!(
                "`oauth.scope` must be scope names separated by single spaces (RFC 6749 \
                 section 3.3)"
            );
        }
        if let Some(issuer) = &raw.issuer {
            let ok = reqwest::Url::parse(issuer).is_ok_and(|url| {
                (url.scheme() == "https" || url.scheme() == "http" && is_loopback(&url))
                    && url.query().is_none()
                    && url.fragment().is_none()
            });
            if !ok {
                bail!(
                    "`oauth.issuer` must be an https URL without query or fragment (http only \
                     to this computer)"
                );
            }
        }
        Ok(Self {
            client_id: raw.client_id,
            client_secret_var,
            scope: raw.scope,
            issuer: raw.issuer,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<OAuthConfig> {
        OAuthConfig::parse(serde_json::from_str(json)?)
    }

    #[test]
    fn checks_the_entry_without_quoting_secrets() {
        let config = parse(
            r#"{"client_id": "junees", "client_secret": "env:DOCS_SECRET",
                "scope": "files:read offline_access", "issuer": "https://auth.example.com"}"#,
        )
        .unwrap();
        assert_eq!(config.client_id.as_deref(), Some("junees"));
        assert_eq!(config.client_secret_var.as_deref(), Some("DOCS_SECRET"));
        assert_eq!(config.scope.as_deref(), Some("files:read offline_access"));
        assert!(parse("{}").unwrap() == OAuthConfig::default());

        let err = |json: &str| format!("{:#}", parse(json).err().unwrap());
        let message = err(r#"{"client_id": "a", "client_secret": "s3cret-value"}"#);
        assert!(message.contains("`env:NAME`"), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
        assert!(err(r#"{"client_secret": "env:A"}"#).contains("needs `oauth.client_id`"));
        assert!(err(r#"{"client_secret": "Bearer env:A", "client_id": "a"}"#).contains("env:"));
        assert!(err(r#"{"client_id": ""}"#).contains("client_id"));
        assert!(err(r#"{"scope": "a  b"}"#).contains("single spaces"));
        assert!(err(r#"{"scope": "a\"b"}"#).contains("single spaces"));
        assert!(err(r#"{"issuer": "http://auth.example.com"}"#).contains("https"));
        assert!(parse(r#"{"issuer": "http://127.0.0.1:9/tenant"}"#).is_ok());
        assert!(err(r#"{"issuer": "https://a.example/?x=1"}"#).contains("query"));
        assert!(err(r#"{"client": "a"}"#).contains("unknown field"));
    }
}
