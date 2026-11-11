use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::args::{self, By, UsageArgs};
use super::prices::{self, Prices, cost, dollars};
use super::*;
use crate::cron::tz::{self, TimeZone};
use crate::message::ServerToolUsage;
use crate::session::{Entry, Journal, SessionWriter};

fn usage(input: u64, write: u64, read: u64, output: u64, searches: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: write,
        cache_read_input_tokens: read,
        server_tool_use: ServerToolUsage {
            web_search_requests: searches,
        },
    }
}

/// Unix seconds of a UTC date and time.
fn at(y: i64, m: u32, d: u32, h: u64, min: u64) -> u64 {
    tz::days_from_civil(y, m, d) as u64 * 86_400 + h * 3600 + min * 60
}

fn zone(name: &str) -> TimeZone {
    TimeZone::named_in(name, Path::new("/nonexistent-zoneinfo")).unwrap()
}

/// Writes a journal by hand: the meta line, then `lines`.
fn journal(home: &Path, id: &str, meta: Value, lines: &[Value]) -> PathBuf {
    let dir = home.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let mut meta_line = json!({
        "type": "meta", "id": id, "format": 6, "created_at": at(2026, 11, 1, 0, 0),
        "cwd": "/work", "model": "claude-sonnet-4-6", "version": "0",
    });
    for (key, value) in meta.as_object().unwrap() {
        meta_line[key] = value.clone();
    }
    let mut text = meta_line.to_string() + "\n";
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    let path = dir.join(format!("{id}.jsonl"));
    fs::write(&path, text).unwrap();
    path
}

fn usage_line(kind: &str, ts: u64, input: u64, output: u64, model: Option<&str>) -> Value {
    let mut line = json!({"type": kind, "input_tokens": input, "output_tokens": output, "ts": ts});
    if let Some(model) = model {
        line["model"] = json!(model);
    }
    line
}

const MTOK: u64 = 1_000_000;

#[test]
fn prices_follow_the_table_and_its_cache_rates() {
    let prices = Prices::default();
    let sonnet = prices.get("claude-sonnet-4-6").unwrap();
    // $3 input, $3.75 5-minute cache write (1.25x), $0.30 cache read
    // (0.1x), $15 output per million tokens; $10 per 1,000 searches.
    assert_eq!(cost(&sonnet, &usage(MTOK, 0, 0, 0, 0), true), 3_000_000_000);
    assert_eq!(cost(&sonnet, &usage(0, MTOK, 0, 0, 0), true), 3_750_000_000);
    assert_eq!(cost(&sonnet, &usage(0, 0, MTOK, 0, 0), true), 300_000_000);
    assert_eq!(
        cost(&sonnet, &usage(0, 0, 0, MTOK, 0), true),
        15_000_000_000
    );
    assert_eq!(
        cost(&sonnet, &usage(0, 0, 0, 0, 1000), true),
        10_000_000_000
    );
    assert_eq!(
        dollars(cost(&sonnet, &usage(1000, 2000, 30_000, 500, 2), true)),
        "$0.0470"
    );
    // Cache reads at 0.05x on Opus 5.5, 0.025x on Fable 5.2.
    let opus = prices.get("claude-opus-5-5").unwrap();
    assert_eq!(cost(&opus, &usage(0, 0, MTOK, 0, 0), true), 200_000_000);
    let fable = prices.get("claude-fable-5-2").unwrap();
    assert_eq!(cost(&fable, &usage(0, 0, MTOK, 0, 0), true), 200_000_000);
    assert_eq!(fable.rates.input, 8_000);
    // Dated ids of older models find their row; a shorter id is not a
    // prefix match.
    assert_eq!(
        prices.get("claude-haiku-4-5-20251001"),
        prices.get("claude-haiku-4-5")
    );
    assert_eq!(prices.get("claude-opus-4").unwrap().rates.input, 15_000);
    assert_eq!(prices.get("claude-opus-4-5").unwrap().rates.input, 5_000);
    assert_eq!(prices.get("claude-opus-4-5-2"), None);
    assert_eq!(prices.get("gpt-4"), None);
    assert_eq!(prices.get(""), None);
}

