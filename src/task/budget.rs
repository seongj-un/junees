//! The token and time budget of `task run`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::agent::Limit;
use crate::message::Usage;

/// Reads the current time; tests pass a fake one.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Limits on a task beyond its session count. Without either, a run is
/// limited by `--max-sessions` alone.
#[derive(Clone)]
pub struct Budget {
    /// Tokens every session of the task may use together, over all runs
    /// (see [`tokens`]). Kept in state.json, so a new run goes on counting.
    pub max_tokens: Option<u64>,
    /// Wall-clock time of one `task run`, from its start.
    pub max_time: Option<Duration>,
    pub clock: Clock,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_tokens: None,
            max_time: None,
            clock: Arc::new(Instant::now),
        }
    }
}

impl Budget {
    pub fn now(&self) -> Instant {
        (self.clock)()
    }

    /// Why the run has to stop, if it has to: `tokens` used by the task so
    /// far, and the time since the run started at `started`.
    pub fn exceeded(&self, tokens: u64, started: Instant) -> Option<String> {
        if let Some(max) = self.max_tokens
            && tokens >= max
        {
            return Some(format!("token budget used up: {tokens} of {max} tokens"));
        }
        let elapsed = self.now().saturating_duration_since(started);
        if let Some(max) = self.max_time
            && elapsed >= max
        {
            return Some(format!(
                "time budget used up: {} of {}",
                format_duration(elapsed),
                format_duration(max)
            ));
        }
        None
    }

    /// The check a session's agent runs before each model call, given the
    /// tokens the task used before the session. `None` without a budget.
    pub fn session_limit(&self, used_before: u64, started: Instant) -> Option<Limit> {
        if self.max_tokens.is_none() && self.max_time.is_none() {
            return None;
        }
        let budget = self.clone();
        Some(Box::new(move |usage| {
            budget.exceeded(used_before + tokens(usage), started)
        }))
    }
}

/// Tokens counted against the budget: everything the model read (cache
/// writes and reads included) and wrote.
pub fn tokens(usage: Usage) -> u64 {
    usage.input_tokens
        + usage.cache_creation_input_tokens
        + usage.cache_read_input_tokens
        + usage.output_tokens
}

/// Parses `90s`, `30m`, `2h` or combinations such as `1h30m`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let invalid = || {
        anyhow::anyhow!(
            "`{text}` is not a duration; use a number with s, m or h (for example 90s, 30m, 2h, 1h30m)"
        )
    };
    let mut total: u64 = 0;
    let mut digits = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            _ => return Err(invalid()),
        };
        let value: u64 = digits.parse().map_err(|_| invalid())?;
        digits.clear();
        total = value
            .checked_mul(unit)
            .and_then(|secs| total.checked_add(secs))
            .ok_or_else(invalid)?;
    }
    if !digits.is_empty() || text.is_empty() {
        return Err(invalid());
    }
    if total == 0 {
        bail!("the duration must be longer than 0, got `{text}`");
    }
    Ok(Duration::from_secs(total))
}

/// `1h2m3s`, leaving out zero parts (`2h`, `90s` as `1m30s`).
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut text = String::new();
    if h > 0 {
        text.push_str(&format!("{h}h"));
    }
    if m > 0 {
        text.push_str(&format!("{m}m"));
    }
    if s > 0 || text.is_empty() {
        text.push_str(&format!("{s}s"));
    }
    text
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A clock that stands still until `secs` is moved forward.
    pub fn fake_clock(secs: Arc<AtomicU64>) -> Clock {
        let start = Instant::now();
        Arc::new(move || start + Duration::from_secs(secs.load(Ordering::SeqCst)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::testing::fake_clock;
    use super::*;

    #[test]
    fn parses_durations() {
        for (text, secs) in [
            ("90s", 90),
            ("30m", 1800),
            ("2h", 7200),
            (" 1h30m ", 5400),
            ("1h0m5s", 3605),
            ("0h1s", 1),
        ] {
            assert_eq!(parse_duration(text).unwrap().as_secs(), secs, "{text}");
        }
        for bad in [
            "", "90", "m", "1x", "1.5h", "-1m", "1 h", "2d", "0s", "0h0m",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert!(parse_duration("99999999999999999999h").is_err());
        assert!(
            parse_duration("1d")
                .unwrap_err()
                .to_string()
                .contains("for example 90s, 30m, 2h")
        );
    }

    #[test]
    fn formats_durations() {
        for (secs, text) in [
            (0, "0s"),
            (59, "59s"),
            (90, "1m30s"),
            (7200, "2h"),
            (3723, "1h2m3s"),
        ] {
            assert_eq!(format_duration(Duration::from_secs(secs)), text);
        }
    }

    #[test]
    fn counts_every_token_and_checks_both_limits() {
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 4,
        };
        assert_eq!(tokens(usage), 10);

        let secs = Arc::new(AtomicU64::new(0));
        let budget = Budget {
            max_tokens: Some(100),
            max_time: Some(Duration::from_secs(60)),
            clock: fake_clock(secs.clone()),
        };
        let started = budget.now();
        assert_eq!(budget.exceeded(99, started), None);
        assert_eq!(
            budget.exceeded(100, started).as_deref(),
            Some("token budget used up: 100 of 100 tokens")
        );
        secs.store(61, Ordering::SeqCst);
        assert_eq!(
            budget.exceeded(0, started).as_deref(),
            Some("time budget used up: 1m1s of 1m")
        );

        // A session's check adds the tokens used before it.
        let mut limit = budget.session_limit(95, budget.now()).unwrap();
        assert_eq!(limit(Usage::default()), None);
        assert!(limit(usage).unwrap().contains("105 of 100"));
        assert!(Budget::default().session_limit(0, started).is_none());
    }
}
