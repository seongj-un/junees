//! When a job runs: the schedule grammar and the next run time.
//!
//! - `every 30m`, `every 2h`, `every 1h30m`: from the time it was added on,
//!   at that interval (at least a minute).
//! - `in 20m`: once, that long after it was added.
//! - `daily 09:00`, `weekdays 09:00` (Monday to Friday): local time.
//! - a cron expression of 5 fields (`minute hour day month weekday`, local
//!   time): numbers, `*`, ranges `a-b`, steps `*/n` and `a-b/n`, lists
//!   `a,b`, month and weekday names (`jan`, `mon`); weekday 0 and 7 are
//!   Sunday. As in Vixie cron, when both the day and the weekday are
//!   restricted, either matching is enough.
//! - an ISO time, once: `2026-11-10T09:00`, `2026-11-10 09:00:30`, local,
//!   or with `Z` or an offset (`+09:00`).
//!
//! Local times follow [`TimeZone::to_utc`]: a time the clock skips runs at
//! the end of the gap, a time it repeats runs once, the first time.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::tz::{self, TimeZone, civil_from_days, days_from_civil, days_in_month, split};
use crate::task::budget::{format_duration, parse_duration};

/// Shortest interval of `every`: the daemon looks once a minute.
pub const MIN_INTERVAL: u64 = 60;

/// Longest interval of `every` and delay of `in`: 10 years. Larger values
/// would overflow the run time arithmetic.
pub const MAX_SPAN: u64 = 10 * 366 * 86_400;

/// How far ahead a cron expression is searched for its next match: a
/// February 29 can be eight years away (2096 to 2104).
const SEARCH_DAYS: i64 = 366 * 9;

const DAY: i64 = 86_400;

const SYNTAX: &str = "use `every 30m`, `in 20m`, `daily 09:00`, `weekdays 09:00`, a cron \
                      expression of 5 fields (`0 9 * * 1-5`) or a time (`2026-11-10T09:00`)";

/// A parsed schedule, as jobs.json keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Every `secs` seconds from `anchor` (unix time) on.
    Every { secs: u64, anchor: i64 },
    /// Once, at unix time `at`.
    Once { at: i64 },
    /// Every day at a local time.
    Daily { hour: u32, minute: u32 },
    /// Monday to Friday at a local time.
    Weekdays { hour: u32, minute: u32 },
    /// A 5-field cron expression, kept as written.
    Cron { expr: String },
}

impl Schedule {
    /// Parses `text`; `now` (unix time) anchors `every` and `in`, and an
    /// ISO time must come after it. `tz` reads local times.
    pub fn parse(text: &str, now: i64, tz: &TimeZone) -> Result<Self> {
        let text = text.trim();
        let mut words = text.split_whitespace();
        let first = words.next().unwrap_or_default().to_ascii_lowercase();
        let rest: Vec<&str> = words.collect();
        let one = |what: &str| -> Result<&str> {
            match rest.as_slice() {
                [value] => Ok(value),
                _ => bail!("`{what}` takes one value ({SYNTAX})"),
            }
        };
        match first.as_str() {
            "every" => {
                let secs = span(one("every")?, "every")?;
                if secs < MIN_INTERVAL {
                    bail!(
                        "`every` needs at least a minute (the schedule is checked once a minute)"
                    );
                }
                Ok(Self::Every { secs, anchor: now })
            }
            "in" => {
                let secs = span(one("in")?, "in")?;
                let at = i64::try_from(secs)
                    .ok()
                    .and_then(|secs| now.checked_add(secs))
                    .context("the time is out of range")?;
                Ok(Self::Once { at })
            }
            "daily" | "weekdays" => {
                let (hour, minute) = clock_time(one(&first)?)?;
                Ok(if first == "daily" {
                    Self::Daily { hour, minute }
                } else {
                    Self::Weekdays { hour, minute }
                })
            }
            _ if rest.len() == 4 => {
                let expr = text.split_whitespace().collect::<Vec<_>>().join(" ");
                let cron = CronExpr::parse(&expr)?;
                if cron.next_after(now, tz).is_none() {
                    bail!("the cron expression `{expr}` never matches a date");
                }
                Ok(Self::Cron { expr })
            }
            _ => match parse_time(text, tz) {
                Some(Ok(at)) if at <= now => {
                    bail!("{} is in the past", tz.format(at))
                }
                Some(Ok(at)) => Ok(Self::Once { at }),
                Some(Err(err)) => Err(err),
                None => bail!("cannot read the schedule `{text}`: {SYNTAX}"),
            },
        }
    }

