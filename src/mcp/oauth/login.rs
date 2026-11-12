//! The login: discovery, the client, PKCE, the browser and the code
//! exchange, then the saved tokens. Only `junees mcp login <server>` and
//! `/mcp login <server>` start one; nothing else does (unattended runs
//! use the saved login only).
//!
//! MCP 2026-07-28, authorization.md, "Authorization Flow Steps" and
//! "Authorization Response Validation" (RFC 9207 `iss`, checked before
//! the code goes anywhere), "Scope Selection Strategy" and "Step-Up
//! Authorization Flow" (the previously granted scopes are asked for
//! again). Junees prints the address to open and never starts a browser.

use std::path::Path;
use std::time::Duration;

use reqwest::Url;

use super::discovery::{AuthServer, Discovery, discover};
use super::fetch;
use super::pkce::{Pkce, random_text};
use super::redirect::{self, Listener, Params};
use super::store::{self, FORMAT, Saved, now};
use super::token::{self, Client};
use crate::mcp::config::ServerConfig;
use crate::mcp::http::HttpConfig;

/// How long the login waits for the browser's answer.
pub const WAIT: Duration = Duration::from_secs(300);

/// Where the login talks to the user.
#[async_trait::async_trait(?Send)]
pub trait LoginUi {
    /// Shows a line.
    fn say(&mut self, text: &str);
    /// The next line the user typed; `None` at the end of input.
    async fn line(&mut self) -> Option<String>;
}

/// The `url` server's HTTP settings, if OAuth can be used with it.
pub fn oauth_target(server: &ServerConfig) -> Result<&HttpConfig, String> {
    let name = &server.name;
    let http = server.http.as_ref().ok_or_else(|| {
        format!(
            "`{name}` is a `command` server; OAuth logins are for `url` servers (a command \
             server takes its credentials from `env`)"
        )
    })?;
    if http.has_authorization_header() {
        return Err(format!(
            "`{name}` sends its own `Authorization` header (`headers` in mcp.json), which wins \
             over an OAuth login; remove that header to log in"
        ));
    }
    Ok(http)
}

