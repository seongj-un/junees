//! Finding a server's authorization server: the unauthenticated probe and
//! its `WWW-Authenticate` challenge, the protected resource metadata
//! (RFC 9728) and the authorization server metadata (RFC 8414 or OpenID
//! Connect Discovery).
//!
//! MCP 2026-07-28, authorization/authorization-server-discovery.md:
//! - "Protected Resource Metadata Discovery Requirements": use the
//!   `resource_metadata` URL of the challenge when present, otherwise try
//!   the well-known URI at the path of the MCP endpoint, then at the root.
//! - "Authorization Server Metadata Discovery": for an issuer with a path,
//!   try `/.well-known/oauth-authorization-server/<path>`,
//!   `/.well-known/openid-configuration/<path>`, then
//!   `<path>/.well-known/openid-configuration`; without one the first two
//!   at the root. The document's `issuer` MUST be identical to the issuer
//!   the URL was built from.
//!
//! security-considerations.md, "Authorization Code Protection": without
//! `code_challenge_methods_supported` (holding `S256`) the client MUST
//! refuse to proceed.

use reqwest::Url;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde_json::{Map, Value, json};

use super::challenge::Bearer;
use super::fetch::{self, endpoint, origin};
use crate::mcp::http::HttpConfig;
use crate::mcp::server::MODERN_VERSION;

/// What the login needs to know about a server.
pub struct Discovery {
    /// The resource identifier the server's metadata gave (RFC 8707: sent
    /// as `resource` with every authorization and token request).
    pub resource: String,
    /// `scopes_supported` of the protected resource metadata.
    pub resource_scopes: Vec<String>,
    /// The scope of the 401 challenge, if it named one.
    pub challenge_scope: Option<String>,
    pub server: AuthServer,
}

/// An authorization server's validated metadata.
pub struct AuthServer {
    /// Its issuer identifier, exactly as the resource metadata named it.
    pub issuer: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub registration_endpoint: Option<Url>,
    pub scopes_supported: Vec<String>,
    /// `authorization_response_iss_parameter_supported` (RFC 9207).
    pub iss_parameter_supported: bool,
    /// `token_endpoint_auth_methods_supported`, if it said.
    pub auth_methods: Vec<String>,
}

/// What the unauthenticated probe got back.
pub enum Probe {
    /// HTTP 401, with the `Bearer` challenge if there was one.
    Unauthorized(Option<Bearer>),
    /// Any other status.
    Other(u16),
}

/// POSTs `server/discover` without a token (the configured headers, which
/// hold no `Authorization`, go along) and reads the challenge.
pub async fn probe(
    http: &HttpConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Probe, String> {
    let mut headers = http.resolve(env)?;
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "mcp-protocol-version",
        HeaderValue::from_static(MODERN_VERSION),
    );
    headers.insert("mcp-method", HeaderValue::from_static("server/discover"));
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover",
        "params": {"_meta": crate::mcp::server::request_meta(MODERN_VERSION)}});
    let answer = fetch::post_with(&http.url, headers, body.to_string())
        .await
        .map_err(|reason| format!("the MCP server did not answer: {reason}"))?;
    Ok(match answer.status.as_u16() {
        401 => Probe::Unauthorized(Bearer::from_headers(&answer.headers)),
        status => Probe::Other(status),
    })
}

/// Probes the server and follows its metadata to the authorization server
/// to use (`issuer`, when the user named one).
pub async fn discover(
    http: &HttpConfig,
    env: &dyn Fn(&str) -> Option<String>,
    issuer: Option<&str>,
) -> Result<Discovery, String> {
    let probed = probe(http, env).await?;
    follow(http, probed, issuer).await
}

