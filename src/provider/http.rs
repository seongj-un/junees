//! What the HTTP providers share: timeouts, which failures are worth
//! another attempt, and how long to wait before it.

use std::time::Duration;

use reqwest::StatusCode;

use super::ProviderError;

pub(super) const MAX_ATTEMPTS: u32 = 4;
/// Bounds one whole model call that is not streamed.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a streamed call may wait for its response to start and then
/// for each next event. A stream has no limit on its whole length: the
/// docs advise streaming for long requests, and a long answer goes on as
/// long as events keep coming. The API sends tool input one whole value
/// at a time, so a large file to write can mean a long pause; at the old
/// whole-call limit, no call that used to finish can time out here.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub(super) const MAX_RETRY_AFTER_SECS: u64 = 60;

/// One failed attempt and whether another one may help.
pub(super) struct Failure {
    pub error: ProviderError,
    pub retryable: bool,
    pub retry_after: Option<u64>,
}

pub(super) fn send_failure(err: reqwest::Error) -> Failure {
    Failure {
        // is_request covers connections closed before a response.
        retryable: err.is_timeout() || err.is_connect() || err.is_request(),
        retry_after: None,
        error: ProviderError::Transport(Box::new(err)),
    }
}

/// A body cut off midway is a transport problem worth retrying.
pub(super) fn body_failure(err: reqwest::Error) -> Failure {
    Failure {
        retryable: true,
        retry_after: None,
        error: ProviderError::Transport(Box::new(err)),
    }
}

pub(super) fn idle_failure(idle: Duration) -> Failure {
    Failure {
        retryable: true,
        retry_after: None,
        error: ProviderError::Transport(
            format!("no response from the API for {} s", idle.as_secs_f32()).into(),
        ),
    }
}

pub(super) fn retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

pub(super) fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 || status.is_server_error()
}

/// For an error that arrived inside a stream, which carries the status its
/// type has in a plain response.
pub(super) fn is_retryable_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Api { status, .. } => StatusCode::from_u16(*status).is_ok_and(is_retryable),
        ProviderError::Transport(_) => true,
        ProviderError::Decode(_) => false,
    }
}

/// Wait before retry number `attempt` (1-based). Honors `retry-after` when the
/// server sends one, otherwise backs off exponentially from 500ms.
pub(super) fn backoff(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs {
        Some(secs) => Duration::from_secs(secs.min(MAX_RETRY_AFTER_SECS)),
        None => Duration::from_millis(500 * 2u64.pow(attempt.saturating_sub(1).min(6))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::from_u16(529).unwrap()));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn backoff_grows_and_respects_retry_after() {
        assert_eq!(backoff(1, None), Duration::from_millis(500));
        assert_eq!(backoff(2, None), Duration::from_millis(1000));
        assert_eq!(backoff(3, None), Duration::from_millis(2000));
        assert_eq!(backoff(1, Some(7)), Duration::from_secs(7));
        assert_eq!(
            backoff(1, Some(3600)),
            Duration::from_secs(MAX_RETRY_AFTER_SECS)
        );
    }
}