    /// The first run time strictly after `after` (unix time), or `None`
    /// when a one-time job has had its time.
    pub fn next_after(&self, after: i64, tz: &TimeZone) -> Option<i64> {
        match self {
            Self::Every { secs, anchor } => {
                // jobs.json may hold any number: a time out of range is no
                // next run, not one in the past.
                let secs = i64::try_from((*secs).max(MIN_INTERVAL)).ok()?;
                if after < *anchor {
                    return anchor.checked_add(secs);
                }
                let periods = after.checked_sub(*anchor)? / secs + 1;
                anchor.checked_add(periods.checked_mul(secs)?)
            }
            Self::Once { at } => (*at > after).then_some(*at),
            Self::Daily { hour, minute } => daily(after, tz, *hour, *minute, |_| true),
            Self::Weekdays { hour, minute } => daily(after, tz, *hour, *minute, |days| {
                (1..=5).contains(&tz::weekday(days))
            }),
            Self::Cron { expr } => CronExpr::parse(expr).ok()?.next_after(after, tz),
        }
    }

    /// Whether the job runs more than once.
    pub fn repeats(&self) -> bool {
        !matches!(self, Self::Once { .. })
    }

    /// The schedule in words, with the zone for local times.
    pub fn describe(&self, tz: &TimeZone) -> String {
        match self {
            Self::Every { secs, .. } => {
                format!(
                    "every {}",
                    format_duration(std::time::Duration::from_secs(*secs))
                )
            }
            Self::Once { at } => format!("once at {}", tz.format(*at)),
            Self::Daily { hour, minute } => format!("daily {hour:02}:{minute:02} ({})", tz.name()),
            Self::Weekdays { hour, minute } => {
                format!("weekdays {hour:02}:{minute:02} ({})", tz.name())
            }
            Self::Cron { expr } => format!("cron `{expr}` ({})", tz.name()),
        }
    }
}

/// The first local `hour:minute` after `after` on a day `wanted` accepts.
fn daily(
    after: i64,
    tz: &TimeZone,
    hour: u32,
    minute: u32,
    wanted: impl Fn(i64) -> bool,
) -> Option<i64> {
    let (today, _) = split(tz.local(after));
    // A day before, in case the zone's offset moved the time across midnight.
    for days in today - 1..today + 9 {
        if !wanted(days) {
            continue;
        }
        let t = tz.to_utc(days * DAY + i64::from(hour) * 3600 + i64::from(minute) * 60);
        if t > after {
            return Some(t);
        }
    }
    None
}

/// The duration of `every` or `in`, at most [`MAX_SPAN`].
fn span(text: &str, what: &str) -> Result<u64> {
    let secs = parse_duration(text)?.as_secs();
    if secs > MAX_SPAN {
        bail!("`{what}` takes at most 10 years (got `{text}`)");
    }
    Ok(secs)
}

/// `HH:MM`, 00:00 to 23:59.
fn clock_time(text: &str) -> Result<(u32, u32)> {
    let invalid = || anyhow::anyhow!("`{text}` is not a time of day; use HH:MM, such as 09:00");
    let (h, m) = text.split_once(':').ok_or_else(invalid)?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return Err(invalid());
    }
    let hour: u32 = h.parse().map_err(|_| invalid())?;
    let minute: u32 = m.parse().map_err(|_| invalid())?;
    if hour > 23 || minute > 59 {
        return Err(invalid());
    }
    Ok((hour, minute))
}

