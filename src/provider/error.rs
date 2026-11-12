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
    /// The conversation no longer fits the model's context window: the
    /// Anthropic API says the prompt is too long, OpenAI-compatible
    /// servers name the context length (OpenAI's error code
    /// `context_length_exceeded`, which [`super::openai`] keeps as the
    /// kind).
    ///
    /// Not when the output limit alone is the window or more
    /// ([`Self::is_output_limit_too_large`]): no shorter history fits then.
    pub fn is_context_overflow(&self) -> bool {
        if self.is_output_limit_too_large() {
            return false;
        }
        match self {
            Self::Api {
                kind: Some(kind), ..
            } if kind == "context_length_exceeded" => true,
            Self::Api {
                status: 400,
                message,
                ..
            } => {
                let message = message.to_lowercase();
                message.contains("prompt is too long")
                    || message.contains("context window")
                    || message.contains("context length")
            }
            _ => false,
        }
    }

    /// The output limit asked for (`max_tokens`) is the model's whole
    /// context window or more, as vLLM and OpenAI word it for a model
    /// with a small window: compacting the conversation cannot help,
    /// lowering `max_tokens` does.
    pub fn is_output_limit_too_large(&self) -> bool {
        match self {
            Self::Api {
                status: 400,
                message,
                ..
            } => window_and_output(message).is_some_and(|(window, output)| output >= window),
            _ => false,
        }
    }

    /// The model does not take a thinking budget (`thinking.type:
    /// "enabled"`): Claude 4.7 and later models reject it.
    pub fn is_thinking_budget_unsupported(&self) -> bool {
        matches!(
            self,
            Self::Api { status: 400, message, .. } if message.contains("thinking.type.enabled")
        )
    }

    /// The request offered web search, which an administrator turned off
    /// for the organization: the docs say the API then answers 400 with a
    /// message that web search is not enabled.
    pub fn is_web_search_disabled(&self) -> bool {
        match self {
            Self::Api {
                status: 400,
                message,
                ..
            } => {
                let message = message.to_lowercase().replace('_', " ");
                message.contains("web search")
                    && (message.contains("not enabled") || message.contains("disabled"))
            }
            _ => false,
        }
    }

    /// The API rejected the request because of an image in it: a 400
    /// `invalid_request_error` whose message names an image (`messages.0
    /// .content.0.image.source.base64: ... does not appear to be a valid
    /// png image`, `image exceeds 5 MB maximum`, the smaller limit of
    /// many-image requests). A 400 that does not mention one (a bad tool
    /// schema, a rejected thinking budget) leaves the images alone, and so
    /// does a context overflow, which compaction handles.
    pub fn is_image_rejected(&self) -> bool {
        match self {
            Self::Api {
                status: 400,
                kind,
                message,
            } => {
                kind.as_deref()
                    .is_none_or(|kind| kind == "invalid_request_error")
                    && message.to_lowercase().contains("image")
                    && !self.is_context_overflow()
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
                if self.is_thinking_budget_unsupported() {
                    write!(
                        f,
                        " (this model takes no thinking budget: set JUNEES_THINKING_BUDGET \
                         to 0; models that think by default keep doing so)"
                    )?;
                }
                if *status == 400 && window_and_output(message).is_some() {
                    write!(
                        f,
                        " (the output limit counts toward the model's context window: on a \
                         model with a small window, lower max_tokens (JUNEES_MAX_TOKENS))"
                    )?;
                }
                if self.is_web_search_disabled() {
                    write!(
                        f,
                        " (web search is turned off for this organization in the Claude \
                         Console: set JUNEES_WEB_SEARCH=off, or ask an admin to turn it on)"
                    )?;
                }
                Ok(())
            }
            Self::Transport(err) => write!(f, "request failed: {err}"),
            Self::Decode(err) => write!(f, "could not parse the API response: {err}"),
        }
    }
}

/// The context window and the output limit an error message names, as
/// vLLM and OpenAI write them: "This model's maximum context length is
/// 4096 tokens. However, you requested 8292 tokens (100 in the messages,
/// 8192 in the completion)", or newer vLLM: "'max_tokens' or
/// 'max_completion_tokens' is too large: 8192. This model's maximum
/// context length is 4096 tokens and your request has 100 input tokens".
fn window_and_output(message: &str) -> Option<(u64, u64)> {
    let message = message.to_lowercase();
    let window = number_after(&message, "maximum context length is")?;
    let output = number_before(&message, "in the completion")
        .or_else(|| number_after(&message, "is too large:"))?;
    Some((window, output))
}

