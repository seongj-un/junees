//! `junees doctor` as a program: its text and JSON output, its exit code,
//! a broken settings file, and `/doctor` in the conversation.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

const SECRET: &str = "sk-binary-doctor-secret";

/// A temporary HOME holding a state folder (0700) and a working folder.
struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    work: PathBuf,
}

impl Home {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, work) = (root.join("state"), root.join("work"));
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&work).unwrap();
        mode(&state, 0o700);
        let home = Self {
            _dir: dir,
            root,
            state,
            work,
        };
        home.config(config);
        home
    }

    fn config(&self, text: &str) {
        let path = self.state.join("config.toml");
        std::fs::write(&path, text).unwrap();
        mode(&path, 0o600);
    }

    async fn junees(&self, args: &[&str], stdin: &str) -> Output {
        let mut command = common::junees_binary(&self.work, &self.root);
        command
            .args(args)
            .env("JUNEES_HOME", &self.state)
            // No claude, no bwrap: the claude-code part is skipped.
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.as_bytes()).await.unwrap();
        drop(input);
        tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
            .await
            .expect("junees did not exit")
            .unwrap()
    }
}

fn mode(path: &Path, bits: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn doctor_lists_the_checks_and_exits_1_on_a_failure() {
    let home = Home::new(&format!("api_key = \"{SECRET}\"\n"));
    let output = home.junees(&["doctor"], "").await;
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.starts_with(&format!("junees doctor: {} (offline", home.state.display())),
        "{stdout}"
    );
    for line in [
        "\nstate folder\n  ok    folder: ",
        "  ok    config.toml: private (mode 600)\n",
        "  ok    Anthropic API key: set (config.toml)\n",
        "  --    claude: not used (the backend is api) and `claude` is not on PATH\n",
        "\n0 failed, ",
    ] {
        assert!(stdout.contains(line), "missing {line:?} in\n{stdout}");
    }
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));

    mode(&home.state.join("config.toml"), 0o644);
    let output = home.junees(&["doctor"], "").await;
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("  FAIL  config.toml: other users can read it (mode 644)"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "        fix: chmod 600 {}",
            home.state.join("config.toml").display()
        )),
        "{stdout}"
    );

    let output = home.junees(&["doctor", "--json"], "").await;
    assert_eq!(output.status.code(), Some(1));
    let (stdout, _) = text(&output);
    assert!(!stdout.contains(SECRET));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["online"], false);
    assert_eq!(report["counts"]["fail"], 1);
    let failed: Vec<&Value> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["status"] == "fail")
        .collect();
    assert_eq!(failed.len(), 1, "{stdout}");
    assert_eq!(failed[0]["area"], "state folder");
    assert_eq!(failed[0]["name"], "config.toml");
    assert!(failed[0]["fix"].as_str().unwrap().starts_with("chmod 600 "));
}

#[tokio::test]
async fn a_broken_settings_file_is_a_finding_not_an_error() {
    let home = Home::new("api_key = \"sk-x\"\nwat = 1\n");
    let output = home.junees(&["doctor"], "").await;
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("  FAIL  config.toml: ")
            && stdout.contains("line 2: unknown setting `wat`"),
        "{stdout}"
    );
    // The checks went on without the file.
    assert!(stdout.contains("\nsessions\n"), "{stdout}");
}

#[tokio::test]
async fn doctor_help_and_bad_arguments() {
    let home = Home::new("");
    let output = home.junees(&["doctor", "--help"], "").await;
    let (stdout, _) = text(&output);
    assert!(output.status.success());
    assert!(
        stdout.starts_with("junees doctor [--json] [--online]\n"),
        "{stdout}"
    );
    let output = home.junees(&["doctor", "--fix"], "").await;
    let (_, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr.contains("unknown argument `--fix` for doctor"),
        "{stderr}"
    );
}

#[tokio::test]
async fn the_conversation_runs_doctor() {
    let home = Home::new(&format!(
        "api_key = \"{SECRET}\"\nbase_url = \"http://127.0.0.1:9\"\n"
    ));
    let output = home
        .junees(&["--no-session"], "/doctor\n/doctor now\n/exit\n")
        .await;
    let (stdout, stderr) = text(&output);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!("junees doctor: {} (offline", home.state.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains("  ok    Anthropic API key: set (config.toml)\n"),
        "{stdout}"
    );
    assert!(stderr.contains("/doctor takes no argument"), "{stderr}");
    assert!(!stdout.contains(SECRET) && !stderr.contains(SECRET));
}