/// [`discover`] after the probe.
pub async fn follow(
    http: &HttpConfig,
    probed: Probe,
    issuer: Option<&str>,
) -> Result<Discovery, String> {
    let (challenge, open) = match probed {
        Probe::Unauthorized(challenge) => (challenge, None),
        Probe::Other(status) => (None, Some(status)),
    };
    let (resource, metadata) = resource_metadata(&http.url, challenge.as_ref())
        .await
        .map_err(|reason| match open {
            Some(status) => format!(
                "it answered HTTP {status} without asking for a login, and {reason}; it may \
                 need no OAuth login"
            ),
            None => reason,
        })?;
    let servers: Vec<&str> = metadata
        .get("authorization_servers")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if servers.is_empty() {
        return Err("its protected resource metadata names no `authorization_servers`".into());
    }
    let chosen = match issuer {
        Some(wanted) => servers
            .iter()
            .find(|listed| **listed == wanted)
            .copied()
            .ok_or_else(|| {
                format!(
                    "its metadata does not list the authorization server `oauth.issuer` names \
                     (it lists {})",
                    list(&servers)
                )
            })?,
        None => servers
            .iter()
            .find(|listed| endpoint(listed, "x").is_ok())
            .copied()
            .ok_or_else(|| {
                format!(
                    "none of its authorization servers is https ({})",
                    list(&servers)
                )
            })?,
    };
    let server = auth_server(chosen).await?;
    Ok(Discovery {
        resource,
        resource_scopes: strings(&metadata, "scopes_supported"),
        challenge_scope: challenge.and_then(|challenge| challenge.scope),
        server,
    })
}