#[test]
fn every_table_row_keeps_the_documented_ratios() {
    for (model, price) in prices::TABLE {
        let rates = price.rates;
        assert_eq!(rates.cache_write * 4, rates.input * 5, "{model}");
        let read_ratio = rates.input / rates.cache_read;
        assert!(
            [10, 20, 40].contains(&read_ratio),
            "{model}: cache read is 1/{read_ratio} of input"
        );
    }
}

#[test]
fn a_long_prompt_raises_the_price_of_one_call_only() {
    let prices = Prices::default();
    let haiku = prices.get("claude-haiku-5-5").unwrap();
    // 100,000 prompt tokens: the base rates.
    let short = usage(50_000, 0, 50_000, 1000, 0);
    assert_eq!(
        cost(&haiku, &short, true),
        50_000 * 100 + 50_000 * 10 + 1000 * 500
    );
    // One more token: every count of the call at the higher rates.
    let long = usage(50_001, 0, 50_000, 1000, 0);
    assert_eq!(
        cost(&haiku, &long, true),
        50_001 * 500 + 50_000 * 50 + 1000 * 2500
    );
    // A sum of several calls (subagents) says nothing about one prompt.
    assert_eq!(
        cost(&haiku, &long, false),
        50_001 * 100 + 50_000 * 10 + 1000 * 500
    );
}

#[test]
fn the_prices_setting_adds_and_replaces_models() {
    let prices = Prices::from_setting(Some(
        " claude-sonnet-4-6=1/2 , my-proxy-model=0.5/2.5/0.6/0.05,,claude-sonnet-4-6=2/8",
    ))
    .unwrap();
    assert!(prices.overridden());
    // The last entry for a model wins; cache rates follow input.
    let sonnet = prices.get("claude-sonnet-4-6").unwrap();
    assert_eq!(
        sonnet.rates,
        prices::Rates {
            input: 2_000,
            output: 8_000,
            cache_write: 2_500,
            cache_read: 200
        }
    );
    let proxy = prices.get("my-proxy-model").unwrap();
    assert_eq!(
        proxy.rates,
        prices::Rates {
            input: 500,
            output: 2_500,
            cache_write: 600,
            cache_read: 50
        }
    );
    // The rest of the table stays.
    assert_eq!(prices.get("claude-opus-4-7").unwrap().rates.input, 5_000);
    // A dated id finds a setting given without the date.
    let dated = Prices::from_setting(Some("claude-haiku-4-5=9/9")).unwrap();
    assert_eq!(
        dated.get("claude-haiku-4-5-20251001").unwrap().rates.input,
        9_000
    );
    assert!(!Prices::from_setting(None).unwrap().overridden());
    assert!(!Prices::from_setting(Some("  ")).unwrap().overridden());

    for bad in [
        "claude-x",
        "=1/2",
        "claude-x=1",
        "claude-x=1/2/3",
        "claude-x=1/2/3/4/5",
        "claude-x=1.2345/2",
        "claude-x=-1/2",
        "claude-x=1./2",
        "claude-x=.5/2",
        "claude-x=2000000/2",
        "claude-x=one/two",
        "my model=1/2",
    ] {
        let err = Prices::from_setting(Some(bad)).unwrap_err();
        assert!(
            err.starts_with("JUNEES_PRICES (`prices`): `"),
            "{bad}: {err}"
        );
    }
    let err = Prices::from_setting(Some("x\u{202e}=1")).unwrap_err();
    assert!(err.contains("\\u{202e}"), "{err}");
}

