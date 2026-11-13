//! The one-time code the opened browser trades for the token. Opening a
//! browser puts its address on a command line (`xdg-open`, the browser),
//! which other processes of the computer can read in `/proc`, unattended
//! runs included; so that address carries a code, not the token. The code
//! works once, for [`LIFETIME`], and [`MAX_FAILURES`] wrong codes end it,
//! so what is left on a command line is of no use.

use std::time::{Duration, Instant};

/// How long a code works after it is made.
pub const LIFETIME: Duration = Duration::from_secs(60);
/// Wrong codes that end the current one.
pub const MAX_FAILURES: u32 = 5;

struct Code {
    value: String,
    made: Instant,
    failures: u32,
}

/// The code that may be traded now, if any.
#[derive(Default)]
pub struct Codes {
    current: Option<Code>,
}

impl Codes {
    /// Makes `value` the code to trade, from `now` on; an earlier one ends.
    pub fn issue(&mut self, value: String, now: Instant) {
        self.current = Some(Code {
            value,
            made: now,
            failures: 0,
        });
    }

    /// Ends the current code, if any.
    pub fn clear(&mut self) {
        self.current = None;
    }

    /// Whether `given` is the current code at `now`. A right code ends it
    /// (it works once), as do an expired one and the [`MAX_FAILURES`]th
    /// wrong one. Compared in constant time.
    pub fn redeem(&mut self, given: &str, now: Instant) -> bool {
        let Some(code) = &mut self.current else {
            return false;
        };
        if now.saturating_duration_since(code.made) > LIFETIME {
            self.current = None;
            return false;
        }
        if super::server::constant_time_eq(given.as_bytes(), code.value.as_bytes()) {
            self.current = None;
            return true;
        }
        code.failures += 1;
        if code.failures >= MAX_FAILURES {
            self.current = None;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_works_once() {
        let now = Instant::now();
        let mut codes = Codes::default();
        assert!(!codes.redeem("abc", now), "no code yet");
        codes.issue("abc".into(), now);
        assert!(codes.redeem("abc", now + Duration::from_secs(59)));
        assert!(!codes.redeem("abc", now + Duration::from_secs(59)));
    }

    #[test]
    fn a_code_ends_after_a_minute() {
        let now = Instant::now();
        let mut codes = Codes::default();
        codes.issue("abc".into(), now);
        assert!(!codes.redeem("abc", now + LIFETIME + Duration::from_millis(1)));
        assert!(!codes.redeem("abc", now), "ended, not just late");
    }

    #[test]
    fn five_wrong_codes_end_it() {
        let now = Instant::now();
        let mut codes = Codes::default();
        codes.issue("abc".into(), now);
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!codes.redeem("abd", now));
        }
        assert!(codes.redeem("abc", now), "four wrong ones leave it");
        codes.issue("xyz".into(), now);
        for wrong in ["", "xy", "xyzz", "XYZ", "xyy"] {
            assert!(!codes.redeem(wrong, now));
        }
        assert!(!codes.redeem("xyz", now), "the fifth wrong one ended it");
    }

    #[test]
    fn a_cleared_code_does_not_work() {
        let now = Instant::now();
        let mut codes = Codes::default();
        codes.issue("abc".into(), now);
        codes.clear();
        assert!(!codes.redeem("abc", now));
    }

    #[test]
    fn a_new_code_replaces_the_old() {
        let now = Instant::now();
        let mut codes = Codes::default();
        codes.issue("old".into(), now);
        codes.issue("new".into(), now);
        assert!(!codes.redeem("old", now));
        assert!(codes.redeem("new", now));
    }
}
