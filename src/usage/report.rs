//! The text of `/usage` and `junees usage`.

use std::collections::BTreeMap;
use std::path::Path;

use super::args::{By, UsageArgs};
use super::prices::{self, Prices};
use super::scan::{self, Stats};
use super::{Kind, Record, Totals, model_name};
use crate::cron::tz::{self, TimeZone};
use crate::message::Usage;
use crate::prompt_text::escape_hidden;

/// What the running conversation used, as its engine counts it. The
/// journal can hold less: nothing with `--no-session`, and nothing after a
/// write to it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub usage: Usage,
    /// The model the conversation runs on now.
    pub model: String,
    /// The conversation runs on Claude Code (the subscription).
    pub claude_code: bool,
}

/// `/usage`: the tokens of the session `id` by kind and model, from its
/// journal `records`, its subagents included, with their cost.
pub fn session(
    id: Option<&str>,
    records: &[Record],
    stats: &Stats,
    live: &Live,
    prices: &Prices,
) -> String {
    let mut groups: BTreeMap<(Kind, String), Totals> = BTreeMap::new();
    let mut saved = Usage::default();
    for record in records {
        saved.add(record.usage);
        groups
            .entry((record.kind, record.model.clone()))
            .or_default()
            .add(record, prices);
    }
    let mut rows: Vec<(String, Totals)> = groups
        .into_iter()
        .map(|((kind, model), totals)| {
            let label = if model.is_empty() {
                kind.label().to_string()
            } else {
                format!("{} ({})", kind.label(), model_name(&model))
            };
            (label, totals)
        })
        .collect();
    let unsaved = minus(live.usage, saved);
    if unsaved != Usage::default() {
        let mut totals = Totals::default();
        let record = Record {
            ts: 0,
            kind: if live.claude_code {
                Kind::ClaudeCode
            } else {
                Kind::Call
            },
            model: live.model.clone(),
            usage: unsaved,
            one_call: false, // a sum of calls: priced at the base rates
        };
        totals.add(&record, prices);
        let label = if id.is_some() {
            "not in the journal"
        } else {
            "not saved"
        };
        let label = if live.model.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({})", model_name(&live.model))
        };
        rows.push((label, totals));
    }
    let mut out = match id {
        Some(id) => format!("Session {}, subagents included:\n", escape_hidden(id)),
        None => "This session (not saved), subagents included:\n".to_string(),
    };
    if rows.is_empty() {
        out.push_str("no tokens used yet\n");
        return out;
    }
    let mut total = Totals::default();
    for (_, totals) in &rows {
        total.merge(totals);
    }
    let show_total = rows.len() > 1;
    out.push_str(&table(&rows, show_total.then_some(&total)));
    out.push_str(&notes(&total, stats, prices));
    out
}

/// Each count of `a` less that of `b`, not below 0.
fn minus(a: Usage, b: Usage) -> Usage {
    let mut out = a;
    out.input_tokens = a.input_tokens.saturating_sub(b.input_tokens);
    out.output_tokens = a.output_tokens.saturating_sub(b.output_tokens);
    out.cache_creation_input_tokens = a
        .cache_creation_input_tokens
        .saturating_sub(b.cache_creation_input_tokens);
    out.cache_read_input_tokens = a
        .cache_read_input_tokens
        .saturating_sub(b.cache_read_input_tokens);
    out.reasoning_tokens = a.reasoning_tokens.saturating_sub(b.reasoning_tokens);
    out.server_tool_use.web_search_requests = a.web_searches().saturating_sub(b.web_searches());
    out
}

/// `junees usage`: the tokens and cost of the sessions of the state
/// folder `home` over the days `args` names, in `zone`, as of `now` (Unix
/// seconds); only those started in `cwd` unless `args.all`.
pub fn period(
    home: &Path,
    cwd: &Path,
    args: &UsageArgs,
    prices: &Prices,
    zone: &TimeZone,
    now: u64,
) -> anyhow::Result<String> {
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let (today, _) = tz::split(zone.local(now));
    let first = today - i64::from(args.days.max(1) - 1);
    let since = u64::try_from(zone.to_utc(first * 86_400)).unwrap_or(0);
    let cwd = cwd.to_string_lossy();
    let folder = (!args.all).then_some(cwd.as_ref());

    let mut groups: BTreeMap<String, Totals> = BTreeMap::new();
    let stats = scan::scan(home, since, folder, prices, &mut |origin, record| {
        let key = match args.by {
            By::Day => {
                let ts = i64::try_from(record.ts).unwrap_or(i64::MAX);
                date(tz::split(zone.local(ts)).0)
            }
            By::Model => model_key(&record),
            By::Folder => escape_hidden(&origin.cwd),
            By::Source => origin.source.clone(),
        };
        groups.entry(key).or_default().add(&record, prices);
    })?;

    let place = if args.all {
        "in every folder".to_string()
    } else {
        format!("in this folder ({})", escape_hidden(&cwd))
    };
    let zone_name = escape_hidden(zone.name());
    let range = if args.days == 1 {
        format!("today ({}, {zone_name})", date(today))
    } else {
        format!(
            "the last {} days ({} to {}, {zone_name})",
            args.days,
            date(first),
            date(today)
        )
    };
    if groups.is_empty() {
        let mut out = format!("No usage recorded {place} {range}.\n");
        if !args.all {
            out.push_str("--all shows every folder.\n");
        }
        out.push_str(&notes(&Totals::default(), &stats, prices));
        return Ok(out);
    }
    let by = match args.by {
        By::Day => "day",
        By::Model => "model",
        By::Folder => "folder",
        By::Source => "source",
    };
    let mut rows: Vec<(String, Totals)> = groups.into_iter().collect();
    if args.by != By::Day {
        rows.sort_by(|a, b| b.1.tokens().cmp(&a.1.tokens()).then_with(|| a.0.cmp(&b.0)));
    }
    let mut total = Totals::default();
    for (_, totals) in &rows {
        total.merge(totals);
    }
    let mut out = format!("Usage {place} {range}, by {by}:\n");
    out.push_str(&table(&rows, Some(&total)));
    out.push_str(&notes(&total, &stats, prices));
    Ok(out)
}