#[test]
fn two_prices_keep_the_cache_rates_of_the_table() {
    let rates = |setting: &str, model: &str| {
        Prices::from_setting(Some(setting))
            .unwrap()
            .get(model)
            .unwrap()
            .rates
    };
    // Writing the table's own prices changes nothing: Opus 5.5 reads the
    // cache at 0.05x input, not 0.1x.
    assert_eq!(
        rates("claude-opus-5-5=4/20", "claude-opus-5-5"),
        Prices::default().get("claude-opus-5-5").unwrap().rates
    );
    // New prices keep the table's ratios: 1.25x write, 0.025x read.
    assert_eq!(
        rates("claude-fable-5-2=16/80", "claude-fable-5-2"),
        prices::Rates {
            input: 16_000,
            output: 80_000,
            cache_write: 20_000,
            cache_read: 400
        }
    );
    // A dated id finds its row too.
    assert_eq!(
        rates(
            "claude-sonnet-5-5-20261001=1/5",
            "claude-sonnet-5-5-20261001"
        ),
        prices::Rates {
            input: 1_000,
            output: 5_000,
            cache_write: 1_250,
            cache_read: 50
        }
    );
    // A model the table does not know: 1.25x and 0.1x.
    assert_eq!(
        rates("my-proxy-model=1/4", "my-proxy-model"),
        prices::Rates {
            input: 1_000,
            output: 4_000,
            cache_write: 1_250,
            cache_read: 100
        }
    );
    // Four prices are taken as written.
    assert_eq!(
        rates("claude-opus-5-5=4/20/5/0.4", "claude-opus-5-5").cache_read,
        400
    );
}

#[test]
fn dollars_round_to_a_hundredth_of_a_cent() {
    assert_eq!(dollars(0), "$0.0000");
    assert_eq!(dollars(49_999), "$0.0000");
    assert_eq!(dollars(50_000), "$0.0001");
    assert_eq!(dollars(12_345_678_901), "$12.3457");
    // The most a sum can hold (sums saturate), not rounded up past it.
    assert_eq!(dollars(u64::MAX), "$18446744073.7095");
}

#[test]
fn every_usage_entry_kind_is_read_with_its_model() {
    let home = tempfile::tempdir().unwrap();
    let path = journal(
        home.path(),
        "20261101-000000-aaaa",
        json!({}),
        &[
            json!({"type": "message", "role": "user", "content": [{"type": "text", "text": "usage"}]}),
            usage_line("usage", 100, 10, 1, Some("claude-opus-4-7")),
            // Lines from before `model` (and `ts`) existed.
            json!({"type": "usage", "input_tokens": 20, "output_tokens": 2,
                   "cache_creation_input_tokens": 5, "cache_read_input_tokens": 7,
                   "server_tool_use": {"web_search_requests": 3}}),
            usage_line("summary_usage", 300, 30, 3, None),
            usage_line("subagent_usage", 400, 40, 4, Some("claude-haiku-5-5")),
            json!({"type": "format", "format": 6}),
            json!({"type": "compact", "summary": "s", "keep_from": 1}),
            // A line written another way still counts.
            json!({"ts": 500, "output_tokens": 5, "type": "usage", "input_tokens": 50}),
            json!({"type": "usage", "input_tokens": "many"}),
            json!({"type": "some_later_entry", "x": 1}),
        ],
    );
    // A broken line and a torn last line.
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("{\"type\":\"usage\",\"input_tok\nnot json at all\n{\"type\":\"usage\",\"inp");
    fs::write(&path, text).unwrap();

    let (records, stats) = read_session(&path).unwrap();
    let created = at(2026, 11, 1, 0, 0);
    let expected = [
        (100, Kind::Call, "claude-opus-4-7", usage(10, 0, 0, 1, 0)),
        (
            created,
            Kind::Call,
            "claude-sonnet-4-6",
            usage(20, 5, 7, 2, 3),
        ),
        (
            300,
            Kind::Summary,
            "claude-sonnet-4-6",
            usage(30, 0, 0, 3, 0),
        ),
        (
            400,
            Kind::Subagents,
            "claude-haiku-5-5",
            usage(40, 0, 0, 4, 0),
        ),
        (500, Kind::Call, "claude-sonnet-4-6", usage(50, 0, 0, 5, 0)),
    ]
    .map(|(ts, kind, model, usage)| Record {
        ts,
        kind,
        model: model.to_string(),
        usage,
    });
    assert_eq!(records, expected);
    // `"many"`, the cut line, `not json` and the torn line.
    assert_eq!(stats.broken_lines, 4);
    assert_eq!(stats.journals, 1);

    // A session with no journal yet, and a file that is no journal.
    let (records, _) = read_session(&home.path().join("missing.jsonl")).unwrap();
    assert!(records.is_empty());
    let other = home.path().join("other.jsonl");
    fs::write(&other, "{\"type\":\"usage\",\"input_tokens\":1}\n").unwrap();
    assert!(read_session(&other).unwrap().0.is_empty());
}

