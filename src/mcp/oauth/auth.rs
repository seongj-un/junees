//! A saved login in use: the `Authorization` header of every request to
//! the server, and its renewal with the refresh token shortly before it
//! expires or after the server refused it (HTTP 401), once.
//!
//! MCP 2026-07-28, authorization.md, "Access Token Usage": the token goes
//! in `Authorization: Bearer` on every HTTP request, never in the URL, and
//! only to the server it was issued for. Renewal uses only what was saved
//! at the login: it never starts one, so unattended runs (cron, webhooks)
//! work with the saved login alone.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::header::HeaderValue;

use super::discovery::resource_allows;
use super::fetch::endpoint;
use super::store::{self, Saved, now};
use super::token::{self, Client, TokenError};
use crate::mcp::config::ServerConfig;

/// Renew this long before the token expires.
const EARLY: u64 = 60;
/// After a failed early renewal, wait this long before trying again
/// early (a 401 still renews at once).
const RETRY_AFTER: Duration = Duration::from_secs(60);

pub struct Auth {
    server: String,
    home: PathBuf,
    saved: Mutex<Saved>,
    /// Held while renewing, so concurrent requests renew once.
    renewing: tokio::sync::Mutex<()>,
    failed_early: Mutex<Option<Instant>>,
}

impl Auth {
    /// The saved login of `server`, checked against its configuration:
    /// `Ok(None)` without one, for a `command` server, or when mcp.json
    /// gives the server its own `Authorization` header (that header wins).
    pub fn load(home: &Path, server: &ServerConfig) -> Result<Option<Self>, String> {
        let Some(http) = &server.http else {
            return Ok(None);
        };
        if http.has_authorization_header() {
            return Ok(None);
        }
        let Some(saved) = store::read(&store::path(home, &server.name))? else {
            return Ok(None);
        };
        // The token goes to the address it was issued for, and nowhere else.
        if saved.server_url != http.url.as_str() || !resource_allows(&saved.resource, &http.url) {
            return Err(format!(
                "its saved OAuth login is for another address (the `url` in mcp.json changed \
                 since); run `junees mcp login {}` again",
                server.name
            ));
        }
        endpoint(&saved.token_endpoint, "the saved token endpoint")?;
        Ok(Some(Self {
            server: server.name.clone(),
            home: home.to_path_buf(),
            saved: Mutex::new(saved),
            renewing: tokio::sync::Mutex::new(()),
            failed_early: Mutex::new(None),
        }))
    }

    /// `Bearer <access token>`, marked sensitive.
    pub fn header(&self) -> HeaderValue {
        let saved = self.saved.lock().unwrap();
        bearer(&saved.access_token)
    }

    /// The header to send now: renewed first when the token expires
    /// within a minute and can be renewed. A failed renewal sends the old
    /// token (the server decides).
    pub async fn ready(&self) -> HeaderValue {
        let (header, expiring) = {
            let saved = self.saved.lock().unwrap();
            (
                bearer(&saved.access_token),
                saved.refresh_token.is_some()
                    && saved.expires_at.is_some_and(|at| now() + EARLY >= at),
            )
        };
        let recently_failed = self
            .failed_early
            .lock()
            .unwrap()
            .is_some_and(|at| at.elapsed() < RETRY_AFTER);
        if !expiring || recently_failed {
            return header;
        }
        match self.renew(&header).await {
            Ok(()) => self.header(),
            Err(_) => {
                *self.failed_early.lock().unwrap() = Some(Instant::now());
                header
            }
        }
    }

    /// Renews the token after the server refused `rejected` (or it
    /// expires). Nothing to do when another request renewed it meanwhile,
    /// in this process or another one.
    pub async fn renew(&self, rejected: &HeaderValue) -> Result<(), String> {
        let _turn = self.renewing.lock().await;
        if self.header() != rejected {
            return Ok(());
        }
        let home = self.home.clone();
        let _lock = tokio::task::spawn_blocking(move || store::lock(&home))
            .await
            .map_err(|_| "the renewal stopped".to_string())??;
        let current = self.saved.lock().unwrap().clone();
        if let Ok(Some(on_disk)) = store::read(&store::path(&self.home, &self.server))
            && on_disk.access_token != current.access_token
            && on_disk.server_url == current.server_url
            && on_disk.resource == current.resource
        {
            *self.saved.lock().unwrap() = on_disk;
            return Ok(());
        }
        let Some(refresh_token) = current.refresh_token.clone() else {
            return Err("the saved login has no refresh token to renew it with".into());
        };
        let token_endpoint = endpoint(&current.token_endpoint, "the saved token endpoint")?;
        let client = Client {
            id: current.client_id.clone(),
            secret: current.client_secret.clone(),
            auth: current.client_auth.clone(),
        };
        let issued = token::refresh(&token_endpoint, &client, &refresh_token, &current.resource)
            .await
            .map_err(|err| match err {
                TokenError::InvalidGrant(text) => {
                    format!("{text}: the saved login is no longer valid")
                }
                TokenError::Other(text) => text,
            })?;
        let saved_at = now();
        let renewed = Saved {
            access_token: issued.access_token,
            // RFC 6749 section 6: without a new one the old one stays.
            refresh_token: issued.refresh_token.or(current.refresh_token.clone()),
            expires_at: issued.expires_in.map(|secs| saved_at.saturating_add(secs)),
            scope: issued.scope.or(current.scope.clone()),
            saved_at,
            ..current
        };
        store::write(&self.home, &self.server, &renewed)?;
        *self.saved.lock().unwrap() = renewed;
        *self.failed_early.lock().unwrap() = None;
        Ok(())
    }
}

impl Auth {
    /// Remembers `scope`, which the server asked for (HTTP 403
    /// `insufficient_scope`), so the next login asks for it along with the
    /// granted scopes (MCP 2026-07-28, authorization.md, "Step-Up
    /// Authorization Flow").
    pub async fn want_scope(&self, scope: &str) -> Result<(), String> {
        let _turn = self.renewing.lock().await;
        let home = self.home.clone();
        let _lock = tokio::task::spawn_blocking(move || store::lock(&home))
            .await
            .map_err(|_| "the update stopped".to_string())??;
        let path = store::path(&self.home, &self.server);
        let mut saved = match store::read(&path)? {
            Some(saved) if saved.server_url == self.saved.lock().unwrap().server_url => saved,
            _ => return Ok(()),
        };
        let mut wanted: Vec<String> = saved
            .wanted_scope
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let granted = saved.scope.clone().unwrap_or_default();
        let mut changed = false;
        for item in scope.split_whitespace() {
            if !wanted.iter().any(|known| known == item)
                && !granted.split_whitespace().any(|known| known == item)
            {
                wanted.push(item.to_string());
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        saved.wanted_scope = Some(wanted.join(" "));
        store::write(&self.home, &self.server, &saved)?;
        *self.saved.lock().unwrap() = saved;
        Ok(())
    }
}

fn bearer(token: &str) -> HeaderValue {
    // Checked visible ASCII when issued; a broken file sends nothing usable.
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer invalid"));
    value.set_sensitive(true);
    value
}
