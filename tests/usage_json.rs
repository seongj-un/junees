//! JSON usage through the real binary, using only local synthetic journals.

use std::fs;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use junees::message::{ContentBlock, Message, Role, ServerToolUsage, Usage};
use junees::session::{ClaudeTurn, Entry, Journal, SessionWriter};
use serde_json::{Value, json};

mod common;
use common::junees_binary;

async fn command(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    command_with_prices(home, cwd, args, "fixture-priced=1/4/2/0.5").await
}

async fn command_with_prices(home: &Path, cwd: &Path, args: &[&str], prices: &str) -> Output {
    let mut command = junees_binary(cwd, home);
    command
        .env_clear()
        .env("HOME", home)
        .env("JUNEES_HOME", home.join("state"))
        .env("JUNEES_TIMEZONE", "UTC")
        .env("JUNEES_PRICES", prices)
        .args(["usage"])
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("junees usage did not exit")
        .unwrap()
}

fn parse(out: &Output) -> Value {
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    assert!(out.stderr.is_empty(), "{:?}", out);
    assert!(out.stdout.ends_with(b"\n"));
    // Parses all stdout: an extra object or human-readable banner is an error.
    serde_json::from_slice(&out.stdout).unwrap()
}

#[tokio::test]
async fn reports_ranked_rows_and_priced_subset_without_exposing_journal_content() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let state = home.join("state");
    let work = home.join("work");
    let other = home.join("other");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&other).unwrap();

    let (api_meta, mut api) = SessionWriter::create(&state, &work, "fixture-priced").unwrap();
    let (discord_meta, mut discord) = SessionWriter::create_for_channel(
        &state,
        &other,
        "fixture-priced",
        None,
        "discord",
        "private-channel-marker",
    )
    .unwrap();
    let (claude_meta, mut claude) =
        SessionWriter::create_for(&state, &other, "fixture-subscription", Some("claude-code"))
            .unwrap();
    let priced = Usage {
        input_tokens: 5,
        output_tokens: 6,
        cache_creation_input_tokens: 3,
        cache_read_input_tokens: 4,
        reasoning_tokens: 2,
        server_tool_use: ServerToolUsage {
            web_search_requests: 2,
        },
    };
    let credential = concat!("sk-", "ant-fixtureNotARealAPIKey");
    api.record(&Entry::Message(Message::user_text(format!(
        "private-user-marker {credential}"
    ))))
    .unwrap();
    api.record(&Entry::Message(Message {
        role: Role::User,
        content: vec![ContentBlock::tool_result(
            "private-tool-id",
            "private-tool-output",
            false,
        )],
    }))
    .unwrap();
    api.record_usage(&Entry::Usage(priced), "fixture-priced")
        .unwrap();
    // Same model in another folder. Three model rows sort by the context
    // token count (output less reasoning), not just input or model name.
    discord
        .record_usage(
            &Entry::Usage(Usage {
                input_tokens: 2,
                ..Usage::default()
            }),
            "fixture-priced",
        )
        .unwrap();
    discord
        .record_usage(
            &Entry::Usage(Usage {
                input_tokens: 1,
                output_tokens: 24,
                ..Usage::default()
            }),
            "z-fixture-unknown",
        )
        .unwrap();
    claude
        .record(&Entry::ClaudeTurn(ClaudeTurn {
            claude_session: Some("private-claude-session-marker".into()),
            input: "private-claude-input-marker".into(),
            text: "private-claude-output-marker".into(),
            usage: Usage {
                input_tokens: 18,
                ..Usage::default()
            },
            end: "answered".into(),
        }))
        .unwrap();
    let journals = [
        api.path().to_owned(),
        discord.path().to_owned(),
        claude.path().to_owned(),
    ];
    drop((api, discord, claude));
    let before = journals.each_ref().map(|path| fs::read(path).unwrap());

    let out = command(&home, &work, &["--json", "--all", "--days=3", "--by=model"]).await;
    let report = parse(&out);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["by"], "model");
    assert_eq!(report["range"]["days"], 3);
    assert_eq!(report["range"]["timezone"], "UTC");
    assert_eq!(report["scope"], json!({"all": true, "folder": null}));
    assert_eq!(report["pricing"]["scope"], "priced_api_usage_only");
    assert_eq!(report["pricing"]["overrides"], true);
    let rows = report["rows"].as_array().unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "z-fixture-unknown",
            "claude-code (fixture-subscription)",
            "fixture-priced"
        ]
    );
    assert_eq!(
        rows[0]["totals"]["cost"],
        json!({
            "estimated_usd_nanos": null, "unpriced_models": ["z-fixture-unknown"],
            "subscription": false, "base_rated_models": []
        })
    );
    assert_eq!(
        rows[1]["totals"]["cost"]["estimated_usd_nanos"],
        Value::Null
    );
    assert_eq!(rows[1]["totals"]["cost"]["subscription"], true);
    // $1/$4/$2/$0.5 per million input/output/write/read; two $0.01 searches.
    assert_eq!(
        rows[2]["totals"],
        json!({
            "input_tokens": 7, "output_tokens": 6, "cache_creation_input_tokens": 3,
            "cache_read_input_tokens": 4, "reasoning_tokens": 2, "web_search_requests": 2,
            "context_tokens": 18, "cache_hit_permille": 285,
            "cost": {"estimated_usd_nanos": 20_039_000, "unpriced_models": [],
                     "subscription": false, "base_rated_models": []}
        })
    );
    assert_eq!(report["total"]["input_tokens"], 26);
    assert_eq!(report["total"]["output_tokens"], 30);
    assert_eq!(report["total"]["web_search_requests"], 2);
    assert_eq!(report["total"]["context_tokens"], 61);
    assert_eq!(
        report["total"]["cost"],
        json!({
            "estimated_usd_nanos": 20_039_000, "unpriced_models": ["z-fixture-unknown"],
            "subscription": true, "base_rated_models": []
        })
    );
    let stdout = std::str::from_utf8(&out.stdout).unwrap();
    for marker in [
        "private-",
        &credential[..16],
        &api_meta.id,
        &discord_meta.id,
        &claude_meta.id,
    ] {
        assert!(
            !stdout.contains(marker),
            "stdout contained a private marker"
        );
    }

    let local = parse(&command(&home, &work, &["--json", "--by=source"]).await);
    assert_eq!(
        local["scope"],
        json!({"all": false, "folder": work.to_str().unwrap()})
    );
    assert_eq!(local["by"], "source");
    let rows = local["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "terminal");
    assert_eq!(local["total"]["input_tokens"], 5);

    let out = command(&home, &work, &["--by=model"]).await;
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty(), "{:?}", out);
    let text = std::str::from_utf8(&out.stdout).unwrap();
    assert!(text.starts_with("Usage in this folder ("));
    assert!(
        text.lines()
            .any(|line| line.starts_with("fixture-priced") && line.ends_with("$0.0200"))
    );
    assert!(
        text.lines()
            .any(|line| line.starts_with("total") && line.ends_with("$0.0200"))
    );
    assert_eq!(
        journals.each_ref().map(|path| fs::read(path).unwrap()),
        before
    );
}