#[test]
fn lines_the_session_writer_writes_are_read() {
    let home = tempfile::tempdir().unwrap();
    let (_, mut writer) =
        SessionWriter::create(home.path(), Path::new("/w"), "claude-sonnet-4-6").unwrap();
    writer
        .record(&Entry::Message(crate::message::Message::user_text(
            "{\"type\":\"usage\"",
        )))
        .unwrap();
    writer
        .record_usage(&Entry::Usage(usage(1, 2, 3, 4, 0)), "claude-opus-5-5")
        .unwrap();
    writer.record(&Entry::Usage(usage(5, 0, 0, 6, 0))).unwrap();
    let (records, stats) = read_session(writer.path()).unwrap();
    let models: Vec<&str> = records.iter().map(|r| r.model.as_str()).collect();
    assert_eq!(models, ["claude-opus-5-5", "claude-sonnet-4-6"]);
    assert_eq!(stats.broken_lines, 0);
    assert!(records.iter().all(|r| r.ts > 1_700_000_000));
}

#[test]
fn claude_code_turns_count_as_subscription_tokens() {
    let home = tempfile::tempdir().unwrap();
    let path = journal(
        home.path(),
        "20261101-000000-cccc",
        json!({"backend": "claude-code", "model": ""}),
        &[
            json!({"type": "claude_turn", "input": "hi", "text": "hello",
                 "usage": {"input_tokens": 10, "output_tokens": 3, "cache_read_input_tokens": 90},
                 "end": "answered", "ts": 100}),
        ],
    );
    let (records, _) = read_session(&path).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, Kind::ClaudeCode);
    let live = Live {
        usage: usage(10, 0, 90, 3, 0),
        model: String::new(),
        claude_code: true,
    };
    let text = session(
        Some("20261101-000000-cccc"),
        &records,
        &Stats::default(),
        &live,
        &Prices::default(),
    );
    let lines: Vec<&str> = text.lines().collect();
    let words = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(lines.len(), 5, "{text}");
    assert_eq!(
        lines[0],
        "Session 20261101-000000-cccc, subagents included:"
    );
    assert_eq!(
        words(lines[1]),
        "input cache write cache read output web searches cache hit cost"
    );
    assert_eq!(
        words(lines[2]),
        "Claude Code turns 10 0 90 3 0 90.0% subscription"
    );
    // The columns line up: numbers right-aligned to the same end.
    assert_eq!(lines[1].len(), lines[2].len(), "{text}");
    assert_eq!(
        lines[3],
        "Cache hit is cache read / (cache read + cache write + input)."
    );
    assert_eq!(
        lines[4],
        "Claude Code turns run on your Claude subscription: tokens only, no cost."
    );
}