fn list(items: &[&str]) -> String {
    items
        .iter()
        .map(|item| format!("`{}`", fetch::short(item, 100)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn strings(map: &Map<String, Value>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The protected resource metadata and its validated `resource`.
async fn resource_metadata(
    server: &Url,
    challenge: Option<&Bearer>,
) -> Result<(String, Map<String, Value>), String> {
    let candidates = match challenge.and_then(|bearer| bearer.resource_metadata.as_deref()) {
        Some(url) => vec![endpoint(
            url,
            "the resource metadata address of its challenge",
        )?],
        None => well_known_resource(server),
    };
    let mut tried = Vec::new();
    for url in &candidates {
        let answer = match fetch::get(url).await {
            Ok(answer) => answer,
            Err(reason) => {
                tried.push(format!("{}: {reason}", origin(url)));
                continue;
            }
        };
        let metadata = match answer.object() {
            Some(map) if answer.status.is_success() => map,
            _ => {
                tried.push(format!("HTTP {}", answer.status.as_u16()));
                continue;
            }
        };
        let resource = metadata
            .get("resource")
            .and_then(Value::as_str)
            .ok_or("its protected resource metadata has no `resource`")?;
        if !resource_allows(resource, server) {
            return Err(format!(
                "its protected resource metadata is for another resource (`{}` does not cover \
                 the server's address)",
                fetch::short(resource, 200)
            ));
        }
        return Ok((resource.to_string(), metadata));
    }
    Err(format!(
        "no protected resource metadata was found ({})",
        tried.join("; ")
    ))
}

/// RFC 9728 section 3.1: `/.well-known/oauth-protected-resource` put
/// between the host and the path; then the root.
pub fn well_known_resource(server: &Url) -> Vec<Url> {
    let mut root = server.clone();
    root.set_query(None);
    root.set_fragment(None);
    let path = server.path().trim_end_matches('/').to_string();
    let mut urls = Vec::new();
    if !path.is_empty() {
        let mut url = root.clone();
        url.set_path(&format!("/.well-known/oauth-protected-resource{path}"));
        urls.push(url);
    }
    root.set_path("/.well-known/oauth-protected-resource");
    urls.push(root);
    urls
}

/// The metadata URLs to try for `issuer`, in the specification's order.
pub fn well_known_issuer(issuer: &Url) -> Vec<Url> {
    let path = issuer.path().trim_end_matches('/').to_string();
    let at = |path: String| {
        let mut url = issuer.clone();
        url.set_query(None);
        url.set_path(&path);
        url
    };
    if path.is_empty() {
        vec![
            at("/.well-known/oauth-authorization-server".into()),
            at("/.well-known/openid-configuration".into()),
        ]
    } else {
        vec![
            at(format!("/.well-known/oauth-authorization-server{path}")),
            at(format!("/.well-known/openid-configuration{path}")),
            at(format!("{path}/.well-known/openid-configuration")),
        ]
    }
}

/// Whether the resource identifier `resource` covers the server at
/// `server`: same scheme, host and port, and the server's path is the
/// resource's path or below it. A resource with a query or fragment
/// covers nothing (RFC 8707 section 2: it SHOULD NOT have a query and
/// MUST NOT have a fragment).
pub fn resource_allows(resource: &str, server: &Url) -> bool {
    let Ok(resource) = Url::parse(resource) else {
        return false;
    };
    if resource.query().is_some() || resource.fragment().is_some() {
        return false;
    }
    if resource.origin() != server.origin() {
        return false;
    }
    let base = resource.path().trim_end_matches('/');
    let path = server.path();
    base.is_empty() || path == base || path.trim_end_matches('/') == base || {
        path.strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// The metadata of the authorization server `issuer`, validated.
pub async fn auth_server(issuer: &str) -> Result<AuthServer, String> {
    let issuer_url = endpoint(issuer, "the authorization server")?;
    if issuer_url.query().is_some() {
        return Err("the authorization server's issuer has a query".into());
    }
    let mut tried = Vec::new();
    for url in well_known_issuer(&issuer_url) {
        let answer = match fetch::get(&url).await {
            Ok(answer) => answer,
            Err(reason) => {
                tried.push(reason);
                continue;
            }
        };
        let Some(metadata) = answer.object().filter(|_| answer.status.is_success()) else {
            tried.push(format!("HTTP {}", answer.status.as_u16()));
            continue;
        };
        return validate(issuer, &metadata);
    }
    Err(format!(
        "no metadata of the authorization server `{}` was found ({})",
        origin(&issuer_url),
        tried.join("; ")
    ))
}

/// Checks a metadata document fetched for `issuer`.
pub fn validate(issuer: &str, metadata: &Map<String, Value>) -> Result<AuthServer, String> {
    let named = metadata.get("issuer").and_then(Value::as_str);
    if named != Some(issuer) {
        return Err(format!(
            "the authorization server's metadata names another issuer (`{}`, not `{}`), so \
             it is not used",
            fetch::short(named.unwrap_or_default(), 200),
            fetch::short(issuer, 200)
        ));
    }
    let url = |key: &str, what: &str| -> Result<Option<Url>, String> {
        metadata
            .get(key)
            .and_then(Value::as_str)
            .map(|text| endpoint(text, what))
            .transpose()
    };
    let authorization_endpoint = url("authorization_endpoint", "its authorization endpoint")?
        .ok_or("the authorization server's metadata has no `authorization_endpoint`")?;
    let token_endpoint = url("token_endpoint", "its token endpoint")?
        .ok_or("the authorization server's metadata has no `token_endpoint`")?;
    let registration_endpoint = url("registration_endpoint", "its registration endpoint")?;
    let methods = strings(metadata, "code_challenge_methods_supported");
    if !methods.iter().any(|method| method == "S256") {
        return Err("the authorization server does not announce PKCE with S256 \
             (`code_challenge_methods_supported`), so junees does not log in there"
            .into());
    }
    let response_types = strings(metadata, "response_types_supported");
    if !response_types.is_empty() && !response_types.iter().any(|kind| kind == "code") {
        return Err("the authorization server does not offer the `code` response type".into());
    }
    Ok(AuthServer {
        issuer: issuer.to_string(),
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        scopes_supported: strings(metadata, "scopes_supported"),
        iss_parameter_supported: metadata
            .get("authorization_response_iss_parameter_supported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        auth_methods: strings(metadata, "token_endpoint_auth_methods_supported"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(list: Vec<Url>) -> Vec<String> {
        list.into_iter().map(String::from).collect()
    }

    #[test]
    fn well_known_resource_urls_insert_the_path() {
        let server = Url::parse("https://example.com/public/mcp?key=s3cret").unwrap();
        assert_eq!(
            urls(well_known_resource(&server)),
            [
                "https://example.com/.well-known/oauth-protected-resource/public/mcp",
                "https://example.com/.well-known/oauth-protected-resource",
            ]
        );
        let root = Url::parse("https://example.com/").unwrap();
        assert_eq!(
            urls(well_known_resource(&root)),
            ["https://example.com/.well-known/oauth-protected-resource"]
        );
    }

    #[test]
    fn issuer_urls_follow_the_specified_order() {
        let tenant = Url::parse("https://auth.example.com/tenant1").unwrap();
        assert_eq!(
            urls(well_known_issuer(&tenant)),
            [
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example.com/.well-known/openid-configuration/tenant1",
                "https://auth.example.com/tenant1/.well-known/openid-configuration",
            ]
        );
        let plain = Url::parse("https://auth.example.com").unwrap();
        assert_eq!(
            urls(well_known_issuer(&plain)),
            [
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn a_resource_covers_its_own_server_only() {
        let server = Url::parse("https://mcp.example.com/server/mcp").unwrap();
        for good in [
            "https://mcp.example.com/server/mcp",
            "https://mcp.example.com/server/mcp/",
            "https://mcp.example.com/server",
            "https://mcp.example.com",
            "https://MCP.example.com/",
            "https://mcp.example.com:443/server",
        ] {
            assert!(resource_allows(good, &server), "{good}");
        }
        for bad in [
            "https://mcp.example.com/serv",
            "https://mcp.example.com/server/mcp/more",
            "https://mcp.example.com/other",
            "https://evil.example.com/server/mcp",
            "http://mcp.example.com/server/mcp",
            "https://mcp.example.com:8443/server/mcp",
            "https://mcp.example.com/server/mcp?x=1",
            "https://mcp.example.com/server/mcp#f",
            "mcp.example.com/server/mcp",
        ] {
            assert!(!resource_allows(bad, &server), "{bad}");
        }
    }

    fn metadata(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    #[test]
    fn metadata_must_name_the_issuer_and_offer_s256() {
        let good = json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "registration_endpoint": "https://auth.example.com/register",
            "code_challenge_methods_supported": ["plain", "S256"],
            "response_types_supported": ["code"],
            "authorization_response_iss_parameter_supported": true,
            "scopes_supported": ["a", "offline_access"]
        });
        let server = validate("https://auth.example.com", &metadata(good.clone())).unwrap();
        assert!(server.iss_parameter_supported);
        assert!(server.registration_endpoint.is_some());
        assert_eq!(server.scopes_supported, ["a", "offline_access"]);

        // The attacker example of the specification.
        let message = validate("https://attacker.example", &metadata(good.clone()))
            .err()
            .unwrap();
        assert!(message.contains("another issuer"), "{message}");
        // A trailing slash is another issuer: simple string comparison.
        assert!(validate("https://auth.example.com/", &metadata(good.clone())).is_err());

        let without = |key: &str| {
            let mut copy = good.clone();
            copy.as_object_mut().unwrap().remove(key);
            validate("https://auth.example.com", &metadata(copy))
        };
        assert!(
            without("code_challenge_methods_supported")
                .err()
                .unwrap()
                .contains("PKCE")
        );
        assert!(without("token_endpoint").is_err());
        assert!(without("authorization_endpoint").is_err());
        assert!(without("registration_endpoint").is_ok());
        let mut plain_only = good.clone();
        plain_only["code_challenge_methods_supported"] = json!(["plain"]);
        assert!(validate("https://auth.example.com", &metadata(plain_only)).is_err());
        let mut insecure = good.clone();
        insecure["token_endpoint"] = json!("http://auth.example.com/token");
        let message = validate("https://auth.example.com", &metadata(insecure))
            .err()
            .unwrap();
        assert!(message.contains("not https"), "{message}");
    }
}