#[tokio::test]
async fn empty_report_has_zero_counts_and_no_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let work = home.join("empty");
    fs::create_dir(&work).unwrap();
    let report = parse(&command(&home, &work, &["--json"]).await);
    assert_eq!(report["rows"], json!([]));
    assert_eq!(report["by"], "day");
    assert_eq!(report["range"]["days"], 7);
    assert_eq!(
        report["total"],
        json!({
            "input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0, "reasoning_tokens": 0, "web_search_requests": 0,
            "context_tokens": 0, "cache_hit_permille": null,
            "cost": {"estimated_usd_nanos": null, "unpriced_models": [],
                     "subscription": false, "base_rated_models": []}
        })
    );
    assert_eq!(report["journal_warnings"]["broken_lines"], 0);
    assert_eq!(report["journal_warnings"]["oversized_lines"], 0);
    assert!(!home.join("state").exists());
}

#[tokio::test]
async fn free_usage_distinguishes_cache_hit_boundaries_from_null() {
    for (case, usage, expected_hit) in [
        (
            "cache reads only",
            Usage {
                cache_read_input_tokens: 8,
                ..Usage::default()
            },
            json!(1000),
        ),
        (
            "cache writes only",
            Usage {
                cache_creation_input_tokens: 8,
                ..Usage::default()
            },
            json!(0),
        ),
        (
            "output only",
            Usage {
                output_tokens: 8,
                ..Usage::default()
            },
            Value::Null,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let state = home.join("state");
        let work = home.join("work");
        fs::create_dir(&work).unwrap();
        let (_, mut journal) = SessionWriter::create(&state, &work, "fixture-free").unwrap();
        journal
            .record_usage(&Entry::Usage(usage), "fixture-free")
            .unwrap();
        drop(journal);

        let out = command_with_prices(
            &home,
            &work,
            &["--json", "--by=model"],
            "fixture-free=0/0/0/0",
        )
        .await;
        let report = parse(&out);
        let rows = report["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{case}");
        assert_eq!(rows[0]["name"], "fixture-free", "{case}");
        for (scope, totals) in [
            ("model row", &rows[0]["totals"]),
            ("total", &report["total"]),
        ] {
            assert_eq!(totals["context_tokens"], 8, "{case}: {scope}");
            // `get` distinguishes a present JSON null from an omitted key.
            assert_eq!(
                totals.get("cache_hit_permille"),
                Some(&expected_hit),
                "{case}: {scope} cache hit rate"
            );
            assert_eq!(
                totals["cost"]["estimated_usd_nanos"].as_u64(),
                Some(0),
                "{case}: {scope} priced zero cost"
            );
        }
    }
}

#[tokio::test]
async fn json_is_a_switch_and_does_not_change_cli_errors() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let work = home.join("work");
    fs::create_dir(&work).unwrap();
    let help = command(home, &work, &["--help"]).await;
    assert_eq!(help.status.code(), Some(0));
    assert!(help.stderr.is_empty());
    assert!(
        std::str::from_utf8(&help.stdout)
            .unwrap()
            .contains("--json")
    );
    for (args, error) in [
        (&["--json=true"][..], "unknown argument `--json=true`"),
        (&["--json", "--days=0"][..], "--days takes a whole number"),
    ] {
        let out = command(home, &work, args).await;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(std::str::from_utf8(&out.stderr).unwrap().contains(error));
    }
}