#[test]
fn the_session_report_counts_every_kind_and_what_the_journal_missed() {
    let records = vec![
        Record {
            ts: 1,
            kind: Kind::Call,
            model: "claude-sonnet-4-6".into(),
            usage: usage(1000, 2000, 30_000, 500, 2),
        },
        Record {
            ts: 2,
            kind: Kind::Call,
            model: "claude-sonnet-4-6".into(),
            usage: usage(1000, 0, 0, 500, 0),
        },
        Record {
            ts: 3,
            kind: Kind::Summary,
            model: "claude-sonnet-4-6".into(),
            usage: usage(4000, 0, 0, 1000, 0),
        },
        Record {
            ts: 4,
            kind: Kind::Subagents,
            model: "claude-sonnet-4-6".into(),
            usage: usage(10_000, 0, 0, 2000, 0),
        },
        Record {
            ts: 5,
            kind: Kind::Call,
            model: "mystery-model".into(),
            usage: usage(7, 0, 0, 7, 0),
        },
    ];
    let mut saved = Usage::default();
    for record in &records {
        saved.add(record.usage);
    }
    // The engine counted one more call than the journal holds.
    let mut live = saved;
    live.add(usage(1_000_000, 0, 0, 0, 0));
    let live = Live {
        usage: live,
        model: "claude-opus-4-7".into(),
        claude_code: false,
    };
    let stats = Stats {
        journals: 1,
        broken_lines: 2,
        long_lines: 1,
    };
    let text = session(Some("s-1"), &records, &stats, &live, &Prices::default());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "Session s-1, subagents included:");
    assert!(
        lines[1].starts_with(' ') && lines[1].ends_with("cost"),
        "{text}"
    );
    let row = |label: &str| {
        lines
            .iter()
            .find(|line| line.starts_with(label))
            .unwrap_or_else(|| panic!("no {label} row in\n{text}"))
            .split_whitespace()
            .rev()
            .take(2)
            .collect::<Vec<_>>()
    };
    // 2000 input * $3 + 2000 write * $3.75 + 30000 read * $0.30 + 1000
    // output * $15 per million + 2 searches * $0.01; 30000 of 34000
    // prompt tokens read from the cache.
    assert_eq!(row("model calls (claude-sonnet-4-6)"), ["$0.0575", "88.2%"]);
    assert_eq!(row("summaries (claude-sonnet-4-6)"), ["$0.0270", "0.0%"]);
    assert_eq!(row("subagents (claude-sonnet-4-6)"), ["$0.0600", "0.0%"]);
    assert_eq!(row("model calls (mystery-model)"), ["price", "no"]);
    // 1M input on the model running now: $5.
    assert_eq!(
        row("not in the journal (claude-opus-4-7)"),
        ["$5.0000", "0.0%"]
    );
    assert!(text.contains("\ntotal  "), "{text}");
    assert_eq!(row("total"), ["price", "no"]);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("total") && line.contains("$5.1445 + no price")),
        "{text}"
    );
    assert!(text.contains(prices::SOURCE), "{text}");
    assert!(
        text.contains("No price is known for mystery-model: their tokens are counted"),
        "{text}"
    );
    assert!(
        text.contains("2 journal line(s) could not be read"),
        "{text}"
    );
    assert!(text.contains("1 journal line(s) over 4 MiB"), "{text}");

    // Without a journal everything is unsaved, priced on the model now.
    let live = Live {
        usage: usage(MTOK, 0, 0, 0, 0),
        model: "claude-sonnet-4-6".into(),
        claude_code: false,
    };
    let text = session(None, &[], &Stats::default(), &live, &Prices::default());
    assert!(
        text.starts_with("This session (not saved), subagents included:\n"),
        "{text}"
    );
    assert!(text.contains("\nnot saved (claude-sonnet-4-6)  "), "{text}");
    assert!(text.contains("$3.0000"), "{text}");
    assert!(!text.contains("\ntotal"), "{text}");

    let none = Live {
        usage: Usage::default(),
        model: "m".into(),
        claude_code: false,
    };
    assert_eq!(
        session(
            Some("s-2"),
            &[],
            &Stats::default(),
            &none,
            &Prices::default()
        ),
        "Session s-2, subagents included:\nno tokens used yet\n"
    );
}

/// A state folder with sessions in two folders and from two sources,
/// around midnight UTC between 2026-11-10 and 2026-11-11.
fn period_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let late = at(2026, 11, 10, 23, 30);
    let early = at(2026, 11, 11, 0, 30);
    journal(
        home.path(),
        "20261110-233000-aaaa",
        json!({"created_at": late}),
        &[
            usage_line("usage", late, 1000, 100, None),
            usage_line("usage", early, 2000, 200, Some("claude-opus-4-7")),
            usage_line("subagent_usage", early, 500, 50, None),
        ],
    );
    journal(
        home.path(),
        "20261111-003000-bbbb",
        json!({"created_at": early, "cwd": "/other", "source": "cron", "channel": "job-1"}),
        &[usage_line("usage", early, 4000, 400, Some("mystery-model"))],
    );
    // A subagent journal: its tokens are in the parent's subagent_usage.
    let sub = home.path().join("sessions/20261110-233000-aaaa");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("sub-1.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"type": "meta", "id": "20261110-233000-aaaa/sub-1", "format": 6,
                   "created_at": early, "cwd": "/work", "model": "claude-sonnet-4-6", "version": "0"}),
            usage_line("usage", early, 500, 50, None)
        ),
    )
    .unwrap();
    home
}

