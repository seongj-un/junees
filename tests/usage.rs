//! Usage and cost through the real binary: `/usage` in a conversation
//! against a fake API, and `junees usage` over real-format journals.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use junees::message::{ContentBlock, Message, Role, ServerToolUsage, Usage};
use junees::session::{Entry, Journal, SessionWriter};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{fake_api, junees_binary};

fn reply(text: &str, usage: Value) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": usage
    })
}

/// `YYYY-MM-DD` (UTC) of `days_ago` days before now.
fn utc_date(days_ago: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (y, m, d) = junees::cron::tz::civil_from_days(now.div_euclid(86_400) - days_ago);
    format!("{y:04}-{m:02}-{d:02}")
}

async fn usage_command(
    work: &Path,
    home: &Path,
    args: &[&str],
    prices: Option<&str>,
) -> (bool, String, String) {
    let mut command = junees_binary(work, home);
    command
        .arg("usage")
        .args(args)
        .env("JUNEES_HOME", home.join("junees"))
        .env("JUNEES_TIMEZONE", "UTC")
        .stdin(Stdio::null());
    if let Some(prices) = prices {
        command.env("JUNEES_PRICES", prices);
    }
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("junees usage did not exit")
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The cells of the row starting with `label`, split at runs of spaces.
fn row(text: &str, label: &str) -> Vec<String> {
    let line = text
        .lines()
        .find(|line| line.starts_with(label))
        .unwrap_or_else(|| panic!("no `{label}` row in\n{text}"));
    line[label.len()..]
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn usage_of_the_session_and_of_past_days() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let work = home.join("work");
    std::fs::create_dir(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let (url, server) = fake_api(vec![
        (
            200,
            reply(
                "First.",
                json!({"input_tokens": 1000, "cache_creation_input_tokens": 2000,
                       "cache_read_input_tokens": 30000, "output_tokens": 500,
                       "server_tool_use": {"web_search_requests": 2}}),
            ),
        ),
        (
            200,
            reply(
                "Second.",
                json!({"input_tokens": 1_000_000, "output_tokens": 0}),
            ),
        ),
    ])
    .await;
    let mut child = junees_binary(&work, home)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("JUNEES_HOME", home.join("junees"))
        .env("JUNEES_MODEL", "claude-sonnet-4-6")
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"/usage\nhello\n/usage\n/model claude-opus-4-7\nagain\n/usage\n/exit\n")
        .await
        .unwrap();
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not exit")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);

    let reports: Vec<&str> = stdout.split("Session ").skip(1).collect();
    assert_eq!(reports.len(), 3, "{stdout}");
    assert!(reports[0].contains("no tokens used yet"), "{stdout}");
    // 1000 input * $3 + 2000 cache write * $3.75 + 30000 cache read *
    // $0.30 + 500 output * $15 per million, 2 searches * $0.01; 30000 of
    // 33000 prompt tokens were cache reads.
    assert_eq!(
        row(reports[1], "model calls (claude-sonnet-4-6)"),
        ["1,000", "2,000", "30,000", "500", "2", "90.9%", "$0.0470"],
        "{stdout}"
    );
    assert!(
        reports[1].contains("https://platform.claude.com/docs/en/about-claude/pricing"),
        "{stdout}"
    );
    // After /model the next call is priced as Opus 4.7: 1M input at $5.
    assert_eq!(
        row(reports[2], "model calls (claude-opus-4-7)"),
        ["1,000,000", "0", "0", "0", "0", "0.0%", "$5.0000"],
        "{stdout}"
    );
    assert_eq!(
        row(reports[2], "total"),
        [
            "1,001,000",
            "2,000",
            "30,000",
            "500",
            "2",
            "2.9%",
            "$5.0470"
        ],
        "{stdout}"
    );

    // The journal names the model of each call.
    let sessions = home.join("junees/sessions");
    let journal = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let models: Vec<String> = std::fs::read_to_string(&journal)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["type"] == "usage")
        .map(|line| line["model"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(models, ["claude-sonnet-4-6", "claude-opus-4-7"]);

    // `junees usage` reads the same journal, with no API key.
    let (ok, text, err) = usage_command(&work, home, &["--by", "model"], None).await;
    assert!(ok, "{text}\n{err}");
    assert!(
        text.starts_with(&format!(
            "Usage in this folder ({}) the last 7 days ({} to {}, UTC), by model:\n",
            work.display(),
            utc_date(6),
            utc_date(0)
        )),
        "{text}"
    );
    assert_eq!(row(&text, "claude-opus-4-7")[6], "$5.0000", "{text}");
    assert_eq!(row(&text, "claude-sonnet-4-6")[6], "$0.0470", "{text}");
    assert_eq!(row(&text, "total")[6], "$5.0470", "{text}");

    // By day, with a price of the setting for Opus 4.7.
    let (ok, text, err) = usage_command(&work, home, &[], Some("claude-opus-4-7=1/2")).await;
    assert!(ok, "{text}\n{err}");
    assert_eq!(
        row(&text, &utc_date(0)),
        [
            "1,001,000",
            "2,000",
            "30,000",
            "500",
            "2",
            "2.9%",
            "$1.0470"
        ],
        "{text}"
    );
    assert!(text.contains("and the `prices` setting"), "{text}");

    // Another folder sees nothing, unless --all.
    let elsewhere = home.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let (ok, text, _) = usage_command(&elsewhere, home, &["--days", "1"], None).await;
    assert!(ok);
    assert!(
        text.starts_with("No usage recorded in this folder ("),
        "{text}"
    );
    assert!(text.contains("--all shows every folder."), "{text}");
    let (ok, text, _) = usage_command(
        &elsewhere,
        home,
        &["--days", "1", "--all", "--by", "folder"],
        None,
    )
    .await;
    assert!(ok);
    assert_eq!(
        row(&text, &work.display().to_string())[0],
        "1,001,000",
        "{text}"
    );

    // Help, a bad option and a bad price setting.
    let (ok, text, _) = usage_command(&work, home, &["--help"], None).await;
    assert!(ok && text.starts_with("junees usage [--days N]"), "{text}");
    let (ok, _, err) = usage_command(&work, home, &["--days", "0"], None).await;
    assert!(!ok);
    assert!(
        err.contains("--days takes a whole number from 1 to"),
        "{err}"
    );
    let (ok, _, err) = usage_command(&work, home, &[], Some("claude-x=cheap")).await;
    assert!(!ok);
    assert!(err.contains("JUNEES_PRICES (`prices`)"), "{err}");
}

#[tokio::test]
async fn usage_all_by_source_prints_sorted_rows_and_total() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let state = home.join("junees");
    let work = home.join("work");
    let dir_a = home.join("dir-a");
    let dir_b = home.join("dir-b");
    for folder in [&state, &work, &dir_a, &dir_b] {
        std::fs::create_dir(folder).unwrap();
    }

    // The writer timestamps these journals when it writes them. The default
    // seven-day window still includes them if the CLI runs just after midnight.
    // Neither journal folder is the CLI cwd, so dropping --all loses the rows.
    let model = "usage-test-model";
    for (folder, channel, [input, write, read, output, searches]) in [
        (&dir_b, None, [1000, 400, 1200, 400, 2]),
        (
            &dir_a,
            Some(("discord", "100000000000000001")),
            [500, 500, 1500, 500, 1],
        ),
        (
            &dir_b,
            Some(("cron", "test-job")),
            [3600, 600, 1800, 1000, 3],
        ),
    ] {
        let (_, mut writer) = match channel {
            Some((source, channel)) => {
                SessionWriter::create_for_channel(&state, folder, model, None, source, channel)
            }
            None => SessionWriter::create(&state, folder, model),
        }
        .unwrap();
        writer
            .record(&Entry::Message(Message::user_text("Fixture question.")))
            .unwrap();
        writer
            .record_usage(
                &Entry::Usage(Usage {
                    input_tokens: input,
                    cache_creation_input_tokens: write,
                    cache_read_input_tokens: read,
                    output_tokens: output,
                    server_tool_use: ServerToolUsage {
                        web_search_requests: searches,
                    },
                    ..Usage::default()
                }),
                model,
            )
            .unwrap();
        writer
            .record(&Entry::Message(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::text("Fixture answer.")],
            }))
            .unwrap();
    }

    let mut command = junees_binary(&work, home);
    command
        .args(["usage", "--all", "--by", "source"])
        .env_clear()
        .env("HOME", home)
        .env("JUNEES_HOME", &state)
        .env("JUNEES_TIMEZONE", "UTC")
        .env("JUNEES_PRICES", "usage-test-model=2/8/3/1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("junees usage did not exit")
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut lines = stdout.lines();
    let title = lines.next().expect("missing usage title");
    assert!(
        title.starts_with("Usage in every folder ") && title.ends_with(", by source:"),
        "{title}"
    );
    let headings: Vec<_> = lines
        .next()
        .expect("missing usage table headings")
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();
    assert_eq!(
        headings,
        [
            "input",
            "cache write",
            "cache read",
            "output",
            "web searches",
            "cache hit",
            "cost"
        ],
        "{stdout}"
    );

    // Inspect all remaining lines for rows, even after the explanatory notes,
    // so any extra total or row after total fails without pinning the notes.
    let rows: Vec<Vec<_>> = lines
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|cells| {
            cells.first() == Some(&"total")
                || cells
                    .get(1)
                    .is_some_and(|cell| cell.starts_with(|c: char| c.is_ascii_digit()))
        })
        .collect();
    // Cron has 7,000 tokens; Discord and terminal tie at 3,000 but have
    // different input counts, so the tie follows source name, not input.
    assert_eq!(
        rows,
        [
            [
                "cron", "3,600", "600", "1,800", "1,000", "3", "30.0%", "$0.0488"
            ],
            [
                "discord", "500", "500", "1,500", "500", "1", "60.0%", "$0.0180"
            ],
            [
                "terminal", "1,000", "400", "1,200", "400", "2", "46.1%", "$0.0276"
            ],
            [
                "total", "5,100", "1,500", "4,500", "1,900", "6", "40.5%", "$0.0944"
            ],
        ],
        "{stdout}"
    );
}
