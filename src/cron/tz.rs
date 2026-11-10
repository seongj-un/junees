//! Time zones for schedules, read from the system's time zone database
//! (`/usr/share/zoneinfo`, TZif files, RFC 8536) without a dependency.
//!
//! A TZif file lists the zone's offset changes up to some year, and its
//! footer holds a POSIX `TZ` rule (`EST5EDT,M3.2.0,M11.1.0`) for every time
//! after the last listed change; slim files list almost nothing and rely on
//! the rule. Both are read here. Local times that do not exist (the hour a
//! clock skips) move forward by the gap; local times that happen twice (the
//! hour a clock repeats) take the earlier one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Where the zone files are.
pub const ZONEINFO: &str = "/usr/share/zoneinfo";

/// Seconds in a day.
const DAY: i64 = 86_400;

/// Largest TZif file read: the biggest real zone is under 4 KiB.
const MAX_FILE: u64 = 256 * 1024;

/// A time zone: a name and how its offset from UTC changes.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeZone {
    name: String,
    rules: Rules,
}

#[derive(Debug, Clone, PartialEq)]
enum Rules {
    /// The same offset (seconds east of UTC) all the time.
    Fixed(i32),
    /// A TZif file's changes, then its footer rule.
    Table {
        /// Unix times the offset changed, ascending.
        times: Vec<i64>,
        /// The offset from each change on (same length as `times`).
        offsets: Vec<i32>,
        /// The offset before the first change.
        initial: i32,
        /// For times after the last change.
        footer: Option<Posix>,
    },
    Posix(Posix),
}

/// A POSIX `TZ` rule: standard offset and, maybe, daylight saving time.
#[derive(Debug, Clone, PartialEq)]
struct Posix {
    /// Seconds east of UTC.
    std: i32,
    dst: Option<Dst>,
}

