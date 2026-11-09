//! A write that fails part way (a full disk, a quota) must leave the edited
//! file as it was and report an error. A low file-size limit stands in for a
//! full disk; it applies to the whole process, so this test has its own
//! binary.

use junees::tools::{EditFile, Tool};
use serde_json::json;

#[tokio::test]
async fn a_failed_write_keeps_the_original_and_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.txt");
    let original = format!("HEAD\n{}\nTAIL\n", "x".repeat(20_000));
    std::fs::write(&path, &original).unwrap();

    // SAFETY: plain libc calls on this process's own signal disposition and
    // resource limits. Ignoring SIGXFSZ turns a write past the limit into an
    // EFBIG error, as a full disk gives ENOSPC.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let mut limit = std::mem::zeroed::<libc::rlimit>();
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit), 0);
        limit.rlim_cur = 4096;
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
    }

    let output = EditFile::default()
        .run(json!({"path": path, "old_string": "HEAD", "new_string": "HEAD2"}))
        .await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output.content.starts_with("cannot write "),
        "{}",
        output.content
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    // The partial temporary file is gone too.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["big.txt"]);
}