fn run(home: &Path, days: u32, by: By, all: bool, zone_name: &str, now: u64) -> String {
    period(
        home,
        Path::new("/work"),
        &UsageArgs { days, by, all },
        &Prices::default(),
        &zone(zone_name),
        now,
    )
    .unwrap()
}

#[test]
fn days_split_at_midnight_of_the_configured_zone() {
    let home = period_home();
    let now = at(2026, 11, 11, 12, 0);
    let utc = run(home.path(), 7, By::Day, true, "UTC", now);
    assert!(
        utc.starts_with(
            "Usage in every folder the last 7 days (2026-11-05 to 2026-11-11, UTC), by day:\n"
        ),
        "{utc}"
    );
    let input = |text: &str, label: &str| -> Option<String> {
        text.lines()
            .find(|line| line.starts_with(label))
            .map(|line| line.split_whitespace().nth(1).unwrap().to_string())
    };
    assert_eq!(input(&utc, "2026-11-10").as_deref(), Some("1,000"));
    assert_eq!(input(&utc, "2026-11-11").as_deref(), Some("6,500"));
    assert_eq!(input(&utc, "total").as_deref(), Some("7,500"));

    // In Seoul (UTC+9) both are on the 11th.
    let seoul = run(home.path(), 7, By::Day, true, "KST-9", now);
    assert_eq!(input(&seoul, "2026-11-10"), None, "{seoul}");
    assert_eq!(input(&seoul, "2026-11-11").as_deref(), Some("7,500"));
    // In New York (UTC-5) both are on the 10th.
    let new_york = run(home.path(), 7, By::Day, true, "EST5", now);
    assert_eq!(input(&new_york, "2026-11-10").as_deref(), Some("7,500"));

    // --days 1 is today from local midnight: the 23:30 UTC call is out.
    let today = run(home.path(), 1, By::Day, true, "UTC", now);
    assert!(
        today.starts_with("Usage in every folder today (2026-11-11, UTC), by day:\n"),
        "{today}"
    );
    assert_eq!(input(&today, "total").as_deref(), Some("6,500"));
    // In Seoul today started at 15:00 UTC on the 10th.
    let today = run(home.path(), 1, By::Day, true, "KST-9", now);
    assert_eq!(input(&today, "total").as_deref(), Some("7,500"));
    // Days later both are outside a 1-day range, inside a 3-day one.
    let later = at(2026, 11, 13, 12, 0);
    let none = run(home.path(), 1, By::Day, true, "UTC", later);
    assert!(
        none.starts_with("No usage recorded in every folder today (2026-11-13, UTC).\n"),
        "{none}"
    );
    let three = run(home.path(), 3, By::Day, true, "UTC", later);
    assert_eq!(input(&three, "total").as_deref(), Some("6,500"), "{three}");
}

#[test]
fn rows_by_model_folder_and_source() {
    let home = period_home();
    let now = at(2026, 11, 11, 12, 0);
    let rows = |by: By, all: bool| -> Vec<String> {
        run(home.path(), 7, by, all, "UTC", now)
            .lines()
            .skip(2)
            .take_while(|line| !line.starts_with("total"))
            .map(|line| {
                let mut words = line.split("  ").filter(|w| !w.is_empty());
                format!("{} {}", words.next().unwrap(), words.next().unwrap().trim())
            })
            .collect()
    };
    // Most tokens first.
    assert_eq!(
        rows(By::Model, true),
        [
            "mystery-model 4,000",
            "claude-opus-4-7 2,000",
            "claude-sonnet-4-6 1,500"
        ]
    );
    assert_eq!(rows(By::Folder, true), ["/other 4,000", "/work 3,500"]);
    assert_eq!(rows(By::Source, true), ["cron 4,000", "terminal 3,500"]);
    // Without --all, this folder only.
    assert_eq!(rows(By::Folder, false), ["/work 3,500"]);
    let text = run(home.path(), 7, By::Source, false, "UTC", now);
    assert!(
        text.starts_with("Usage in this folder (/work) the last 7 days"),
        "{text}"
    );
    assert!(!text.contains("No price"), "{text}");
    let text = run(home.path(), 7, By::Source, true, "UTC", now);
    assert!(
        text.contains("No price is known for mystery-model"),
        "{text}"
    );
    assert!(text.contains(" + no price\n"), "{text}");
}

