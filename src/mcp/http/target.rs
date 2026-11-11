//! Where an HTTP server of `mcp.json` is and what it is sent: `url` and
//! `headers`.
//!
//! ```json
//! { "servers": { "docs": { "url": "https://mcp.example.com/mcp",
//!   "headers": {"Authorization": "Bearer env:DOCS_TOKEN"} } } }
//! ```
//!
//! A header value names an environment variable (`env:NAME`, optionally
//! after one scheme word such as `Bearer`) instead of holding the secret,
//! so the file holds no token. The variables are hidden from bash, MCP
//! servers and plugins like `JUNEES_HIDDEN_ENV` ([`crate::config::Config`]).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Header names the transport sets itself.
const RESERVED: &[&str] = &[
    "accept",
    "connection",
    "content-length",
    "content-type",
    "host",
    "last-event-id",
    "mcp-method",
    "mcp-name",
    "mcp-protocol-version",
    "mcp-session-id",
    "transfer-encoding",
];

/// The server's address and headers. Deliberately not `Debug`: the URL
/// may carry a token in its path or query.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub url: Url,
    /// In name order.
    pub headers: Vec<HeaderRef>,
}

/// A header whose value comes from an environment variable.
#[derive(Clone, PartialEq, Eq)]
pub struct HeaderRef {
    pub name: String,
    /// A word put before the value with a space (`Bearer`).
    pub scheme: Option<String>,
    /// The variable holding the value.
    pub var: String,
}

