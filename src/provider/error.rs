//! Errors a provider can return, typed so the agent can react to them.

use std::fmt;

#[derive(Debug)]
pub enum ProviderError {
    /// The API answered with an error status.
    Api {
        status: u16,
        /// The API's error type, e.g. `invalid_request_error`.
        kind: Option<String>,
        message: String,
    },
    /// The request did not complete: connection, timeout, or a body cut off.
    Transport(Box<dyn std::error::Error + Send + Sync>),
    /// The response arrived but could not be understood.
    Decode(String),
}

impl ProviderError {
    /// The conversation no longer fits the model's context window.
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::Api {
                status: 400,
                message,
                ..
            } => {
                let message = message.to_lowercase();
                message.contains("prompt is too long") || message.contains("context window")
            }
            _ => false,
        }
    }

    /// The credentials were rejected.
    pub fn is_auth(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: 401 | 403,
                ..
            }
        )
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api {
                status,
                kind,
                message,
            } => {
                write!(f, "API error {status}")?;
                if let Some(kind) = kind {
                    write!(f, " ({kind})")?;
                }
                if !message.is_empty() {
                    write!(f, ": {message}")?;
                }
                Ok(())
            }
            Self::Transport(err) => write!(f, "request failed: {err}"),
            Self::Decode(err) => write!(f, "could not parse the API response: {err}"),
        }
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, kind: Option<&str>, message: &str) -> ProviderError {
        ProviderError::Api {
            status,
            kind: kind.map(Into::into),
            message: message.into(),
        }
    }

    #[test]
    fn displays_api_errors() {
        assert_eq!(
            api(401, Some("authentication_error"), "invalid x-api-key").to_string(),
            "API error 401 (authentication_error): invalid x-api-key"
        );
        assert_eq!(
            api(502, None, "upstream down").to_string(),
            "API error 502: upstream down"
        );
        assert_eq!(api(502, None, "").to_string(), "API error 502");
    }

    #[test]
    fn detects_context_overflow() {
        let overflow = api(
            400,
            Some("invalid_request_error"),
            "prompt is too long: 215000 tokens > 200000 maximum",
        );
        assert!(overflow.is_context_overflow());
        assert!(!api(400, None, "messages: field required").is_context_overflow());
        assert!(!api(500, None, "prompt is too long").is_context_overflow());
    }

    #[test]
    fn detects_auth_errors() {
        assert!(api(401, None, "").is_auth());
        assert!(api(403, None, "").is_auth());
        assert!(!api(429, None, "").is_auth());
    }
}