#[derive(Debug, Clone, PartialEq)]
struct Dst {
    offset: i32,
    /// DST starts at this local (standard) time...
    start: (RuleDay, i32),
    /// ...and ends at this local (daylight) time.
    end: (RuleDay, i32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RuleDay {
    /// `Jn`: day 1..=365, February 29 never counted.
    Julian(u16),
    /// `n`: day 0..=365, February 29 counted.
    Zero(u16),
    /// `Mm.w.d`: weekday `d` (0 Sunday) of week `w` (5 the last) of month `m`.
    Month { month: u8, week: u8, weekday: u8 },
}

impl TimeZone {
    pub fn utc() -> Self {
        Self {
            name: "UTC".into(),
            rules: Rules::Fixed(0),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The zone `name`: an IANA name (`Asia/Seoul`) looked up in
    /// `zoneinfo`, `UTC`, or a POSIX rule (`KST-9`, `EST5EDT,M3.2.0,M11.1.0`).
    pub fn named_in(name: &str, zoneinfo: &Path) -> Result<Self> {
        let name = name.trim();
        if matches!(name, "UTC" | "Etc/UTC" | "Z") {
            return Ok(Self::utc());
        }
        if is_zone_name(name) {
            let path = zoneinfo.join(name);
            if path.is_file() {
                return Self::from_file(name, &path);
            }
        }
        if name.chars().any(|c| c.is_ascii_digit())
            && let Ok(rule) = parse_posix(name)
        {
            return Ok(Self {
                name: name.to_string(),
                rules: Rules::Posix(rule),
            });
        }
        bail!(
            "unknown time zone `{name}`: use an IANA name such as Asia/Seoul or \
             America/New_York (the files in {})",
            zoneinfo.display()
        )
    }

    /// [`TimeZone::named_in`] the system's zone folder.
    pub fn named(name: &str) -> Result<Self> {
        Self::named_in(name, Path::new(ZONEINFO))
    }

    /// The system's zone: `TZ` (a name, or `:name`), else `/etc/localtime`,
    /// else UTC.
    pub fn system(tz: Option<String>) -> Self {
        if let Some(tz) = tz.filter(|tz| !tz.trim().is_empty()) {
            let tz = tz.trim().trim_start_matches(':');
            let found = if tz.starts_with('/') {
                Self::from_file(tz, Path::new(tz))
            } else {
                Self::named(tz)
            };
            if let Ok(zone) = found {
                return zone;
            }
        }
        let local = Path::new("/etc/localtime");
        // The link's target names the zone (`.../zoneinfo/Asia/Seoul`).
        let name = std::fs::read_link(local)
            .ok()
            .and_then(|target| zone_name_of(&target))
            .unwrap_or_else(|| "localtime".to_string());
        if name == "UTC" || name == "Etc/UTC" {
            return Self::utc();
        }
        Self::from_file(&name, local).unwrap_or_else(|_| Self::utc())
    }

    fn from_file(name: &str, path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .len();
        if size > MAX_FILE {
            bail!("{} is too big for a time zone file", path.display());
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        Self::from_tzif(name, &bytes).with_context(|| format!("cannot read {}", path.display()))
    }

    /// A zone from the bytes of a TZif file.
    pub fn from_tzif(name: &str, bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            name: name.to_string(),
            rules: parse_tzif(bytes)?,
        })
    }

    /// The offset from UTC, in seconds east, at unix time `t`.
    pub fn offset_at(&self, t: i64) -> i32 {
        match &self.rules {
            Rules::Fixed(offset) => *offset,
            Rules::Posix(rule) => rule.offset_at(t),
            Rules::Table {
                times,
                offsets,
                initial,
                footer,
            } => {
                let after = times.partition_point(|&change| change <= t);
                if after == times.len()
                    && let Some(rule) = footer
                {
                    return rule.offset_at(t);
                }
                if after == 0 {
                    *initial
                } else {
                    offsets[after - 1]
                }
            }
        }
    }

    /// The unix time of local wall-clock time `local` (seconds since
    /// 1970-01-01 00:00 local). A skipped local time moves forward by the
    /// gap (02:30 on a spring-forward night is 03:30); a repeated one is the
    /// earlier of the two.
    pub fn to_utc(&self, local: i64) -> i64 {
        let before = self.offset_at(local - DAY);
        let after = self.offset_at(local + DAY);
        let mut found: Option<i64> = None;
        for offset in [before, self.offset_at(local), after] {
            let t = local - i64::from(offset);
            if self.offset_at(t) == offset {
                found = Some(found.map_or(t, |best| best.min(t)));
            }
        }
        found.unwrap_or(local - i64::from(before))
    }

    /// Local wall-clock seconds of unix time `t`.
    pub fn local(&self, t: i64) -> i64 {
        t + i64::from(self.offset_at(t))
    }

    /// `YYYY-MM-DD HH:MM` local time and the offset (`+09:00`).
    pub fn format(&self, t: i64) -> String {
        let offset = self.offset_at(t);
        let (date, secs) = split(t + i64::from(offset));
        let (y, m, d) = civil_from_days(date);
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.unsigned_abs();
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02} {sign}{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            abs / 3600,
            abs / 60 % 60
        )
    }
}

/// Whether `name` can be a path under the zone folder: no `..`, no
/// leading `/`, only the characters zone names use.
fn is_zone_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

/// `Asia/Seoul` from `/usr/share/zoneinfo/Asia/Seoul`.
fn zone_name_of(target: &Path) -> Option<String> {
    let text = target.to_str()?;
    let (_, name) = text.rsplit_once("zoneinfo/")?;
    Some(name.to_string())
}

/// Days since 1970-01-01 and seconds into the day.
pub fn split(local: i64) -> (i64, i64) {
    (local.div_euclid(DAY), local.rem_euclid(DAY))
}

/// Days since 1970-01-01 of a Gregorian date (Howard Hinnant's
/// `days_from_civil`).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The Gregorian date of a day count since 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Day of the week of a day count, 0 for Sunday.
pub fn weekday(days: i64) -> u32 {
    // 1970-01-01 was a Thursday.
    (days + 4).rem_euclid(7) as u32
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Posix {
    fn offset_at(&self, t: i64) -> i32 {
        let Some(dst) = &self.dst else {
            return self.std;
        };
        let (days, _) = split(t + i64::from(self.std));
        let (year, _, _) = civil_from_days(days);
        let start =
            rule_day(dst.start.0, year) * DAY + i64::from(dst.start.1) - i64::from(self.std);
        let end = rule_day(dst.end.0, year) * DAY + i64::from(dst.end.1) - i64::from(dst.offset);
        let in_dst = if start <= end {
            start <= t && t < end
        } else {
            // Southern hemisphere: DST over the new year.
            !(end <= t && t < start)
        };
        if in_dst { dst.offset } else { self.std }
    }
}

/// Days since 1970-01-01 of a rule's day in `year`.
fn rule_day(day: RuleDay, year: i64) -> i64 {
    let jan1 = days_from_civil(year, 1, 1);
    match day {
        RuleDay::Julian(n) => {
            let n = i64::from(n);
            jan1 + n - 1 + i64::from(is_leap(year) && n >= 60)
        }
        RuleDay::Zero(n) => jan1 + i64::from(n),
        RuleDay::Month {
            month,
            week,
            weekday: wanted,
        } => {
            let month = u32::from(month);
            let first = days_from_civil(year, month, 1);
            let first_match = (i64::from(wanted) - i64::from(weekday(first))).rem_euclid(7);
            let mut day = first_match + 7 * (i64::from(week) - 1);
            let length = i64::from(days_in_month(year, month));
            while day >= length {
                day -= 7;
            }
            first + day
        }
    }
}

/// Parses a POSIX `TZ` rule such as `KST-9`, `<+09>-9` or
/// `EST5EDT,M3.2.0,M11.1.0`.
fn parse_posix(text: &str) -> Result<Posix> {
    let mut rest = text;
    let invalid = || anyhow::anyhow!("`{text}` is not a POSIX TZ rule");
    take_name(&mut rest).ok_or_else(invalid)?;
    let std = -take_time(&mut rest).ok_or_else(invalid)?;
    if rest.is_empty() {
        return Ok(Posix { std, dst: None });
    }
    take_name(&mut rest).ok_or_else(invalid)?;
    let offset = if rest.is_empty() || rest.starts_with(',') {
        std + 3600
    } else {
        -take_time(&mut rest).ok_or_else(invalid)?
    };
    let (start, end) = if rest.is_empty() {
        // The POSIX default is implementation-defined; tzcode uses US rules.
        (
            (
                RuleDay::Month {
                    month: 3,
                    week: 2,
                    weekday: 0,
                },
                7200,
            ),
            (
                RuleDay::Month {
                    month: 11,
                    week: 1,
                    weekday: 0,
                },
                7200,
            ),
        )
    } else {
        rest = rest.strip_prefix(',').ok_or_else(invalid)?;
        let start = take_rule(&mut rest).ok_or_else(invalid)?;
        rest = rest.strip_prefix(',').ok_or_else(invalid)?;
        let end = take_rule(&mut rest).ok_or_else(invalid)?;
        if !rest.is_empty() {
            return Err(invalid());
        }
        (start, end)
    };
    Ok(Posix {
        std,
        dst: Some(Dst { offset, start, end }),
    })
}

fn take_name(rest: &mut &str) -> Option<()> {
    if let Some(quoted) = rest.strip_prefix('<') {
        let end = quoted.find('>')?;
        if end < 3 {
            return None;
        }
        *rest = &quoted[end + 1..];
        return Some(());
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    if end < 3 {
        return None;
    }
    *rest = &rest[end..];
    Some(())
}

/// `[+-]hh[:mm[:ss]]` in seconds (as written: west of UTC is positive).
fn take_time(rest: &mut &str) -> Option<i32> {
    let (sign, body) = match rest.as_bytes().first()? {
        b'-' => (-1, &rest[1..]),
        b'+' => (1, &rest[1..]),
        _ => (1, *rest),
    };
    let end = body
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(body.len());
    let mut total = 0i32;
    let mut parts = body[..end].split(':');
    for (index, unit) in [3600, 60, 1].into_iter().enumerate() {
        match parts.next() {
            Some(part) if !part.is_empty() && part.len() <= 3 => {
                let value: i32 = part.parse().ok()?;
                if index > 0 && value > 59 {
                    return None;
                }
                total += value * unit;
            }
            None if index > 0 => break,
            _ => return None,
        }
    }
    if parts.next().is_some() || total > 167 * 3600 {
        return None;
    }
    *rest = &body[end..];
    Some(sign * total)
}

fn take_rule(rest: &mut &str) -> Option<(RuleDay, i32)> {
    let end = rest.find([',', '/']).unwrap_or(rest.len());
    let day_text = &rest[..end];
    let day = if let Some(n) = day_text.strip_prefix('J') {
        let n: u16 = n.parse().ok()?;
        (1..=365).contains(&n).then_some(RuleDay::Julian(n))?
    } else if let Some(m) = day_text.strip_prefix('M') {
        let mut parts = m.split('.');
        let month: u8 = parts.next()?.parse().ok()?;
        let week: u8 = parts.next()?.parse().ok()?;
        let weekday: u8 = parts.next()?.parse().ok()?;
        if parts.next().is_some()
            || !(1..=12).contains(&month)
            || !(1..=5).contains(&week)
            || weekday > 6
        {
            return None;
        }
        RuleDay::Month {
            month,
            week,
            weekday,
        }
    } else {
        let n: u16 = day_text.parse().ok()?;
        (n <= 365).then_some(RuleDay::Zero(n))?
    };
    *rest = &rest[end..];
    let time = match rest.strip_prefix('/') {
        Some(after) => {
            *rest = after;
            take_time(rest)?
        }
        None => 7200,
    };
    Some((day, time))
}

/// Reads a TZif file (versions 1 to 4): the 64-bit block when there is one.
fn parse_tzif(bytes: &[u8]) -> Result<Rules> {
    let header = Header::read(bytes, 0)?;
    if header.version == 0 {
        return header.rules(bytes, 44, 4, None);
    }
    let second_start = 44 + header.block_len(4);
    let second = Header::read(bytes, second_start)?;
    let data = second_start + 44;
    let footer_start = data + second.block_len(8);
    let footer = bytes
        .get(footer_start..)
        .context("the TZif file is cut short")?;
    let footer = footer
        .strip_prefix(b"\n")
        .and_then(|rest| {
            rest.iter()
                .position(|&b| b == b'\n')
                .map(|end| &rest[..end])
        })
        .context("the TZif file has no footer")?;
    let footer = std::str::from_utf8(footer).context("the TZif footer is not text")?;
    let rule = if footer.is_empty() {
        None
    } else {
        Some(parse_posix(footer)?)
    };
    second.rules(bytes, data, 8, rule)
}

struct Header {
    version: u8,
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl Header {
    fn read(bytes: &[u8], at: usize) -> Result<Self> {
        let head = bytes
            .get(at..at + 44)
            .context("the TZif file is cut short")?;
        if &head[..4] != b"TZif" {
            bail!("not a TZif file");
        }
        let count = |index: usize| {
            let start = 20 + index * 4;
            u32::from_be_bytes([
                head[start],
                head[start + 1],
                head[start + 2],
                head[start + 3],
            ]) as usize
        };
        let version = match head[4] {
            0 => 0,
            b'2'..=b'9' => head[4] - b'0',
            other => bail!("unknown TZif version {other}"),
        };
        let header = Self {
            version,
            isutcnt: count(0),
            isstdcnt: count(1),
            leapcnt: count(2),
            timecnt: count(3),
            typecnt: count(4),
            charcnt: count(5),
        };
        if header.typecnt == 0 {
            bail!("the TZif file has no local time types");
        }
        Ok(header)
    }

    /// Bytes of the data block with `time_size`-byte times.
    fn block_len(&self, time_size: usize) -> usize {
        self.timecnt * time_size
            + self.timecnt
            + self.typecnt * 6
            + self.charcnt
            + self.leapcnt * (time_size + 4)
            + self.isstdcnt
            + self.isutcnt
    }

    fn rules(
        &self,
        bytes: &[u8],
        data: usize,
        time_size: usize,
        footer: Option<Posix>,
    ) -> Result<Rules> {
        let block = bytes
            .get(data..data + self.block_len(time_size))
            .context("the TZif file is cut short")?;
        let mut times = Vec::with_capacity(self.timecnt);
        for index in 0..self.timecnt {
            let at = &block[index * time_size..(index + 1) * time_size];
            times.push(if time_size == 8 {
                i64::from_be_bytes(at.try_into().expect("8 bytes"))
            } else {
                i64::from(i32::from_be_bytes(at.try_into().expect("4 bytes")))
            });
        }
        if times.windows(2).any(|pair| pair[0] >= pair[1]) {
            bail!("the TZif transition times are not ascending");
        }
        let indexes = &block[self.timecnt * time_size..self.timecnt * (time_size + 1)];
        let types_at = self.timecnt * (time_size + 1);
        let mut type_offsets = Vec::with_capacity(self.typecnt);
        for index in 0..self.typecnt {
            let at = &block[types_at + index * 6..types_at + index * 6 + 4];
            let offset = i32::from_be_bytes(at.try_into().expect("4 bytes"));
            if offset.unsigned_abs() > 26 * 3600 {
                bail!("the TZif file has an offset of {offset} seconds");
            }
            type_offsets.push(offset);
        }
        let mut offsets = Vec::with_capacity(self.timecnt);
        for &index in indexes {
            offsets.push(
                *type_offsets
                    .get(usize::from(index))
                    .context("the TZif file names a missing time type")?,
            );
        }
        if times.is_empty()
            && let Some(rule) = footer
        {
            return Ok(Rules::Posix(rule));
        }
        Ok(Rules::Table {
            times,
            offsets,
            initial: type_offsets[0],
            footer,
        })
    }
}

/// The zone folder from `TZDIR` if set, else [`ZONEINFO`].
pub fn zoneinfo_dir(tzdir: Option<String>) -> PathBuf {
    tzdir
        .filter(|dir| dir.starts_with('/'))
        .map_or_else(|| PathBuf::from(ZONEINFO), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// America/New_York as the system had it (fat TZif, version 2+).
    const NEW_YORK: &[u8] = include_bytes!("../../tests/fixtures/America_New_York.tzif");

    fn utc(y: i64, m: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, m, d) * DAY + h * 3600 + mi * 60
    }

    fn zones() -> Vec<TimeZone> {
        vec![
            TimeZone::from_tzif("America/New_York", NEW_YORK).unwrap(),
            TimeZone {
                name: "EST5EDT".into(),
                rules: Rules::Posix(parse_posix("EST5EDT,M3.2.0,M11.1.0").unwrap()),
            },
        ]
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in [-800_000, -1, 0, 1, 11_016, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(weekday(days_from_civil(2026, 11, 10)), 2);
    }

    #[test]
    fn new_york_offsets_around_both_changes() {
        // 2026: DST from March 8 07:00Z to November 1 06:00Z.
        for zone in zones() {
            let spring = utc(2026, 3, 8, 7, 0);
            let fall = utc(2026, 11, 1, 6, 0);
            assert_eq!(zone.offset_at(spring - 1), -5 * 3600, "{}", zone.name);
            assert_eq!(zone.offset_at(spring), -4 * 3600, "{}", zone.name);
            assert_eq!(zone.offset_at(fall - 1), -4 * 3600, "{}", zone.name);
            assert_eq!(zone.offset_at(fall), -5 * 3600, "{}", zone.name);
            // Past the table of a fat file (2037): the footer rule.
            assert_eq!(zone.offset_at(utc(2060, 7, 1, 0, 0)), -4 * 3600);
            assert_eq!(zone.offset_at(utc(2060, 1, 1, 0, 0)), -5 * 3600);
        }
        // Before the first change: local mean time (-4:56:02).
        let ny = &zones()[0];
        assert_eq!(ny.offset_at(utc(1850, 1, 1, 0, 0)), -17_762);
    }

    #[test]
    fn skipped_local_times_move_forward_and_repeated_ones_take_the_first() {
        for zone in zones() {
            // 02:30 does not exist on 2026-03-08: it is 03:30 EDT (07:30Z).
            let local = utc(2026, 3, 8, 2, 30);
            assert_eq!(zone.to_utc(local), utc(2026, 3, 8, 7, 30), "{}", zone.name);
            // 01:30 happens twice on 2026-11-01: the first is EDT (05:30Z).
            let local = utc(2026, 11, 1, 1, 30);
            assert_eq!(zone.to_utc(local), utc(2026, 11, 1, 5, 30), "{}", zone.name);
            // An ordinary time.
            let local = utc(2026, 7, 4, 9, 0);
            assert_eq!(zone.to_utc(local), utc(2026, 7, 4, 13, 0));
            assert_eq!(
                zone.format(utc(2026, 7, 4, 13, 0)),
                "2026-07-04 09:00 -04:00"
            );
        }
    }

    #[test]
    fn posix_rules() {
        let seoul = parse_posix("KST-9").unwrap();
        assert_eq!(seoul.offset_at(0), 9 * 3600);
        let quoted = parse_posix("<+0530>-5:30").unwrap();
        assert_eq!(quoted.offset_at(0), 5 * 3600 + 1800);
        // Southern hemisphere: Sydney, DST from October to April.
        let sydney = parse_posix("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.offset_at(utc(2026, 1, 15, 0, 0)), 11 * 3600);
        assert_eq!(sydney.offset_at(utc(2026, 6, 15, 0, 0)), 10 * 3600);
        // Julian day rules (J60 is March 1 even in leap years).
        let j = parse_posix("AAA0BBB,J60/0,J300/0").unwrap();
        assert_eq!(j.offset_at(utc(2028, 2, 29, 12, 0)), 0);
        assert_eq!(j.offset_at(utc(2028, 3, 1, 12, 0)), 3600);
        for bad in [
            "",
            "K-9",
            "KST",
            "KST-9X",
            "EST5EDT,M13.1.0,M11.1.0",
            "EST5EDT,M3.2.0",
        ] {
            assert!(parse_posix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn zone_names_cannot_leave_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("America")).unwrap();
        std::fs::write(dir.path().join("America/New_York"), NEW_YORK).unwrap();
        let zone = TimeZone::named_in("America/New_York", dir.path()).unwrap();
        assert_eq!(zone.name(), "America/New_York");
        assert_eq!(zone.offset_at(utc(2026, 7, 1, 0, 0)), -4 * 3600);
        for bad in [
            "../etc/passwd",
            "/etc/localtime",
            "America/../America/New_York",
            "Mars/Base",
        ] {
            let err = TimeZone::named_in(bad, dir.path()).unwrap_err();
            assert!(
                err.to_string().contains("unknown time zone"),
                "{bad}: {err}"
            );
        }
        assert_eq!(
            TimeZone::named_in("UTC", dir.path()).unwrap(),
            TimeZone::utc()
        );
        let posix = TimeZone::named_in("KST-9", dir.path()).unwrap();
        assert_eq!(posix.offset_at(0), 9 * 3600);
    }

    #[test]
    fn broken_files_are_errors() {
        assert!(TimeZone::from_tzif("x", b"TZif2").is_err());
        assert!(TimeZone::from_tzif("x", &NEW_YORK[..100]).is_err());
        assert!(
            TimeZone::from_tzif(
                "x",
                b"not a zone file at all, but long enough to have a header...."
            )
            .is_err()
        );
    }
}