/// Logs in to `server` and saves the tokens under `home`; returns what to
/// tell the user (never a token). `env` reads `oauth.client_secret` and
/// the header variables; `wait` bounds the wait for the browser.
pub async fn login(
    home: &Path,
    server: &ServerConfig,
    env: &dyn Fn(&str) -> Option<String>,
    ui: &mut dyn LoginUi,
    wait: Duration,
) -> Result<String, String> {
    let name = &server.name;
    let http = oauth_target(server)?;
    let settings = http.oauth.clone().unwrap_or_default();
    let client_secret = match &settings.client_secret_var {
        None => None,
        Some(var) => Some(
            env(var)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!(
                        "`oauth.client_secret` takes its value from the variable {var}, which \
                         is not set"
                    )
                })?,
        ),
    };
    ui.say(&format!(
        "Looking up the authorization server of `{name}` ({})...",
        http.origin()
    ));
    let discovery = discover(http, env, settings.issuer.as_deref()).await?;
    let auth_server = &discovery.server;
    // A broken earlier file only loses its scopes.
    let previous = store::read(&store::path(home, name))
        .ok()
        .flatten()
        .filter(|saved| saved.issuer == auth_server.issuer)
        .map(|saved| {
            [saved.scope, saved.wanted_scope]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ")
        });
    let scope = choose_scope(settings.scope.as_deref(), &discovery, previous.as_deref());
    let listener = Listener::bind().await?;
    let client = match settings.client_id {
        Some(id) => Client::configured(id, client_secret, &auth_server.auth_methods),
        None => match &auth_server.registration_endpoint {
            Some(endpoint) => {
                token::register(endpoint, listener.redirect_uri(), scope.as_deref()).await?
            }
            None => {
                return Err(format!(
                    "the authorization server `{}` offers no client registration; register a \
                     client there yourself (redirect URI http://127.0.0.1/callback, any port) \
                     and put its id in `oauth.client_id` of `{name}` in mcp.json",
                    fetch::short(&auth_server.issuer, 200)
                ));
            }
        },
    };
    let pkce = Pkce::new()?;
    let state = random_text(32)?;
    let address = authorization_url(
        &auth_server.authorization_endpoint,
        &client.id,
        listener.redirect_uri(),
        scope.as_deref(),
        &state,
        &pkce.challenge,
        &discovery.resource,
    );
    ui.say(&format!(
        "Open this address in a browser and allow the access:\n\n  {address}\n"
    ));
    ui.say(&format!(
        "Waiting up to {} minutes for the browser to come back to {}. If the browser runs on \
         another computer, it ends on a page that does not load: copy that page's whole \
         address from the address bar and paste it here.",
        wait.as_secs().div_ceil(60),
        listener.redirect_uri()
    ));
    let redirect_uri = listener.redirect_uri().to_string();
    let params = answer(&listener, &state, ui, wait).await?;
    drop(listener);
    check_answer(&params, auth_server)?;
    let code = params
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or("the authorization server's answer has no `code`")?;
    let issued = token::exchange_code(
        &auth_server.token_endpoint,
        &client,
        code,
        &pkce.verifier,
        &redirect_uri,
        &discovery.resource,
    )
    .await
    .map_err(token::TokenError::text)?;
    let saved_at = now();
    let saved = Saved {
        format: FORMAT,
        server_url: http.url.as_str().to_string(),
        resource: discovery.resource.clone(),
        issuer: auth_server.issuer.clone(),
        token_endpoint: auth_server.token_endpoint.as_str().to_string(),
        client_id: client.id,
        client_secret: client.secret,
        client_auth: client.auth,
        access_token: issued.access_token,
        refresh_token: issued.refresh_token,
        expires_at: issued.expires_in.map(|secs| saved_at.saturating_add(secs)),
        scope: issued.scope.or(scope),
        wanted_scope: None,
        saved_at,
    };
    // Under the lock: a refresh running in another junees then sees this
    // login and keeps away from it.
    let lock = store::lock_async(home).await?;
    store::write(home, name, &saved)?;
    drop(lock);
    Ok(format!(
        "Logged in to `{name}` (authorization server {}). {}",
        fetch::short(&saved.issuer, 200),
        describe(&saved, saved_at)
    ))
}

/// Waits for the listener or a pasted answer with `state`, up to `wait`.
async fn answer(
    listener: &Listener,
    state: &str,
    ui: &mut dyn LoginUi,
    wait: Duration,
) -> Result<Params, String> {
    let browser = listener.wait(state);
    tokio::pin!(browser);
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    let mut input_open = true;
    loop {
        let line = tokio::select! {
            params = &mut browser => return Ok(params),
            _ = &mut deadline => {
                return Err(format!(
                    "no answer from the browser within {} minutes; run the login again",
                    wait.as_secs().div_ceil(60)
                ));
            }
            line = ui.line(), if input_open => line,
        };
        let Some(text) = line else {
            input_open = false;
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        match redirect::pasted(&text, listener.redirect_uri()) {
            Ok(params) if params.get("state") == Some(state) => return Ok(params),
            Ok(_) => ui.say(
                "That answer belongs to another login (its `state` differs), so it is ignored; \
                 paste the address of this login's answer.",
            ),
            Err(message) => ui.say(&message),
        }
    }
}

/// RFC 9207 section 2.4 as MCP 2026-07-28 tabulates it, then the error
/// of the answer. On an `iss` mismatch the answer's error is not shown.
fn check_answer(params: &Params, server: &AuthServer) -> Result<(), String> {
    match params.get("iss") {
        Some(iss) if iss != server.issuer => {
            return Err(format!(
                "the answer names another authorization server (`iss` is `{}`, expected `{}`); \
                 it was refused and its code not used",
                fetch::short(iss, 200),
                fetch::short(&server.issuer, 200)
            ));
        }
        None if server.iss_parameter_supported => {
            return Err(
                "the answer has no `iss`, though the authorization server announces it; it \
                 was refused and its code not used"
                    .into(),
            );
        }
        _ => {}
    }
    if let Some(error) = params.get("error") {
        let description = params
            .get("error_description")
            .map(|text| format!(": {}", fetch::short(text, 300)))
            .unwrap_or_default();
        return Err(format!(
            "the authorization server did not grant the access (`{}`{description})",
            fetch::short(error, 80)
        ));
    }
    Ok(())
}

/// The scopes to ask for: the user's `oauth.scope`, else the challenge's,
/// else the resource's `scopes_supported`; with the scopes granted before
/// (step-up keeps them) and `offline_access` when the authorization
/// server lists it (for a refresh token). `None` leaves `scope` out.
fn choose_scope(
    configured: Option<&str>,
    discovery: &Discovery,
    previous: Option<&str>,
) -> Option<String> {
    let mut scopes: Vec<String> = match (configured, &discovery.challenge_scope) {
        (Some(scope), _) => split(scope),
        (None, Some(scope)) => split(scope),
        (None, None) => discovery.resource_scopes.clone(),
    };
    if configured.is_none() {
        for scope in previous.map(split).unwrap_or_default() {
            if !scopes.contains(&scope) {
                scopes.push(scope);
            }
        }
        let offline = "offline_access".to_string();
        if !scopes.is_empty()
            && !scopes.contains(&offline)
            && discovery.server.scopes_supported.contains(&offline)
        {
            scopes.push(offline);
        }
    }
    (!scopes.is_empty()).then(|| scopes.join(" "))
}

fn split(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_string).collect()
}

