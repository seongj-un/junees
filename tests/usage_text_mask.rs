//! The text table of `junees usage` through the real binary: credential
//! patterns in folder and model names are masked as in `--json`. Only
//! local synthetic journals.

use std::fs;
use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use junees::message::Usage;
use junees::session::{Entry, SessionWriter};

mod common;
use common::junees_binary;

const FAKE_KEY_A: &str = concat!("sk-", "ant-fixtureNotARealAPIKey");
const FAKE_KEY_B: &str = concat!("sk-", "ant-otherFixtureNotARealKey");

async fn usage(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let mut command = junees_binary(cwd, home);
    command
        .env_clear()
        .env("HOME", home)
        .env("JUNEES_HOME", home.join("state"))
        .env("JUNEES_TIMEZONE", "UTC")
        .args(["usage"])
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out: Output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("junees usage did not exit")
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr was not empty");
    String::from_utf8(out.stdout).unwrap()
}

/// No piece of either fake key is left in `text`. The text is not printed:
/// a failure must not put the key in the test log.
fn assert_no_key(text: &str) {
    for piece in [
        FAKE_KEY_A,
        FAKE_KEY_B,
        concat!("sk-", "ant-"),
        "fixtureNotAReal",
        "FixtureNotAReal",
    ] {
        assert!(!text.contains(piece), "the output held a key piece");
    }
}

fn record(state: &Path, cwd: &Path, model: &str, input: u64) {
    let (_, mut journal) = SessionWriter::create(state, cwd, model).unwrap();
    journal
        .record_usage(
            &Entry::Usage(Usage {
                input_tokens: input,
                ..Usage::default()
            }),
            model,
        )
        .unwrap();
}

#[tokio::test]
async fn text_rows_notes_and_folder_line_mask_credential_labels() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let state = home.join("state");
    let plain = home.join("work");
    let keyed_a = home.join(FAKE_KEY_A);
    let keyed_b = home.join(FAKE_KEY_B);
    for folder in [&plain, &keyed_a, &keyed_b] {
        fs::create_dir(folder).unwrap();
    }
    record(&state, &keyed_a, FAKE_KEY_A, 10);
    record(&state, &keyed_b, FAKE_KEY_B, 2000);
    record(&state, &plain, "fixture-unknown", 300);
    let journals: Vec<_> = fs::read_dir(state.join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(journals.len(), 3);
    let before: Vec<_> = journals
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();

    // Rows by folder and by model: the two masked names stay two rows, in
    // the order of their tokens, and the plain name is as it was.
    for (by, plain_label) in [
        ("--by=folder", plain.to_str().unwrap()),
        ("--by=model", "fixture-unknown"),
    ] {
        let text = usage(&home, &plain, &["--all", "--days=1", by]).await;
        assert_no_key(&text);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Usage in every folder today ("));
        let rows: Vec<(&str, &str)> = lines[2..6]
            .iter()
            .map(|line| {
                // The label may hold single spaces; the columns are two
                // or more apart.
                let mut cells = line.split("  ").filter(|cell| !cell.is_empty());
                (cells.next().unwrap(), cells.next().unwrap().trim())
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("[redacted]", "2,000"),
                (plain_label, "300"),
                ("[redacted]", "10"),
                ("total", "2,310"),
            ],
            "{by}"
        );
        assert!(
            text.contains("No price is known for fixture-unknown, [redacted], [redacted]: "),
            "{by}: the note did not list the masked models"
        );
    }

    // The folder of the opening line, from a folder whose name holds a key.
    let text = usage(&home, &keyed_a, &["--days=1", "--by=source"]).await;
    assert_no_key(&text);
    assert!(text.starts_with("Usage in this folder ([redacted]) today ("));
    assert!(text.lines().any(|line| line.starts_with("terminal  ")));
    assert!(text.contains("No price is known for [redacted]: "));

    // A plain folder is named in full.
    let text = usage(&home, &plain, &["--days=1", "--by=model"]).await;
    assert_no_key(&text);
    assert!(text.starts_with(&format!(
        "Usage in this folder ({}) today (",
        plain.to_str().unwrap()
    )));
    assert!(!text.contains("[redacted]"));
    assert!(text.contains("No price is known for fixture-unknown: "));

    let after: Vec<_> = journals
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    assert_eq!(after, before);
}
