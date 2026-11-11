//! Images through the real binary against a fake API: `--image`, the
//! `/image` command, `read_file` of a picture, a resumed session that sends
//! its images again, and `junees usage` over such a session.

use std::path::Path;
use std::process::{Output, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{body_of, fake_api, text_reply};

/// A PNG header of `width` x `height`: enough for Junees, which never
/// decodes the pixels.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xaeB`\x82");
    bytes
}

fn image_block(bytes: &[u8]) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                       "data": STANDARD.encode(bytes)}})
}

/// A home with the working folder inside it (`<home>/work`).
fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir(&work).unwrap();
    (home, work)
}

fn command(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = common::junees_binary(cwd, home);
    command
        .args(args)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home.join(".junees"))
        .env("JUNEES_STREAM", "0")
        .stdin(Stdio::null());
    command
}

async fn junees(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> Output {
    command(home, cwd, url, args).output().await.unwrap()
}

/// A request's content blocks without their cache marks.
fn unmarked(content: &Value) -> Value {
    let mut content = content.clone();
    for block in content.as_array_mut().unwrap() {
        block.as_object_mut().unwrap().remove("cache_control");
    }
    content
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn read_call(path: &str) -> Value {
    json!({
        "id": "msg_2", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "read_file",
                     "input": {"path": path}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 20, "output_tokens": 5}
    })
}

#[tokio::test]
async fn image_option_sends_the_picture_and_a_resumed_session_sends_it_again() {
    let (home, work) = home();
    let picture = png(40, 30);
    std::fs::write(work.join("shot.png"), &picture).unwrap();
    let (url, server) = fake_api(vec![
        (200, text_reply("A chart.")),
        (200, read_call("shot.png")),
        (200, text_reply("Same picture.")),
    ])
    .await;

    let first = junees(
        home.path(),
        &work,
        &url,
        &["--image", "shot.png", "-p", "what is this?"],
    )
    .await;
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert_eq!(text(&first.stdout), "A chart.\n");
    assert!(
        text(&first.stderr).contains("/work/shot.png (image/png, 40x30 px, 45 B))"),
        "{}",
        text(&first.stderr)
    );
    let second = junees(home.path(), &work, &url, &["-c", "-p", "read it again"]).await;
    assert!(second.status.success(), "{}", text(&second.stderr));
    assert_eq!(text(&second.stdout), "Same picture.\n");

    let requests = server.await.unwrap();
    let first = body_of(&requests[0]);
    assert_eq!(
        unmarked(&first["messages"][0]["content"]),
        json!([image_block(&picture), {"type": "text", "text": "what is this?"}])
    );
    // Continued: the image goes again, read back from the session folder.
    let resumed = body_of(&requests[1]);
    assert_eq!(
        unmarked(&resumed["messages"][0]["content"]),
        unmarked(&first["messages"][0]["content"])
    );
    assert_eq!(
        resumed["messages"][2]["content"][0]["text"],
        "read it again"
    );
    // read_file gave the picture back inside its result.
    let after_read = body_of(&requests[2]);
    let result = &after_read["messages"][4]["content"][0];
    assert_eq!(result["type"], "tool_result");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "{result}");
    assert!(
        content[0]["text"]
            .as_str()
            .unwrap()
            .ends_with("shot.png (image/png, 40x30 px, 45 B), attached below."),
        "{result}"
    );
    assert_eq!(
        unmarked(&json!([content[1]])),
        json!([image_block(&picture)])
    );

    // One file for the picture sent twice; no base64 in the journal.
    let sessions = home.path().join(".junees/sessions");
    let journals: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(journals.len(), 1);
    let journal = std::fs::read_to_string(&journals[0]).unwrap();
    assert!(!journal.contains("\"base64\""));
    let images: Vec<_> = std::fs::read_dir(journals[0].with_extension("").join("images"))
        .unwrap()
        .collect();
    assert_eq!(images.len(), 1);

    // `junees usage` reads such a session like any other.
    let usage = command(home.path(), &work, &url, &["usage"])
        .output()
        .await
        .unwrap();
    assert!(usage.status.success(), "{}", text(&usage.stderr));
    let report = text(&usage.stdout);
    let total: Vec<&str> = report
        .lines()
        .find(|line| line.starts_with("total"))
        .unwrap_or_else(|| panic!("no total row in\n{report}"))["total".len()..]
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();
    // 3 + 20 + 3 input tokens, 1 + 5 + 1 output tokens.
    assert_eq!((total[0], total[3]), ("26", "7"), "{report}");
    // So does session search.
    let found = command(
        home.path(),
        &work,
        &url,
        &["sessions", "search", "what is this"],
    )
    .output()
    .await
    .unwrap();
    assert!(found.status.success(), "{}", text(&found.stderr));
    assert!(
        text(&found.stdout).contains("what is this?"),
        "{}",
        text(&found.stdout)
    );
}

