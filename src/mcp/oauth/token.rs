//! The client and the token endpoint: Dynamic Client Registration
//! (RFC 7591), the authorization code exchange and the refresh
//! (OAuth 2.1 sections 4.1.3 and 4.3), each with the `resource` of
//! RFC 8707.
//!
//! MCP 2026-07-28, authorization.md, "Resource Parameter Implementation":
//! `resource` MUST be included in both authorization requests and token
//! requests. client-registration.md, "Dynamic Client Registration": a
//! client MUST give an `application_type`; a CLI is `native`.

use reqwest::Url;
use serde_json::{Value, json};

use super::fetch::{self, Answer};

/// The OAuth client Junees logs in as. Not `Debug`: it may hold a
/// secret.
#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>,
    /// `none`, `client_secret_basic` or `client_secret_post`.
    pub auth: String,
}

impl Client {
    /// A pre-registered client: the secret, if any, goes in HTTP Basic
    /// unless the server offers only `client_secret_post`.
    pub fn configured(id: String, secret: Option<String>, offered: &[String]) -> Self {
        let auth = match &secret {
            None => "none",
            Some(_)
                if !offered.is_empty()
                    && !offered.iter().any(|m| m == "client_secret_basic")
                    && offered.iter().any(|m| m == "client_secret_post") =>
            {
                "client_secret_post"
            }
            Some(_) => "client_secret_basic",
        };
        Self {
            id,
            secret,
            auth: auth.into(),
        }
    }

    /// The client parameters of a token request: `client_id` (and the
    /// secret for `client_secret_post`) in the form, or HTTP Basic.
    fn authenticate<'a>(
        &'a self,
        form: &mut Vec<(&'static str, &'a str)>,
    ) -> Option<(&'a str, &'a str)> {
        match (self.auth.as_str(), &self.secret) {
            ("client_secret_basic", Some(secret)) => Some((&self.id, secret)),
            ("client_secret_post", Some(secret)) => {
                form.push(("client_id", &self.id));
                form.push(("client_secret", secret));
                None
            }
            _ => {
                form.push(("client_id", &self.id));
                None
            }
        }
    }
}

/// Registers Junees with `endpoint` for `redirect_uri` (RFC 7591 section
/// 3.1): a public native client using the authorization code and refresh
/// tokens.
pub async fn register(
    endpoint: &Url,
    redirect_uri: &str,
    scope: Option<&str>,
) -> Result<Client, String> {
    let mut request = json!({
        "client_name": "Junees",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    });
    if let Some(scope) = scope {
        request["scope"] = json!(scope);
    }
    let answer = fetch::post_json(endpoint, &request)
        .await
        .map_err(|reason| format!("registering junees as a client failed: {reason}"))?;
    if !answer.status.is_success() {
        return Err(format!(
            "the authorization server refused to register junees as a client ({}); set \
             `oauth.client_id` in mcp.json to a client you registered there yourself",
            fetch::oauth_error(&answer)
        ));
    }
    let fields = answer
        .object()
        .ok_or("the client registration answer is not a JSON object")?;
    let id = fields
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("the client registration answer has no `client_id`")?;
    let secret = fields
        .get("client_secret")
        .and_then(Value::as_str)
        .filter(|secret| !secret.is_empty())
        .map(str::to_string);
    let auth = match (
        fields
            .get("token_endpoint_auth_method")
            .and_then(Value::as_str),
        &secret,
    ) {
        (_, None) => "none",
        (Some("client_secret_post"), Some(_)) => "client_secret_post",
        (_, Some(_)) => "client_secret_basic",
    };
    Ok(Client {
        id: id.to_string(),
        secret,
        auth: auth.into(),
    })
}

/// Tokens the token endpoint issued. Not `Debug`.
pub struct Issued {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
}

/// Why a token request failed.
pub enum TokenError {
    /// `invalid_grant`: the code or refresh token is no longer good.
    InvalidGrant(String),
    Other(String),
}

impl TokenError {
    pub fn text(self) -> String {
        match self {
            TokenError::InvalidGrant(text) | TokenError::Other(text) => text,
        }
    }
}

/// Redeems `code` (with the PKCE `verifier`) at `endpoint`.
pub async fn exchange_code(
    endpoint: &Url,
    client: &Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    resource: &str,
) -> Result<Issued, TokenError> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
        ("resource", resource),
    ];
    let basic = client.authenticate(&mut form);
    token_request(endpoint, &form, basic).await
}

/// Gets a new access token with `refresh_token`.
pub async fn refresh(
    endpoint: &Url,
    client: &Client,
    refresh_token: &str,
    resource: &str,
) -> Result<Issued, TokenError> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("resource", resource),
    ];
    let basic = client.authenticate(&mut form);
    token_request(endpoint, &form, basic).await
}

