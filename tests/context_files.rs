//! Runs the real binary against a fake API server: the project instruction
//! files of the working folder's repository reach the system prompt, `/new`
//! reads them again, `/context` lists them, and the setting turns them off.

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

mod common;
use common::{body_of, fake_api, text_reply};

/// The binary in `cwd`, talking to the fake API, with `home` as HOME and
/// state folder, asking no questions.
fn command(home: &Path, cwd: &Path, url: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_junees"));
    command
        .args(args)
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "sk-fake")
        .env("ANTHROPIC_BASE_URL", url)
        .env("JUNEES_HOME", home.join(".junees"))
        .env("HOME", home)
        .env("JUNEES_STREAM", "0")
        .env("JUNEES_APPROVAL", "off")
        .env_remove("JUNEES_MODEL")
        .env_remove("JUNEES_CONTEXT_FILES")
        .stdin(Stdio::null());
    command
}

fn system_of(request: &str) -> String {
    body_of(request)["system"].to_string()
}

fn write_call(path: &Path, content: &str) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "tool_use", "id": "toolu_1", "name": "write_file",
                     "input": {"path": path.display().to_string(), "content": content}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
}

fn check(output: &Output) {
    assert!(
        output.status.success(),
        "junees failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A home with a repository `work/repo` holding `AGENTS.md` at its top and
/// `JUNEES.md` in `app`, and an `AGENTS.md` above the repository.
fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("work/repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(repo.join("app")).unwrap();
    std::fs::write(
        home.path().join("work/AGENTS.md"),
        "Above the repository.\n",
    )
    .unwrap();
    std::fs::write(repo.join("AGENTS.md"), "Use make check.\n").unwrap();
    std::fs::write(repo.join("app/JUNEES.md"), "App rules \u{1b}[2K.\n").unwrap();
    (home, repo)
}

#[tokio::test]
async fn the_files_reach_the_system_prompt_and_new_reads_them_again() {
    let (home, repo) = project();
    let agents = repo.join("AGENTS.md");
    let (url, server) = fake_api(vec![
        (200, write_call(&agents, "Run cargo test first.\n")),
        (200, text_reply("Changed.")),
        (200, text_reply("Hello again.")),
    ])
    .await;
    let mut child = command(home.path(), &repo.join("app"), &url, &[])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"change the rules\n/context\n/new\nhello\n/context\n/exit\n")
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("junees did not end")
        .unwrap();
    check(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake API did not get every request")
        .unwrap();
    assert_eq!(requests.len(), 3);

    let repo = std::fs::canonicalize(&repo).unwrap();
    let first = system_of(&requests[0]);
    assert!(first.contains("\\n\\n# Project instructions\\n"), "{first}");
    assert!(
        first.contains("rank below the user's own messages"),
        "{first}"
    );
    let agents_tag = format!(
        "<project-instructions path=\\\"{}\\\">",
        repo.join("AGENTS.md").display()
    );
    let junees_tag = format!(
        "<project-instructions path=\\\"{}\\\">",
        repo.join("app/JUNEES.md").display()
    );
    let (agents_at, junees_at) = (first.find(&agents_tag), first.find(&junees_tag));
    assert!(agents_at.unwrap() < junees_at.unwrap(), "{first}");
    assert!(
        first.contains("Use make check.\\n</project-instructions>"),
        "{first}"
    );
    // Hidden characters arrive as escapes.
    assert!(first.contains("App rules \\\\u{1b}[2K."), "{first}");
    assert!(!first.contains("Above the repository."), "{first}");
    // The session keeps its prompt (and the cache) after the file changed.
    assert_eq!(system_of(&requests[1]), first);
    assert_eq!(
        std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
        "Run cargo test first.\n"
    );
    // The new session read the file again.
    let after = system_of(&requests[2]);
    assert!(
        after.contains("Run cargo test first.\\n</project-instructions>"),
        "{after}"
    );
    assert!(!after.contains("Use make check."), "{after}");

    // /context lists the files with their sizes, before and after /new.
    let listed = |size: usize| format!("  {}: {size} bytes\n", repo.join("AGENTS.md").display());
    let before = stdout.find(&listed(16)).expect(&stdout);
    let later = stdout.find(&listed(22)).expect(&stdout);
    assert!(before < later, "{stdout}");
    assert!(
        stdout.contains(&format!(
            "  {}: 16 bytes\n",
            repo.join("app/JUNEES.md").display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("project instructions ("), "{stdout}");
    // Each session start names the files on stderr.
    assert_eq!(
        stderr.matches("(project instructions: ").count(),
        2,
        "{stderr}"
    );
}

#[tokio::test]
async fn the_setting_turns_the_files_off() {
    let (home, repo) = project();
    let (url, server) = fake_api(vec![(200, text_reply("Hi.")), (200, text_reply("Hi."))]).await;
    let output = command(home.path(), &repo, &url, &["-p", "hello"])
        .env("JUNEES_CONTEXT_FILES", "off")
        .output()
        .await
        .unwrap();
    check(&output);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("project instructions"));
    // Only some names.
    let output = command(home.path(), &repo.join("app"), &url, &["-p", "hello"])
        .env("JUNEES_CONTEXT_FILES", "JUNEES.md")
        .output()
        .await
        .unwrap();
    check(&output);
    let requests = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the fake API did not get every request")
        .unwrap();
    let off = system_of(&requests[0]);
    assert!(!off.contains("# Project instructions"), "{off}");
    assert!(!off.contains("Use make check."), "{off}");
    let some = system_of(&requests[1]);
    assert!(some.contains("App rules"), "{some}");
    assert!(!some.contains("Use make check."), "{some}");
}