#[tokio::test]
async fn the_image_command_sends_a_picture_in_the_conversation() {
    let (home, work) = home();
    let first = png(8, 8);
    let second = png(16, 4);
    std::fs::write(work.join("a.png"), &first).unwrap();
    std::fs::write(work.join("two words.png"), &second).unwrap();
    std::fs::write(work.join("notes.txt"), "just text").unwrap();
    let (url, server) = fake_api(vec![
        (200, text_reply("A square.")),
        (200, text_reply("A strip.")),
    ])
    .await;
    let mut child = command(home.path(), &work, &url, &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            "/image a.png\n/image \"two words.png\" 이건 뭐야?\n/image missing.png hi\n\
             /image notes.txt\n/image\n/exit\n"
                .as_bytes(),
        )
        .await
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    assert!(
        stdout.contains("A square.") && stdout.contains("A strip."),
        "{stdout}"
    );
    assert!(stderr.contains("(attached "), "{stderr}");
    assert!(stderr.contains("missing.png"), "{stderr}");
    assert!(stderr.contains("notes.txt: not a PNG"), "{stderr}");
    assert!(stderr.contains("/image takes an image file"), "{stderr}");

    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        unmarked(&body_of(&requests[0])["messages"][0]["content"]),
        json!([image_block(&first), {"type": "text", "text": "(sent with no text)"}])
    );
    let body = body_of(&requests[1]);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        unmarked(&messages.last().unwrap()["content"]),
        json!([image_block(&second), {"type": "text", "text": "이건 뭐야?"}])
    );
}

#[tokio::test]
async fn images_the_api_would_refuse_stop_before_any_call() {
    let (home, work) = home();
    std::fs::write(work.join("notes.png"), "not a picture").unwrap();
    let big = std::fs::File::create(work.join("big.png")).unwrap();
    big.set_len(6 * 1024 * 1024).unwrap();
    std::fs::write(work.join("wide.png"), png(9000, 10)).unwrap();
    std::fs::write(work.join("ok.png"), png(1, 1)).unwrap();
    // Nothing listens here: a call would fail differently.
    let url = "http://127.0.0.1:9";
    for (file, said) in [
        ("notes.png", "not a PNG, JPEG, GIF or WebP image"),
        ("big.png", "images are sent up to 5 MB"),
        ("wide.png", "9000x10 px"),
        ("gone.png", "cannot read"),
    ] {
        let output = junees(home.path(), &work, url, &["--image", file, "-p", "hi"]).await;
        assert!(!output.status.success());
        let stderr = text(&output.stderr);
        assert!(
            stderr.contains(said) && stderr.contains(file),
            "{file}: {stderr}"
        );
    }
    let output = junees(home.path(), &work, url, &["--image", "ok.png"]).await;
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("--image goes with a prompt"));
    // The claude-code backend is refused before anything starts.
    let output = command(home.path(), &work, url, &["--image", "ok.png", "-p", "hi"])
        .env("JUNEES_BACKEND", "claude-code")
        .env("JUNEES_CLAUDE_BIN", "/nonexistent/claude")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("images are sent on the api backend only"),
        "{}",
        text(&output.stderr)
    );
    // No session was started for any of them.
    assert!(!home.path().join(".junees/sessions").exists());
}

#[tokio::test]
async fn an_image_the_api_rejects_is_left_out_of_the_resumed_session() {
    let (home, work) = home();
    let picture = png(40, 30);
    std::fs::write(work.join("broken.png"), &picture).unwrap();
    let rejected = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "messages.0.content.0.image.source.base64: The image was specified using \
                    the image/png media type, but does not appear to be a valid png image"}});
    let (url, server) = fake_api(vec![(400, rejected), (200, text_reply("Only text now."))]).await;

    let first = junees(
        home.path(),
        &work,
        &url,
        &["--image", "broken.png", "-p", "what is this?"],
    )
    .await;
    assert!(!first.status.success());
    let stderr = text(&first.stderr);
    assert!(stderr.contains("valid png image"), "{stderr}");
    assert!(
        stderr.contains("the API rejected an image in this message"),
        "{stderr}"
    );
    let second = junees(home.path(), &work, &url, &["-c", "-p", "try again"]).await;
    assert!(second.status.success(), "{}", text(&second.stderr));
    assert_eq!(text(&second.stdout), "Only text now.\n");

    let requests = server.await.unwrap();
    let first = body_of(&requests[0]);
    assert_eq!(first["messages"][0]["content"][0], image_block(&picture));
    // Continued: the input stays, the picture does not.
    let resumed = body_of(&requests[1]);
    assert_eq!(
        resumed["messages"].as_array().unwrap().len(),
        1,
        "{resumed}"
    );
    let content = unmarked(&resumed["messages"][0]["content"]);
    assert_eq!(
        content[0]["text"].as_str().unwrap(),
        "[image removed: the API rejected the request that carried it, so it is not sent again]"
    );
    assert_eq!(
        content,
        json!([content[0], {"type": "text", "text": "what is this?"},
               {"type": "text", "text": "try again"}])
    );
    assert!(!requests[1].contains("\"image\""), "{resumed}");
}