async fn token_request(
    endpoint: &Url,
    form: &[(&str, &str)],
    basic: Option<(&str, &str)>,
) -> Result<Issued, TokenError> {
    let answer = fetch::post_form(endpoint, form, basic)
        .await
        .map_err(|reason| TokenError::Other(format!("the token request failed: {reason}")))?;
    if !answer.status.is_success() {
        let text = format!(
            "the authorization server refused the token request ({})",
            fetch::oauth_error(&answer)
        );
        let invalid_grant = answer
            .object()
            .and_then(|fields| fields.get("error").cloned())
            == Some(json!("invalid_grant"));
        return Err(if invalid_grant {
            TokenError::InvalidGrant(text)
        } else {
            TokenError::Other(text)
        });
    }
    issued(&answer).map_err(TokenError::Other)
}

/// Checks a token response (OAuth 2.1 section 3.2.3).
fn issued(answer: &Answer) -> Result<Issued, String> {
    let fields = answer
        .object()
        .ok_or("the token answer is not a JSON object")?;
    let text = |key: &str| fields.get(key).and_then(Value::as_str);
    let access_token = text("access_token")
        .filter(|token| !token.is_empty())
        .ok_or("the token answer has no `access_token`")?;
    // It goes in an HTTP header: visible ASCII only.
    if access_token.len() > 16 * 1024 || !access_token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err("the access token holds characters an HTTP header cannot carry".into());
    }
    match text("token_type") {
        Some(kind) if kind.eq_ignore_ascii_case("bearer") => {}
        Some(kind) => {
            return Err(format!(
                "the token answer is of type `{}`; junees uses Bearer tokens only",
                fetch::short(kind, 40)
            ));
        }
        None => return Err("the token answer has no `token_type`".into()),
    }
    let refresh_token = text("refresh_token")
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let expires_in = match fields.get("expires_in") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
                .or_else(|| value.as_str().and_then(|n| n.parse().ok()))
                .ok_or("the token answer's `expires_in` is not a number of seconds")?,
        ),
    };
    Ok(Issued {
        access_token: access_token.to_string(),
        refresh_token,
        expires_in,
        scope: text("scope").map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use reqwest::header::HeaderMap;

    fn answer(body: Value) -> Answer {
        Answer {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: body.to_string().into_bytes(),
        }
    }

    #[test]
    fn token_answers_are_checked_without_quoting_tokens() {
        let good = issued(&answer(
            json!({"access_token": "at-1", "token_type": "bearer",
            "expires_in": 3600, "refresh_token": "rt-1", "scope": "a b"}),
        ))
        .ok()
        .unwrap();
        assert_eq!(good.access_token, "at-1");
        assert_eq!(good.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(good.expires_in, Some(3600));
        let lenient = issued(&answer(
            json!({"access_token": "at", "token_type": "Bearer", "expires_in": "60"}),
        ))
        .ok()
        .unwrap();
        assert_eq!(lenient.expires_in, Some(60));
        assert!(lenient.refresh_token.is_none());

        let err = |body: Value| issued(&answer(body)).err().unwrap();
        assert!(err(json!({"token_type": "Bearer"})).contains("no `access_token`"));
        let message = err(json!({"access_token": "s3cret\nx", "token_type": "Bearer"}));
        assert!(!message.contains("s3cret"), "{message}");
        assert!(err(json!({"access_token": "a", "token_type": "DPoP"})).contains("Bearer"));
        assert!(err(json!({"access_token": "a"})).contains("token_type"));
        assert!(
            err(json!({"access_token": "a", "token_type": "Bearer", "expires_in": "soon"}))
                .contains("expires_in")
        );
    }

    #[test]
    fn client_authentication_follows_the_method() {
        let public = Client::configured("id".into(), None, &[]);
        let mut form = Vec::new();
        assert!(public.authenticate(&mut form).is_none());
        assert_eq!(form, [("client_id", "id")]);

        let basic = Client::configured("id".into(), Some("pw".into()), &[]);
        assert_eq!(basic.auth, "client_secret_basic");
        let mut form = Vec::new();
        assert_eq!(basic.authenticate(&mut form), Some(("id", "pw")));
        assert!(form.is_empty());

        let post = Client::configured(
            "id".into(),
            Some("pw".into()),
            &["client_secret_post".to_string()],
        );
        assert_eq!(post.auth, "client_secret_post");
        let mut form = Vec::new();
        assert!(post.authenticate(&mut form).is_none());
        assert_eq!(form, [("client_id", "id"), ("client_secret", "pw")]);
    }
}
