//! `junees update` as a program: `--help`, `--check` and `/update` in the
//! conversation against a fake release server on loopback. Nothing here
//! replaces a binary: the library tests do that on a fake one.

mod common;

use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A release server that answers `count` requests for `latest` with a
/// redirect to the tag `v<latest>`, and returns the paths asked for.
async fn release_server(
    latest: &'static str,
    count: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/releases", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut paths = Vec::new();
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = vec![0u8; 4096];
            let n = socket.read(&mut head).await.unwrap();
            let head = String::from_utf8_lossy(&head[..n]).into_owned();
            paths.push(head.split(' ').nth(1).unwrap_or("").to_string());
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: /seongj-un/junees/releases/tag/v{latest}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
        paths
    });
    (base, server)
}

/// A temporary HOME with a state folder and a working folder.
struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        Self {
            _dir: dir,
            root,
            work,
        }
    }

    async fn junees(&self, args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
        let mut command = common::junees_binary(&self.work, &self.root);
        command
            .args(args)
            .env("JUNEES_HOME", self.root.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.as_bytes()).await.unwrap();
        drop(input);
        tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
            .await
            .expect("junees did not end")
            .unwrap()
    }
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn update_check_says_a_newer_release_is_out() {
    let home = Home::new();
    let (base, server) = release_server("999.0.0", 1).await;
    let output = home
        .junees(
            &["update", "--check"],
            &[("JUNEES_DOWNLOAD_BASE", &base)],
            "",
        )
        .await;
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        stdout,
        format!(
            "junees 999.0.0 is out (this is {}): run `junees update`\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    let paths = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake release server got no request")
        .unwrap();
    assert_eq!(paths, ["/releases/latest"]);
}

#[tokio::test]
async fn update_help_and_bad_arguments() {
    let home = Home::new();
    let output = home.junees(&["update", "--help"], &[], "").await;
    let (stdout, _) = text(&output);
    assert!(output.status.success());
    assert!(
        stdout.starts_with("junees update [--check] [--version <vX.Y.Z>]\n"),
        "{stdout}"
    );
    let output = home.junees(&["update", "--fast"], &[], "").await;
    let (_, stderr) = text(&output);
    assert!(!output.status.success());
    assert!(stderr.contains("unknown argument `--fast`"), "{stderr}");
    // Only a server on this computer stands in for GitHub.
    let output = home
        .junees(
            &["update", "--check"],
            &[("JUNEES_DOWNLOAD_BASE", "https://example.com/releases")],
            "",
        )
        .await;
    let (_, stderr) = text(&output);
    assert!(!output.status.success());
    assert!(stderr.contains("only for tests"), "{stderr}");
}

#[tokio::test]
async fn the_conversation_checks_with_update() {
    let home = Home::new();
    let (base, server) = release_server(env!("CARGO_PKG_VERSION"), 1).await;
    let output = home
        .junees(
            &["--no-session"],
            &[
                ("JUNEES_DOWNLOAD_BASE", &base),
                ("ANTHROPIC_API_KEY", "sk-test-not-used"),
                ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9"),
            ],
            "/update\n/update now\n/exit\n",
        )
        .await;
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!(
            "junees {} is the latest release\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{stdout}"
    );
    assert!(stderr.contains("/update takes no argument"), "{stderr}");
    let paths = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake release server got no request")
        .unwrap();
    assert_eq!(paths, ["/releases/latest"]);
}