/// The authorization request (OAuth 2.1 section 4.1.1, RFC 8707).
fn authorization_url(
    endpoint: &Url,
    client_id: &str,
    redirect_uri: &str,
    scope: Option<&str>,
    state: &str,
    challenge: &str,
    resource: &str,
) -> Url {
    let mut url = endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri);
        if let Some(scope) = scope {
            query.append_pair("scope", scope);
        }
        query
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", resource);
    }
    url
}

/// How long the saved token lasts and whether it renews, for messages.
pub fn describe(saved: &Saved, now: u64) -> String {
    let lasts = match saved.expires_at {
        None => "The access token has no stated expiry".to_string(),
        Some(at) if at <= now => "The access token has expired".to_string(),
        Some(at) => format!("The access token is valid for {}", span(at - now)),
    };
    let renews = if saved.refresh_token.is_some() {
        "; junees renews it by itself"
    } else {
        "; it cannot be renewed (no refresh token), so log in again when it expires"
    };
    let scope = saved
        .scope
        .as_deref()
        .map(|scope| format!(" Scope: {}.", fetch::short(scope, 300)))
        .unwrap_or_default();
    format!("{lasts}{renews}.{scope}")
}

/// `45 s`, `12 min`, `3 h`, `2 days`.
fn span(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        3600..172_800 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    }
}