#[test]
fn journals_last_written_before_the_range_are_not_read() {
    let home = period_home();
    let path = home.path().join("sessions/20261110-233000-aaaa.jsonl");
    let old = UNIX_EPOCH + Duration::from_secs(at(2026, 11, 1, 0, 0));
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let text = run(
        home.path(),
        7,
        By::Folder,
        true,
        "UTC",
        at(2026, 11, 11, 12, 0),
    );
    assert!(!text.contains("/work"), "{text}");
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    let text = run(
        home.path(),
        7,
        By::Folder,
        true,
        "UTC",
        at(2026, 11, 11, 12, 0),
    );
    assert!(text.contains("/work"), "{text}");
}

#[test]
fn no_sessions_folder_is_no_usage() {
    let home = tempfile::tempdir().unwrap();
    let text = run(
        home.path(),
        7,
        By::Day,
        false,
        "UTC",
        at(2026, 11, 11, 0, 0),
    );
    assert_eq!(
        text,
        "No usage recorded in this folder (/work) the last 7 days (2026-11-05 to \
         2026-11-11, UTC).\n--all shows every folder.\n"
    );
}

#[test]
fn usage_arguments() {
    let parse = |args: &[&str]| args::parse(args.iter().map(|arg| arg.to_string()));
    assert_eq!(parse(&[]), Ok(Some(UsageArgs::default())));
    assert_eq!(
        parse(&["--days", "30", "--by", "model", "--all"]),
        Ok(Some(UsageArgs {
            days: 30,
            by: By::Model,
            all: true
        }))
    );
    assert_eq!(
        parse(&["--days=1", "--by=source"]),
        Ok(Some(UsageArgs {
            days: 1,
            by: By::Source,
            all: false
        }))
    );
    assert_eq!(parse(&["--all", "--help"]), Ok(None));
    for (args, error) in [
        (&["--days"][..], "--days needs a number of days"),
        (
            &["--days", "0"][..],
            "--days takes a whole number from 1 to 3660, not `0`",
        ),
        (&["--days", "-3"][..], "not `-3`"),
        (&["--days", "9999"][..], "not `9999`"),
        (
            &["--by", "week"][..],
            "--by takes day, model, folder or source, not `week`",
        ),
        (&["--all=yes"][..], "unknown argument `--all=yes`"),
        (&["today"][..], "unknown argument `today`"),
    ] {
        let err = parse(args).unwrap_err();
        assert!(err.contains(error), "{args:?}: {err}");
    }
}

#[test]
fn totals_name_what_their_cost_leaves_out() {
    let mut totals = Totals::default();
    assert_eq!(totals.cost_text(), "$0.0000");
    assert_eq!(totals.cache_hit_permille(), None);
    let prices = Prices::default();
    let record = |kind, model: &str| Record {
        ts: 0,
        kind,
        model: model.into(),
        usage: usage(10, 0, 30, 1, 0),
    };
    totals.add(&record(Kind::ClaudeCode, ""), &prices);
    assert_eq!(totals.cost_text(), "subscription");
    totals.add(&record(Kind::Call, "x"), &prices);
    assert_eq!(totals.cost_text(), "no price + subscription");
    totals.add(&record(Kind::Call, "claude-haiku-4-5"), &prices);
    assert_eq!(totals.cost_text(), "$0.0000 + no price + subscription");
    assert_eq!(totals.cache_hit_permille(), Some(750));
    // An empty usage entry (a call that failed early) needs no price.
    let mut empty = Totals::default();
    empty.add(
        &Record {
            usage: Usage::default(),
            ..record(Kind::Call, "x")
        },
        &prices,
    );
    assert!(empty.unpriced.is_empty());
    assert_eq!(report::thousands(0), "0");
    assert_eq!(report::thousands(999), "999");
    assert_eq!(report::thousands(1000), "1,000");
    assert_eq!(report::thousands(1_234_567), "1,234,567");
}