/// An ISO time: `None` when `text` does not look like one at all.
fn parse_time(text: &str, tz: &TimeZone) -> Option<Result<i64>> {
    let bytes = text.as_bytes();
    if bytes.len() < 16 || !bytes[..4].iter().all(u8::is_ascii_digit) || bytes[4] != b'-' {
        return None;
    }
    let invalid = || {
        anyhow::anyhow!(
            "`{text}` is not a time; use 2026-11-10T09:00, with :SS, Z or an offset (+09:00) if needed"
        )
    };
    Some((|| {
        let (date, rest) = text.split_at(10);
        let rest = rest.strip_prefix(['T', ' ']).ok_or_else(invalid)?;
        let mut date_parts = date.split('-');
        let mut number = |len: usize| -> Result<u32> {
            let part = date_parts.next().ok_or_else(invalid)?;
            if part.len() != len {
                return Err(invalid());
            }
            part.parse().map_err(|_| invalid())
        };
        let (y, m, d) = (number(4)?, number(2)?, number(2)?);
        if !(1..=12).contains(&m) || d == 0 || d > days_in_month(i64::from(y), m) {
            return Err(invalid());
        }
        // The time, then maybe a zone.
        let zone_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
        let (time, zone) = rest.split_at(zone_at);
        let mut parts = time.split(':');
        let mut field = |max: u32| -> Result<Option<u32>> {
            match parts.next() {
                None => Ok(None),
                Some(part) if part.len() == 2 => match part.parse::<u32>() {
                    Ok(value) if value <= max => Ok(Some(value)),
                    _ => Err(invalid()),
                },
                Some(_) => Err(invalid()),
            }
        };
        let hour = field(23)?.ok_or_else(invalid)?;
        let minute = field(59)?.ok_or_else(invalid)?;
        let second = field(59)?.unwrap_or(0);
        if parts.next().is_some() {
            return Err(invalid());
        }
        let local = days_from_civil(i64::from(y), m, d) * DAY
            + i64::from(hour) * 3600
            + i64::from(minute) * 60
            + i64::from(second);
        Ok(match zone {
            "" => tz.to_utc(local),
            "Z" | "z" => local,
            _ => {
                let sign = if zone.starts_with('-') { -1 } else { 1 };
                let (h, m) = clock_time(&zone[1..]).map_err(|_| invalid())?;
                local - sign * i64::from(h * 3600 + m * 60)
            }
        })
    })())
}