/// Removes the saved login of the server `name`, under the lock, so a
/// refresh running meanwhile cannot write it back.
pub async fn logout(home: &Path, name: &str) -> Result<String, String> {
    crate::mcp::config::check_name(name).map_err(|err| format!("{err:#}"))?;
    if !store::path(home, name).exists() {
        return Ok(format!("`{name}` has no saved login."));
    }
    let _lock = store::lock_async(home).await?;
    Ok(if store::remove(home, name)? {
        format!(
            "Logged out of `{name}`: its saved login is removed. The authorization server may \
             still list the access; revoke it there if needed."
        )
    } else {
        format!("`{name}` has no saved login.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::oauth::discovery::validate;
    use serde_json::json;

    fn discovery(challenge: Option<&str>, resource: &[&str], server: &[&str]) -> Discovery {
        let metadata = json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": server,
        });
        Discovery {
            resource: "https://mcp.example.com/mcp".into(),
            resource_scopes: resource.iter().map(|s| s.to_string()).collect(),
            challenge_scope: challenge.map(str::to_string),
            server: validate(
                "https://auth.example.com",
                metadata.as_object().unwrap(),
                &Url::parse("https://mcp.example.com/mcp").unwrap(),
            )
            .ok()
            .unwrap(),
        }
    }

    #[test]
    fn scopes_follow_the_selection_strategy() {
        let d = discovery(Some("files:read"), &["a", "b"], &[]);
        assert_eq!(choose_scope(None, &d, None).as_deref(), Some("files:read"));
        let d = discovery(None, &["a", "b"], &[]);
        assert_eq!(choose_scope(None, &d, None).as_deref(), Some("a b"));
        let d = discovery(None, &[], &[]);
        assert_eq!(choose_scope(None, &d, None), None);
        // Step-up keeps what was granted.
        let d = discovery(Some("files:write"), &[], &[]);
        assert_eq!(
            choose_scope(None, &d, Some("files:read files:write")).as_deref(),
            Some("files:write files:read")
        );
        // offline_access when the server lists it.
        let d = discovery(Some("x"), &[], &["x", "offline_access"]);
        assert_eq!(
            choose_scope(None, &d, None).as_deref(),
            Some("x offline_access")
        );
        // The user's choice stands alone.
        assert_eq!(
            choose_scope(Some("mine"), &d, Some("old")).as_deref(),
            Some("mine")
        );
    }

    #[test]
    fn the_authorization_request_carries_pkce_state_and_resource() {
        let endpoint = Url::parse("https://auth.example.com/authorize?tenant=t1").unwrap();
        let url = authorization_url(
            &endpoint,
            "client 1",
            "http://127.0.0.1:5555/callback",
            Some("a b"),
            "st",
            "ch",
            "https://mcp.example.com/mcp",
        );
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let get = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("tenant"), Some("t1"));
        assert_eq!(get("response_type"), Some("code"));
        assert_eq!(get("client_id"), Some("client 1"));
        assert_eq!(get("scope"), Some("a b"));
        assert_eq!(get("code_challenge_method"), Some("S256"));
        assert_eq!(get("resource"), Some("https://mcp.example.com/mcp"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1:5555/callback"));
    }

    #[test]
    fn iss_is_checked_as_the_specification_tabulates() {
        let mut server = discovery(None, &[], &[]).server;
        let params = |query: &str| redirect::pasted(&format!("?{query}"), "x").ok().unwrap();
        let ok = |query: &str, server: &AuthServer| check_answer(&params(query), server);
        // Not announced: absent passes, present is compared.
        assert!(ok("state=s&code=c", &server).is_ok());
        assert!(ok("state=s&code=c&iss=https%3A%2F%2Fauth.example.com", &server).is_ok());
        let message = ok("state=s&code=c&iss=https%3A%2F%2Fevil.example", &server)
            .err()
            .unwrap();
        assert!(
            message.contains("another authorization server"),
            "{message}"
        );
        // No normalization: a trailing slash is another issuer.
        assert!(
            ok(
                "state=s&code=c&iss=https%3A%2F%2Fauth.example.com%2F",
                &server
            )
            .is_err()
        );
        // Announced: absent is refused.
        server.iss_parameter_supported = true;
        assert!(
            ok("state=s&code=c", &server)
                .err()
                .unwrap()
                .contains("no `iss`")
        );
        // A mismatched error answer is not shown.
        let message = ok(
            "state=s&error=access_denied&error_description=PHISH&iss=https%3A%2F%2Fevil.example",
            &server,
        )
        .err()
        .unwrap();
        assert!(!message.contains("PHISH"), "{message}");
        let message = ok(
            "state=s&error=access_denied&error_description=no%1b&iss=https%3A%2F%2Fauth.example.com",
            &server,
        )
        .err()
        .unwrap();
        assert!(message.contains("`access_denied`: no\\u{1b}"), "{message}");
    }

    #[test]
    fn descriptions_name_no_token() {
        let mut saved = store::sample();
        saved.expires_at = Some(1000 + 7200);
        let text = describe(&saved, 1000);
        assert!(
            text.contains("valid for 2 h; junees renews it by itself"),
            "{text}"
        );
        assert!(text.contains("Scope: files:read."), "{text}");
        assert!(!text.contains("s3cret"), "{text}");
        saved.refresh_token = None;
        saved.expires_at = Some(10);
        assert!(describe(&saved, 1000).contains("has expired; it cannot be renewed"));
        saved.expires_at = None;
        assert!(describe(&saved, 1000).contains("no stated expiry"));
    }
}