/// The number right after `marker` in `text`.
fn number_after(text: &str, marker: &str) -> Option<u64> {
    let (_, rest) = text.split_once(marker)?;
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The number right before `marker` in `text`.
fn number_before(text: &str, marker: &str) -> Option<u64> {
    let (head, _) = text.split_once(marker)?;
    let head = head.trim_end();
    let start = head
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |at| at + 1);
    head[start..].parse().ok()
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
        // OpenAI-compatible servers.
        assert!(
            api(
                400,
                Some("invalid_request_error"),
                "This model's maximum context length is 32768 tokens. However, you requested \
                 40000 tokens."
            )
            .is_context_overflow()
        );
        assert!(api(413, Some("context_length_exceeded"), "too long").is_context_overflow());
        assert!(!api(400, Some("invalid_request_error"), "bad tool schema").is_context_overflow());
    }

    #[test]
    fn an_output_limit_larger_than_the_window_is_no_overflow() {
        // vLLM, older and newer wording, for max_tokens 8192 on a 4096 window.
        for message in [
            "This model's maximum context length is 4096 tokens. However, you requested 8292 \
             tokens (100 in the messages, 8192 in the completion). Please reduce the length of \
             the messages or completion.",
            "'max_tokens' or 'max_completion_tokens' is too large: 8192. This model's maximum \
             context length is 4096 tokens and your request has 100 input tokens (8192 > 4096 \
             - 100).",
        ] {
            let err = api(400, Some("BadRequestError"), message);
            assert!(err.is_output_limit_too_large(), "{message}");
            assert!(!err.is_context_overflow(), "{message}");
            assert!(err.to_string().contains("lower max_tokens"), "{err}");
            // OpenAI's code does not make it one either.
            let coded = api(400, Some("context_length_exceeded"), message);
            assert!(!coded.is_context_overflow(), "{message}");
        }
        // A long conversation with a small output limit is an overflow,
        // and still names the limit.
        let long = api(
            400,
            Some("invalid_request_error"),
            "This model's maximum context length is 32768 tokens. However, you requested 40000 \
             tokens (31808 in the messages, 8192 in the completion).",
        );
        assert!(long.is_context_overflow());
        assert!(!long.is_output_limit_too_large());
        assert!(long.to_string().contains("JUNEES_MAX_TOKENS"), "{long}");
        // Without the numbers nothing changes.
        let plain = api(
            400,
            None,
            "This model's maximum context length is 8192 tokens.",
        );
        assert!(plain.is_context_overflow());
        assert!(!plain.to_string().contains("JUNEES_MAX_TOKENS"), "{plain}");
        assert!(
            !api(
                500,
                None,
                "maximum context length is 1 tokens (0 in the messages, 9 in the completion)"
            )
            .is_output_limit_too_large()
        );
    }

    #[test]
    fn image_rejections_are_told_apart() {
        let broken = api(
            400,
            Some("invalid_request_error"),
            "messages.0.content.0.image.source.base64: The image was specified using the \
             image/png media type, but does not appear to be a valid png image",
        );
        assert!(broken.is_image_rejected());
        assert!(api(400, None, "Image exceeds 5 MB maximum").is_image_rejected());
        // Other 400s, other statuses and overflows leave images alone.
        assert!(
            !api(400, Some("invalid_request_error"), "tools.0: bad schema").is_image_rejected()
        );
        assert!(!api(400, Some("other_error"), "image").is_image_rejected());
        assert!(!api(500, Some("api_error"), "image service down").is_image_rejected());
        assert!(!api(400, None, "prompt is too long: image tokens").is_image_rejected());
    }

    #[test]
    fn a_rejected_thinking_budget_names_the_setting() {
        // The message Claude 4.7 and later models send for a budget.
        let rejected = api(
            400,
            Some("invalid_request_error"),
            "\"thinking.type.enabled\" is not supported for this model. Use \
             \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking \
             behavior.",
        );
        assert!(rejected.is_thinking_budget_unsupported());
        let shown = rejected.to_string();
        assert!(shown.starts_with("API error 400 (invalid_request_error): \"thinking"));
        assert!(shown.contains("set JUNEES_THINKING_BUDGET to 0"), "{shown}");
        assert!(
            !api(400, None, "thinking.type.adaptive is not supported")
                .is_thinking_budget_unsupported()
        );
        assert!(!api(500, None, "thinking.type.enabled").is_thinking_budget_unsupported());
        assert!(
            !api(400, None, "messages: field required")
                .to_string()
                .contains("JUNEES")
        );
    }

    #[test]
    fn web_search_turned_off_for_the_organization_names_the_setting() {
        let off = api(
            400,
            Some("invalid_request_error"),
            "Web search is not enabled for this organization.",
        );
        assert!(off.is_web_search_disabled());
        let shown = off.to_string();
        assert!(shown.contains("set JUNEES_WEB_SEARCH=off"), "{shown}");
        assert!(api(400, None, "web_search tool is disabled").is_web_search_disabled());
        for other in [
            api(400, None, "messages: field required"),
            api(
                400,
                None,
                "web search: allowed_domains and blocked_domains given",
            ),
            api(500, None, "web search is not enabled"),
        ] {
            assert!(!other.is_web_search_disabled(), "{other}");
            assert!(!other.to_string().contains("JUNEES_WEB_SEARCH"), "{other}");
        }
    }

    #[test]
    fn detects_auth_errors() {
        assert!(api(401, None, "").is_auth());
        assert!(api(403, None, "").is_auth());
        assert!(!api(429, None, "").is_auth());
    }
}