/// What a record's tokens are grouped under by model: the model's name,
/// or `claude-code (<model>)` for a Claude Code turn.
fn model_key(record: &Record) -> String {
    if record.kind != Kind::ClaudeCode {
        return model_name(&record.model);
    }
    if record.model.is_empty() {
        "claude-code".to_string()
    } else {
        format!("claude-code ({})", escape_hidden(&record.model))
    }
}

/// One row of [`daily`]: the tokens of one model on one day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayRow {
    /// `YYYY-MM-DD` in `zone`.
    pub date: String,
    /// As `junees usage --by model` names it.
    pub model: String,
    pub totals: Totals,
}

/// The usage of the last `days` days in `zone` (today included), in every
/// folder, by day and model: the oldest day first, and on each day the
/// models by name.
pub fn daily(
    home: &Path,
    days: u32,
    prices: &Prices,
    zone: &TimeZone,
    now: u64,
) -> anyhow::Result<Vec<DayRow>> {
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let (today, _) = tz::split(zone.local(now));
    let first = today - i64::from(days.max(1) - 1);
    let since = u64::try_from(zone.to_utc(first * 86_400)).unwrap_or(0);
    let mut groups: BTreeMap<(String, String), Totals> = BTreeMap::new();
    scan::scan(home, since, None, prices, &mut |_, record| {
        let ts = i64::try_from(record.ts).unwrap_or(i64::MAX);
        let day = date(tz::split(zone.local(ts)).0);
        groups
            .entry((day, model_key(&record)))
            .or_default()
            .add(&record, prices);
    })?;
    Ok(groups
        .into_iter()
        .map(|((date, model), totals)| DayRow {
            date,
            model,
            totals,
        })
        .collect())
}

/// `YYYY-MM-DD` of a day number (days since 1970-01-01).
fn date(days: i64) -> String {
    let (y, m, d) = tz::civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `1,234,567`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

const COLUMNS: [&str; 8] = [
    "",
    "input",
    "cache write",
    "cache read",
    "output",
    "web searches",
    "cache hit",
    "cost",
];

fn cells(label: &str, totals: &Totals) -> [String; 8] {
    let usage = &totals.usage;
    [
        label.to_string(),
        thousands(usage.input_tokens),
        thousands(usage.cache_creation_input_tokens),
        thousands(usage.cache_read_input_tokens),
        thousands(usage.output_tokens),
        thousands(usage.web_searches()),
        totals.cache_hit_permille().map_or_else(
            || "-".to_string(),
            |permille| format!("{}.{}%", permille / 10, permille % 10),
        ),
        totals.cost_text(),
    ]
}

/// The rows, then the total, in columns: the label left-aligned, the
/// numbers right-aligned.
fn table(rows: &[(String, Totals)], total: Option<&Totals>) -> String {
    let mut lines: Vec<[String; 8]> = vec![COLUMNS.map(str::to_string)];
    lines.extend(rows.iter().map(|(label, totals)| cells(label, totals)));
    if let Some(total) = total {
        lines.push(cells("total", total));
    }
    let mut widths = [0; 8];
    for line in &lines {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for line in &lines {
        let mut text = String::new();
        for (n, (cell, width)) in line.iter().zip(widths).enumerate() {
            if n == 0 {
                text.push_str(&format!("{cell:<width$}"));
            } else {
                text.push_str(&format!("  {cell:>width$}"));
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
    }
    out
}

/// What the numbers mean and what they leave out.
fn notes(total: &Totals, stats: &Stats, prices: &Prices) -> String {
    let mut out = String::new();
    if total.usage != Usage::default() {
        out.push_str("Cache hit is cache read / (cache read + cache write + input).\n");
    }
    if total.priced {
        out.push_str(&format!(
            "The cost is an estimate from the Claude API list prices ({}, checked {}){}, \
             cache writes as 5-minute writes.\n",
            prices::SOURCE,
            prices::CHECKED,
            if prices.overridden() {
                " and the `prices` setting"
            } else {
                ""
            }
        ));
    }
    if !total.unpriced.is_empty() {
        let models: Vec<&str> = total.unpriced.iter().map(String::as_str).collect();
        out.push_str(&format!(
            "No price is known for {}: their tokens are counted but left out of the cost \
             (`prices` in config.toml sets one; see junees usage --help).\n",
            models.join(", ")
        ));
    }
    if !total.base_rated.is_empty() {
        let models: Vec<&str> = total.base_rated.iter().map(String::as_str).collect();
        out.push_str(&format!(
            "Some tokens of {} are only known as sums of several calls (subagents whose \
             own journals are missing or do not add up to what the session recorded, or \
             tokens not in the journal) and are priced at the base rates: calls with long \
             prompts cost more, so the cost may be low.\n",
            models.join(", ")
        ));
    }
    if total.subscription {
        out.push_str("Claude Code turns run on your Claude subscription: tokens only, no cost.\n");
    }
    if stats.broken_lines > 0 {
        out.push_str(&format!(
            "{} journal line(s) could not be read and were left out.\n",
            stats.broken_lines
        ));
    }
    if stats.long_lines > 0 {
        out.push_str(&format!(
            "{} journal line(s) over {} MiB were skipped unread.\n",
            stats.long_lines,
            crate::search::MAX_LINE_BYTES / (1024 * 1024)
        ));
    }
    out
}