/// A parsed 5-field cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minutes: u64,
    hours: u32,
    /// Bits 1..=31.
    days: u32,
    /// Bits 1..=12.
    months: u16,
    /// Bits 0..=6, Sunday 0.
    weekdays: u8,
    /// The day field is `*` (so only the weekday decides).
    any_day: bool,
    /// The weekday field is `*`.
    any_weekday: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl CronExpr {
    pub fn parse(expr: &str) -> Result<Self> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            bail!("a cron expression has 5 fields (minute hour day month weekday), got `{expr}`");
        };
        let context = |name: &str| format!("the {name} field of `{expr}`");
        let weekdays = field(weekday, 0, 7, &WEEKDAYS).with_context(|| context("weekday"))?;
        // 7 is Sunday too.
        let weekdays = (weekdays & 0x7f) | u64::from(weekdays & 0x80 != 0);
        Ok(Self {
            minutes: field(minute, 0, 59, &[]).with_context(|| context("minute"))?,
            hours: field(hour, 0, 23, &[]).with_context(|| context("hour"))? as u32,
            days: field(day, 1, 31, &[]).with_context(|| context("day"))? as u32,
            months: field(month, 1, 12, &MONTHS).with_context(|| context("month"))? as u16,
            weekdays: weekdays as u8,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    fn day_matches(&self, days: i64) -> bool {
        let (_, month, day) = civil_from_days(days);
        if self.months & (1 << month) == 0 {
            return false;
        }
        let by_day = self.days & (1 << day) != 0;
        let by_weekday = self.weekdays & (1 << tz::weekday(days)) != 0;
        match (self.any_day, self.any_weekday) {
            (false, false) => by_day || by_weekday,
            _ => by_day && by_weekday,
        }
    }

    /// The first matching time strictly after `after`.
    pub fn next_after(&self, after: i64, tz: &TimeZone) -> Option<i64> {
        let (today, _) = split(tz.local(after));
        for days in today - 1..today + SEARCH_DAYS {
            if !self.day_matches(days) {
                continue;
            }
            for hour in (0..24).filter(|h| self.hours & (1 << h) != 0) {
                for minute in (0..60).filter(|m| self.minutes & (1 << m) != 0) {
                    let t = tz.to_utc(days * DAY + hour * 3600 + minute * 60);
                    if t > after {
                        return Some(t);
                    }
                }
            }
        }
        None
    }
}

/// One field as a bit set of the values it allows.
fn field(text: &str, min: u32, max: u32, names: &[&str]) -> Result<u64> {
    let value = |part: &str| -> Result<u32> {
        let lower = part.to_ascii_lowercase();
        if let Some(index) = names.iter().position(|name| *name == lower) {
            return Ok(index as u32 + min);
        }
        let number: u32 = part
            .parse()
            .map_err(|_| anyhow::anyhow!("`{part}` is not a number"))?;
        if number < min || number > max {
            bail!("{number} is not within {min}-{max}");
        }
        Ok(number)
    };
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|&step| step > 0)
                    .with_context(|| format!("`{step}` is not a step"))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else if item.contains('/') {
            (value(range)?, max)
        } else {
            let single = value(range)?;
            (single, single)
        };
        if start > end {
            bail!("the range {start}-{end} runs backwards");
        }
        for v in (start..=end).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ny() -> TimeZone {
        TimeZone::from_tzif(
            "America/New_York",
            include_bytes!("../../tests/fixtures/America_New_York.tzif"),
        )
        .unwrap()
    }

    fn seoul() -> TimeZone {
        TimeZone::named_in("KST-9", std::path::Path::new("/nonexistent")).unwrap()
    }

    fn utc(y: i64, m: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, m, d) * DAY + h * 3600 + mi * 60
    }

    #[test]
    fn every_and_in_count_from_when_the_job_was_added() {
        let tz = TimeZone::utc();
        let now = utc(2026, 11, 10, 8, 7);
        let every = Schedule::parse("every 30m", now, &tz).unwrap();
        assert_eq!(
            every,
            Schedule::Every {
                secs: 1800,
                anchor: now
            }
        );
        assert_eq!(every.next_after(now, &tz), Some(now + 1800));
        // Exactly on a run time: the next one.
        assert_eq!(every.next_after(now + 1800, &tz), Some(now + 3600));
        assert_eq!(every.next_after(now + 1801, &tz), Some(now + 3600));
        assert_eq!(
            Schedule::parse("every 1h30m", now, &tz)
                .unwrap()
                .next_after(now, &tz),
            Some(now + 5400)
        );
        let once = Schedule::parse("in 20m", now, &tz).unwrap();
        assert_eq!(once, Schedule::Once { at: now + 1200 });
        assert_eq!(once.next_after(now, &tz), Some(now + 1200));
        assert_eq!(once.next_after(now + 1200, &tz), None);
        assert!(!once.repeats() && every.repeats());
    }

    #[test]
    fn huge_durations_are_refused_and_never_overflow() {
        let tz = TimeZone::utc();
        let now = utc(2026, 11, 10, 8, 7);
        // The review's probe: these were accepted, and `every` then ran
        // every minute (a next run in the past) while `in` never ran.
        for text in [
            "every 10000000000000000000s",
            "in 10000000000000000000s",
            "every 9223372036854775807s",
            "in 9223372036854775807s",
            "every 87841h",
            "in 87841h",
        ] {
            let err = Schedule::parse(text, now, &tz).unwrap_err().to_string();
            assert!(err.contains("at most 10 years"), "{text}: {err}");
        }
        let ten_years = Schedule::parse("every 87840h", now, &tz).unwrap();
        assert_eq!(ten_years.next_after(now, &tz), Some(now + 87_840 * 3600));
        assert_eq!(
            Schedule::parse("in 87840h", now, &tz).unwrap(),
            Schedule::Once {
                at: now + 87_840 * 3600
            }
        );
        // A hand-edited jobs.json: no panic, and never a time in the past.
        for secs in [u64::MAX, 10_000_000_000_000_000_000, i64::MAX as u64] {
            for anchor in [now, i64::MAX - 10, i64::MIN + 10] {
                let every = Schedule::Every { secs, anchor };
                for after in [now, i64::MAX, i64::MIN] {
                    if let Some(next) = every.next_after(after, &tz) {
                        assert!(next > after, "{secs} {anchor} {after}: {next}");
                    }
                }
            }
        }
    }

    #[test]
    fn daily_and_weekdays_follow_local_time() {
        let tz = seoul();
        // 2026-11-10 is a Tuesday; 08:59 in Seoul is 23:59Z the day before.
        let now = utc(2026, 11, 9, 23, 59);
        let daily = Schedule::parse("daily 09:00", now, &tz).unwrap();
        assert_eq!(daily.next_after(now, &tz), Some(utc(2026, 11, 10, 0, 0)));
        // At 09:00 sharp the next one is tomorrow's.
        let nine = utc(2026, 11, 10, 0, 0);
        assert_eq!(daily.next_after(nine, &tz), Some(nine + DAY));
        // Friday 09:00 -> Monday 09:00.
        let friday = utc(2026, 11, 13, 0, 0);
        let weekdays = Schedule::parse("weekdays 09:00", now, &tz).unwrap();
        assert_eq!(weekdays.next_after(friday, &tz), Some(friday + 3 * DAY));
        assert_eq!(weekdays.next_after(friday - 60, &tz), Some(friday));
    }

    #[test]
    fn daylight_saving_changes_in_new_york() {
        let tz = ny();
        // Spring forward 2026-03-08: 02:30 does not exist, it runs at 03:30 EDT.
        let schedule = Schedule::parse("daily 02:30", utc(2026, 3, 1, 0, 0), &tz).unwrap();
        let before = utc(2026, 3, 7, 12, 0);
        assert_eq!(
            schedule.next_after(before, &tz),
            Some(utc(2026, 3, 8, 7, 30))
        );
        // The day after, 02:30 EDT is 06:30Z.
        assert_eq!(
            schedule.next_after(utc(2026, 3, 8, 7, 30), &tz),
            Some(utc(2026, 3, 9, 6, 30))
        );
        // Fall back 2026-11-01: 01:30 happens twice, the job runs once (EDT).
        let schedule = Schedule::parse("daily 01:30", utc(2026, 10, 1, 0, 0), &tz).unwrap();
        let first = schedule.next_after(utc(2026, 10, 31, 12, 0), &tz).unwrap();
        assert_eq!(first, utc(2026, 11, 1, 5, 30));
        assert_eq!(
            schedule.next_after(first, &tz),
            Some(utc(2026, 11, 2, 6, 30))
        );
        // A 9:00 job stays at 9:00 local over the change.
        let nine = Schedule::parse("0 9 * * *", utc(2026, 10, 1, 0, 0), &tz).unwrap();
        assert_eq!(
            nine.next_after(utc(2026, 10, 31, 14, 0), &tz),
            Some(utc(2026, 11, 1, 14, 0))
        );
        assert_eq!(
            nine.next_after(utc(2026, 10, 30, 14, 0), &tz),
            Some(utc(2026, 10, 31, 13, 0))
        );
    }

    #[test]
    fn cron_expressions() {
        let tz = TimeZone::utc();
        let now = utc(2026, 11, 10, 8, 7); // Tuesday
        let next = |expr: &str, after: i64| {
            Schedule::parse(expr, now, &tz)
                .unwrap()
                .next_after(after, &tz)
                .unwrap()
        };
        assert_eq!(next("*/15 * * * *", now), utc(2026, 11, 10, 8, 15));
        assert_eq!(next("0 9 * * 1-5", now), utc(2026, 11, 10, 9, 0));
        assert_eq!(next("0 9 * * sat,sun", now), utc(2026, 11, 14, 9, 0));
        assert_eq!(next("0 0 * * 7", now), utc(2026, 11, 15, 0, 0));
        assert_eq!(next("30 2 1 * *", now), utc(2026, 12, 1, 2, 30));
        assert_eq!(next("0 0 1 jan *", now), utc(2027, 1, 1, 0, 0));
        assert_eq!(next("5,10-12 8 * * *", now), utc(2026, 11, 10, 8, 10));
        assert_eq!(next("0 12 10-20/5 * *", now), utc(2026, 11, 10, 12, 0));
        assert_eq!(next("0 0 29 2 *", now), utc(2028, 2, 29, 0, 0));
        // Day and weekday both restricted: either (the 13th or a Friday).
        assert_eq!(next("0 0 13 * fri", now), utc(2026, 11, 13, 0, 0));
        assert_eq!(
            next("0 0 1 * fri", utc(2026, 11, 13, 0, 0)),
            utc(2026, 11, 20, 0, 0)
        );
        // The last minute of the year rolls into the next.
        assert_eq!(
            next("0 0 * * *", utc(2026, 12, 31, 23, 59)),
            utc(2027, 1, 1, 0, 0)
        );
        assert_eq!(next("59 23 31 12 *", now), utc(2026, 12, 31, 23, 59));
    }

    #[test]
    fn iso_times_run_once() {
        let tz = seoul();
        let now = utc(2026, 11, 10, 0, 0);
        let at = |text: &str| match Schedule::parse(text, now, &tz).unwrap() {
            Schedule::Once { at } => at,
            other => panic!("{other:?}"),
        };
        assert_eq!(at("2026-11-10T18:30"), utc(2026, 11, 10, 9, 30));
        assert_eq!(at("2026-11-10 18:30:15"), utc(2026, 11, 10, 9, 30) + 15);
        assert_eq!(at("2026-11-10T18:30Z"), utc(2026, 11, 10, 18, 30));
        assert_eq!(at("2026-11-10T18:30-05:00"), utc(2026, 11, 10, 23, 30));
        assert_eq!(at("2026-11-10T18:30:00+09:00"), utc(2026, 11, 10, 9, 30));
        let err = Schedule::parse("2026-11-10T08:00", now, &tz).unwrap_err();
        assert!(err.to_string().contains("in the past"), "{err}");
    }

    #[test]
    fn wrong_schedules_say_what_is_wrong() {
        let tz = TimeZone::utc();
        let now = utc(2026, 11, 10, 0, 0);
        for (text, wanted) in [
            ("", "cannot read the schedule"),
            ("sometimes", "cannot read the schedule"),
            ("every", "takes one value"),
            ("every 30s", "at least a minute"),
            ("every 5x", "not a duration"),
            ("in 0m", "longer than 0"),
            ("daily 9", "not a time of day"),
            ("daily 24:00", "not a time of day"),
            ("daily 09:60", "not a time of day"),
            ("weekdays 09:00 10:00", "takes one value"),
            ("60 * * * *", "minute field"),
            ("* 24 * * *", "hour field"),
            ("* * 0 * *", "day field"),
            ("* * * 13 *", "month field"),
            ("* * * * 8", "weekday field"),
            ("*/0 * * * *", "not a step"),
            ("5-1 * * * *", "backwards"),
            ("* * * * mon-", "weekday field"),
            ("0 0 30 2 *", "never matches"),
            ("0 0 31 4,6 *", "never matches"),
            ("2026-02-30T09:00", "is not a time"),
            ("2026-11-10T25:00", "is not a time"),
            ("2026-11-10X09:00", "is not a time"),
            ("2026-11-10T09:00+9", "is not a time"),
            ("2026-11-10T09:00:00:00", "is not a time"),
        ] {
            let err = Schedule::parse(text, now, &tz).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains(wanted), "{text}: {message}");
        }
    }

    #[test]
    fn descriptions_name_the_zone() {
        let tz = seoul();
        let now = utc(2026, 11, 10, 0, 0);
        let describe = |text: &str| Schedule::parse(text, now, &tz).unwrap().describe(&tz);
        assert_eq!(describe("every 2h"), "every 2h");
        assert_eq!(describe("in 20m"), "once at 2026-11-10 09:20 +09:00");
        assert_eq!(describe("weekdays 09:00"), "weekdays 09:00 (KST-9)");
        assert_eq!(describe("0  9 * * *"), "cron `0 9 * * *` (KST-9)");
    }
}
