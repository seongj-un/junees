//! OAuth for `url` MCP servers (MCP 2026-07-28, `basic/authorization`).

pub mod challenge;
pub mod config;
pub mod discovery;
pub mod fetch;
pub mod pkce;
pub mod redirect;
pub mod store;
pub mod token;

pub use config::OAuthConfig;