impl HttpConfig {
    /// Checks `url` (https, or http to this computer only; no user name
    /// or password) and `headers` (names a client may set, values that
    /// are environment references). Errors never quote the values.
    pub fn parse(url: &str, headers: &BTreeMap<String, String>) -> Result<Self> {
        let Ok(mut url) = Url::parse(url) else {
            bail!("`url` is not a valid absolute URL");
        };
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(&url) => {}
            "http" => bail!(
                "`url` must use https; http is allowed only to this computer (localhost, \
                 127.0.0.1, [::1])"
            ),
            _ => bail!("`url` must be an https URL"),
        }
        if url.host().is_none() {
            bail!("`url` has no host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!(
                "`url` must not hold a user name or password; send credentials in `headers` \
                 with an `env:NAME` reference"
            );
        }
        url.set_fragment(None);
        let headers = headers
            .iter()
            .map(|(name, value)| header_ref(name, value))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { url, headers })
    }

    /// `https://host:port`: what may be shown of the URL. Its path and
    /// query can hold a token.
    pub fn origin(&self) -> String {
        self.url.origin().ascii_serialization()
    }

    /// Whether the server is on this computer.
    pub fn is_loopback(&self) -> bool {
        is_loopback(&self.url)
    }

    /// The variables the headers read.
    pub fn env_names(&self) -> impl Iterator<Item = &str> {
        self.headers.iter().map(|header| header.var.as_str())
    }

    /// `Authorization (from env TOKEN)`, for `/mcp` and doctor; never a
    /// value.
    pub fn describe_headers(&self) -> String {
        self.headers
            .iter()
            .map(|header| format!("{} (from env {})", header.name, header.var))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Why a header cannot be built from `lookup`, for each header: the
    /// variable is unset or empty. Values are never read out.
    pub fn missing(&self, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
        self.headers
            .iter()
            .filter(|header| lookup(&header.var).is_none_or(|value| value.trim().is_empty()))
            .map(unset)
            .collect()
    }

    /// The headers with their values from `lookup`, marked sensitive.
    pub fn resolve(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> std::result::Result<HeaderMap, String> {
        let mut map = HeaderMap::new();
        for header in &self.headers {
            let Some(value) = lookup(&header.var).filter(|value| !value.trim().is_empty()) else {
                return Err(unset(header));
            };
            let value = match &header.scheme {
                Some(scheme) => format!("{scheme} {}", value.trim()),
                None => value.trim().to_string(),
            };
            let Ok(mut value) = HeaderValue::from_str(&value) else {
                return Err(format!(
                    "the variable {} holds characters an HTTP header cannot carry (header \
                     `{}`)",
                    header.var, header.name
                ));
            };
            value.set_sensitive(true);
            let name = HeaderName::from_bytes(header.name.as_bytes()).expect("checked name");
            map.insert(name, value);
        }
        Ok(map)
    }
}

fn unset(header: &HeaderRef) -> String {
    format!(
        "header `{}` takes its value from the variable {}, which is not set",
        header.name, header.var
    )
}

/// Whether `url` points at this computer by its address or `localhost`.
pub fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(address) => address.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

fn header_ref(name: &str, value: &str) -> Result<HeaderRef> {
    if HeaderName::from_bytes(name.as_bytes()).is_err() {
        bail!(
            "`headers` has a name that is not a valid HTTP header name: `{}`",
            crate::prompt_text::escape_hidden(name)
        );
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED.contains(&lower.as_str()) || lower.starts_with("mcp-param-") {
        bail!("`headers` may not set `{name}`: the MCP transport sets it");
    }
    let (scheme, reference) = match value.split_once(' ') {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, value),
    };
    let var = reference
        .strip_prefix("env:")
        .filter(|var| is_var_name(var));
    let scheme_ok = scheme.is_none_or(|scheme| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    let (Some(var), true) = (var, scheme_ok) else {
        bail!(
            "the value of header `{name}` must be a reference to an environment variable, \
             `env:NAME` or a scheme word and one, as in `Bearer env:NAME`, so the file holds \
             no secret"
        );
    };
    Ok(HeaderRef {
        name: name.to_string(),
        scheme: scheme.map(str::to_string),
        var: var.to_string(),
    })
}

/// `[A-Za-z_][A-Za-z0-9_]*`.
fn is_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn err(url: &str, pairs: &[(&str, &str)]) -> String {
        format!(
            "{:#}",
            HttpConfig::parse(url, &headers(pairs)).err().unwrap()
        )
    }

    #[test]
    fn urls_are_https_or_loopback_http_without_credentials() {
        for good in [
            "https://mcp.example.com/mcp",
            "http://127.0.0.1:8080/mcp",
            "http://127.9.9.9/",
            "http://[::1]:9/mcp",
            "http://LocalHost:3000/mcp",
        ] {
            assert!(HttpConfig::parse(good, &BTreeMap::new()).is_ok(), "{good}");
        }
        assert!(err("http://mcp.example.com/mcp", &[]).contains("must use https"));
        assert!(err("http://10.0.0.1/mcp", &[]).contains("must use https"));
        assert!(err("http://localhost.example.com/", &[]).contains("must use https"));
        assert!(err("ftp://example.com/", &[]).contains("must be an https URL"));
        assert!(err("mcp.example.com/mcp", &[]).contains("not a valid absolute URL"));
        let message = err("https://me:hunter2@example.com/mcp", &[]);
        assert!(message.contains("user name or password"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        let config = HttpConfig::parse(
            "https://mcp.example.com:8443/t0ken/mcp?key=s3cret#frag",
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(config.origin(), "https://mcp.example.com:8443");
        assert_eq!(config.url.fragment(), None);
        assert!(!config.is_loopback());
    }

    #[test]
    fn header_values_are_environment_references_only() {
        let config = HttpConfig::parse(
            "https://example.com/mcp",
            &headers(&[
                ("Authorization", "Bearer env:DOCS_TOKEN"),
                ("X-Api-Key", "env:_KEY2"),
            ]),
        )
        .unwrap();
        assert_eq!(
            config.describe_headers(),
            "Authorization (from env DOCS_TOKEN), X-Api-Key (from env _KEY2)"
        );
        assert_eq!(
            config.env_names().collect::<Vec<_>>(),
            ["DOCS_TOKEN", "_KEY2"]
        );

        for value in [
            "Bearer ghp_abcdef123456",
            "ghp_abcdef123456",
            "env:",
            "env:1ABC",
            "env:A-B",
            "Bearer  env:A",
            "Bearer x env:A",
            "Bear/er env:A",
            "${A}",
        ] {
            let message = err("https://example.com/", &[("Authorization", value)]);
            assert!(
                message.contains("must be a reference"),
                "{value}: {message}"
            );
            assert!(!message.contains("ghp_"), "{message}");
        }
        for name in ["Mcp-Session-Id", "accept", "MCP-Param-Region", "Host"] {
            let message = err("https://example.com/", &[(name, "env:A")]);
            assert!(message.contains("the MCP transport sets it"), "{message}");
        }
        assert!(err("https://example.com/", &[("bad name", "env:A")]).contains("valid HTTP"));
    }

    #[test]
    fn values_come_from_the_lookup_and_are_sensitive() {
        let config = HttpConfig::parse(
            "https://example.com/mcp",
            &headers(&[
                ("Authorization", "Bearer env:TOKEN"),
                ("X-Team", "env:TEAM"),
            ]),
        )
        .unwrap();
        let lookup = |name: &str| match name {
            "TOKEN" => Some(" s3cret ".to_string()),
            "TEAM" => Some("core".to_string()),
            _ => None,
        };
        assert!(config.missing(&lookup).is_empty());
        let map = config.resolve(&lookup).unwrap();
        assert_eq!(map["authorization"], "Bearer s3cret");
        assert!(map["authorization"].is_sensitive());
        assert_eq!(map["x-team"], "core");

        let partial = |name: &str| (name == "TEAM").then(|| "core".to_string());
        let missing = config.missing(&partial);
        assert_eq!(
            missing,
            ["header `Authorization` takes its value from the variable TOKEN, which is not set"]
        );
        assert_eq!(config.resolve(&partial).unwrap_err(), missing[0]);
        let empty = |_: &str| Some("  ".to_string());
        assert_eq!(config.missing(&empty).len(), 2);

        let newline = |_: &str| Some("a\nb-s3cret".to_string());
        let message = config.resolve(&newline).unwrap_err();
        assert!(message.contains("cannot carry"), "{message}");
        assert!(!message.contains("s3cret"), "{message}");
    }
}
