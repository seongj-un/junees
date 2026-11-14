use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::tools::file_access::Scope;
use crate::tools::{Tool, ToolOutput};

/// A folder `work` to search, inside a temp dir (its parent is elsewhere).
struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let work = root.join("work");
        fs::create_dir(&work).unwrap();
        Self {
            _dir: dir,
            root,
            work,
        }
    }

    fn file(&self, path: &str, text: &str) -> PathBuf {
        let path = self.work.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn at(&self) -> String {
        self.work.display().to_string()
    }
}

fn age(path: &Path, secs: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(secs))
        .unwrap();
}

fn open() -> Search {
    Search::with_access(FileAccess::default(), TIME_LIMIT)
}

/// The rules of an unattended run working in `work`, with `home` for its
/// credential folders.
fn unattended(work: &Path, home: &Path) -> Search {
    let scope = Scope::new(Some(work.to_path_buf()), vec![], vec![], Some(home), None);
    Search::with_access(FileAccess::default().with_scope(scope), TIME_LIMIT)
}

async fn grep(search: Search, input: Value) -> ToolOutput {
    Grep::new(search).run(input).await
}

fn ok(output: ToolOutput) -> String {
    assert!(!output.is_error, "{}", output.content);
    output.content
}

#[tokio::test]
async fn grep_lists_files_shows_lines_and_counts() {
    let tree = Tree::new();
    let a = tree.file("src/a.rs", "fn main() {}\nlet Needle = 1;\nneedle again\n");
    let b = tree.file("b.txt", "one\ntwo\nneedle\nthree\nfour\nfive\nneedle\n");
    tree.file("c.txt", "nothing here\n");
    age(&b, 60);

    let files = ok(grep(open(), json!({"pattern": "needle", "path": tree.at()})).await);
    assert_eq!(files, format!("{}\n{}\n", a.display(), b.display()));

    let counted = ok(grep(
        open(),
        json!({"pattern": "needle", "path": tree.at(), "output": "count", "case_insensitive": true}),
    )
    .await);
    assert_eq!(counted, format!("{}:2\n{}:2\n", a.display(), b.display()));

    let content = ok(grep(
        open(),
        json!({"pattern": "needle", "path": b.display().to_string(), "output": "content", "context": 1}),
    )
    .await);
    let b = b.display();
    assert_eq!(
        content,
        format!("{b}-2-two\n{b}:3:needle\n{b}-4-three\n--\n{b}-6-five\n{b}:7:needle\n")
    );

    let none = ok(grep(open(), json!({"pattern": "absent", "path": tree.at()})).await);
    assert!(none.starts_with("No matches for `absent`"), "{none}");
    let bad = grep(open(), json!({"pattern": "(", "path": tree.at()})).await;
    assert!(bad.is_error && bad.content.starts_with("invalid regular expression"));
    let bad = grep(open(), json!({"pattern": "x", "output": "lines"})).await;
    assert!(bad.is_error, "{}", bad.content);
    let bad = grep(open(), json!({"pattern": "x", "context": 11})).await;
    assert!(bad.content.contains("from 0 to 10"), "{}", bad.content);
}

#[tokio::test]
async fn grep_filters_by_glob_type_and_spans_lines() {
    let tree = Tree::new();
    let rs = tree.file("src/deep/x.rs", "struct Found {\n    a: u8,\n}\n");
    tree.file("src/x.py", "Found = 1\n");
    let by_glob = ok(grep(
        open(),
        json!({"pattern": "Found", "path": tree.at(), "glob": "*.rs"}),
    )
    .await);
    assert_eq!(by_glob, format!("{}\n", rs.display()));
    let by_path = ok(grep(
        open(),
        json!({"pattern": "Found", "path": tree.at(), "glob": "src/*.py"}),
    )
    .await);
    assert!(
        by_path.ends_with("x.py\n") && by_path.lines().count() == 1,
        "{by_path}"
    );
    let by_type = ok(grep(
        open(),
        json!({"pattern": "Found", "path": tree.at(), "type": "rust"}),
    )
    .await);
    assert_eq!(by_type, format!("{}\n", rs.display()));
    let bad = grep(open(), json!({"pattern": "x", "type": "nope"})).await;
    assert!(bad.is_error && bad.content.contains("unknown file type"));

    let spans = ok(grep(
        open(),
        json!({"pattern": "Found \\{.*?\\}", "path": tree.at(), "output": "content", "multiline": true}),
    )
    .await);
    let rs = rs.display();
    assert_eq!(
        spans,
        format!("{rs}:1:struct Found {{\n{rs}:2:    a: u8,\n{rs}:3:}}\n")
    );
}

#[tokio::test]
async fn grep_caps_its_output_and_says_how_much_more() {
    let tree = Tree::new();
    for n in 0..5 {
        tree.file(&format!("f{n}.txt"), "hit\nhit\n");
    }
    let long = tree.file("long.txt", &format!("hit {}\n", "가".repeat(600)));
    let files = ok(grep(
        open(),
        json!({"pattern": "hit", "path": tree.at(), "max_results": 2}),
    )
    .await);
    assert_eq!(files.lines().count(), 3, "{files}");
    assert!(
        files.ends_with("[4 more file(s) not shown; narrow the pattern, path, glob or type]\n"),
        "{files}"
    );
    let lines = ok(grep(
        open(),
        json!({"pattern": "hit", "path": tree.at(), "output": "content", "max_results": 3}),
    )
    .await);
    assert_eq!(lines.lines().count(), 4, "{lines}");
    assert!(
        lines.contains("[8 more matching line(s) not shown"),
        "{lines}"
    );
    let cut = ok(grep(
        open(),
        json!({"pattern": "hit", "path": long.display().to_string(), "output": "content"}),
    )
    .await);
    assert!(cut.ends_with(" [... 104 more characters]\n"), "{cut}");
}

#[tokio::test]
async fn ignore_files_hidden_files_and_build_folders_are_left_out() {
    let tree = Tree::new();
    tree.file(".gitignore", "ignored.txt\n");
    tree.file(".ignore", "also/\n");
    tree.file("ignored.txt", "hit\n");
    tree.file("also/x.txt", "hit\n");
    tree.file(".env", "hit\n");
    tree.file(".hidden/x.txt", "hit\n");
    tree.file("target/debug/x.txt", "hit\n");
    tree.file("node_modules/m/x.txt", "hit\n");
    tree.file(".git/HEAD", "hit\n");
    let kept = tree.file("kept.txt", "hit\n");
    let found = ok(grep(open(), json!({"pattern": "hit", "path": tree.at()})).await);
    assert_eq!(found, format!("{}\n", kept.display()));
    // With `hidden`, dot files too, but never .git or build folders.
    let found = ok(grep(
        open(),
        json!({"pattern": "hit", "path": tree.at(), "hidden": true}),
    )
    .await);
    let mut names: Vec<&str> = found
        .lines()
        .map(|line| line.rsplit_once("/work/").unwrap().1)
        .collect();
    names.sort();
    assert_eq!(names, [".env", ".hidden/x.txt", "kept.txt"]);
}

#[tokio::test]
async fn binary_and_large_files_are_skipped_and_counted() {
    let tree = Tree::new();
    tree.file("bin.dat", "hit\0hit\n");
    tree.file("big.txt", &format!("hit\n{}", "x".repeat(1024 * 1024)));
    let small = tree.file("small.txt", "hit\n");
    let found = ok(grep(open(), json!({"pattern": "hit", "path": tree.at()})).await);
    assert_eq!(
        found,
        format!(
            "{}\n[skipped 1 file(s) over 1 MB and 1 binary file(s)]\n",
            small.display()
        )
    );
}

#[tokio::test]
async fn links_are_not_followed() {
    let tree = Tree::new();
    fs::create_dir(tree.root.join("outside")).unwrap();
    fs::write(tree.root.join("outside/secret.txt"), "hit\n").unwrap();
    std::os::unix::fs::symlink(tree.root.join("outside"), tree.work.join("out")).unwrap();
    std::os::unix::fs::symlink(
        tree.root.join("outside/secret.txt"),
        tree.work.join("link.txt"),
    )
    .unwrap();
    let found = ok(grep(open(), json!({"pattern": "hit", "path": tree.at()})).await);
    assert!(found.starts_with("No matches"), "{found}");
    let globbed = ok(Glob::new(open())
        .run(json!({"pattern": "**/*.txt", "path": tree.at()}))
        .await);
    assert!(globbed.starts_with("No files match"), "{globbed}");
    let listed = ok(ListDir::new(open())
        .run(json!({"path": tree.at(), "depth": 3}))
        .await);
    assert!(
        listed.contains("  link.txt@ (link)\n  out@ (link)\n"),
        "{listed}"
    );
    assert!(!listed.contains("secret"), "{listed}");
}

#[tokio::test]
async fn unattended_runs_neither_leave_their_folder_nor_see_credentials() {
    let tree = Tree::new();
    // The home folder is the working folder here: its `.ssh` is a
    // credential folder the run must not see, nor go into.
    tree.file(".ssh/id_ed25519", "hit\n");
    tree.file(".aws/credentials", "hit\n");
    let kept = tree.file("src/a.txt", "hit\n");
    fs::write(tree.root.join("elsewhere.txt"), "hit\n").unwrap();
    let rules = unattended(&tree.work, &tree.work);
    let found = ok(grep(
        rules.clone(),
        json!({"pattern": "hit", "path": tree.at(), "hidden": true}),
    )
    .await);
    assert_eq!(found, format!("{}\n", kept.display()));
    let listed = ok(ListDir::new(rules.clone())
        .run(json!({"path": tree.at(), "depth": 3, "hidden": true}))
        .await);
    assert!(
        !listed.contains(".ssh") && !listed.contains(".aws"),
        "{listed}"
    );
    let globbed = ok(Glob::new(rules.clone())
        .run(json!({"pattern": "**/*", "path": tree.at(), "hidden": true}))
        .await);
    assert_eq!(globbed, format!("{}\n", kept.display()));
    // Starting in one is refused outright, as is leaving the folder.
    let inside = grep(
        rules.clone(),
        json!({"pattern": "hit", "path": tree.work.join(".ssh").display().to_string()}),
    )
    .await;
    assert!(
        inside.is_error && inside.content.contains("credential"),
        "{}",
        inside.content
    );
    let outside = grep(
        rules,
        json!({"pattern": "hit", "path": tree.root.display().to_string()}),
    )
    .await;
    assert!(outside.is_error, "{}", outside.content);
}

#[tokio::test]
async fn credentials_in_matching_lines_are_hidden() {
    let tree = Tree::new();
    let key = format!("sk-ant-api03-{}", "Ab1".repeat(20));
    tree.file(
        "env.sh",
        &format!("export ANTHROPIC_API_KEY={key}\nexport KEY_NAME=x\n"),
    );
    let content = ok(grep(
        open(),
        json!({"pattern": "KEY", "path": tree.at(), "output": "content"}),
    )
    .await);
    assert!(!content.contains(&key[..20]), "{content}");
    assert!(content.contains("[hidden"), "{content}");
    assert!(content.contains("export KEY_NAME=x"), "{content}");
}

#[tokio::test]
async fn a_search_stops_at_its_time_limit() {
    let tree = Tree::new();
    tree.file("a.txt", "hit\n");
    let slow = Search::with_access(FileAccess::default(), Duration::ZERO);
    let found = ok(grep(slow.clone(), json!({"pattern": "hit", "path": tree.at()})).await);
    assert!(found.contains("[stopped at the 0 s time limit"), "{found}");
    let listed = ok(ListDir::new(slow).run(json!({"path": tree.at()})).await);
    assert!(
        listed.contains("[stopped at the 0 s time limit"),
        "{listed}"
    );
}

#[test]
fn a_dropped_call_stops_the_walk() {
    let stop = Stop::new(TIME_LIMIT);
    let guard = stop.guard();
    assert_eq!(stop.ended(), None);
    drop(guard);
    assert_eq!(stop.ended(), Some(Ended::Cancelled));
    let stop = Stop::new(TIME_LIMIT);
    stop.guard().disarm();
    assert_eq!(stop.ended(), None);
}

#[tokio::test]
async fn glob_matches_paths_newest_first_and_stays_inside() {
    let tree = Tree::new();
    let old = tree.file("src/old.rs", "");
    let new = tree.file("src/deep/new.rs", "");
    tree.file("top.rs", "");
    tree.file("src/note.md", "");
    age(&old, 60);
    let glob = Glob::new(open());
    let found = ok(glob
        .run(json!({"pattern": "src/**/*.rs", "path": tree.at()}))
        .await);
    assert_eq!(found, format!("{}\n{}\n", new.display(), old.display()));
    // `*` stays within one folder.
    let top = ok(glob
        .run(json!({"pattern": "*.rs", "path": tree.at()}))
        .await);
    assert!(
        top.ends_with("/work/top.rs\n") && top.lines().count() == 1,
        "{top}"
    );
    for pattern in ["../*", "/etc/*", "a/../../b"] {
        let out = glob
            .run(json!({"pattern": pattern, "path": tree.at()}))
            .await;
        assert!(
            out.is_error && out.content.contains("leaves the folder"),
            "{}",
            out.content
        );
    }
    let file = glob
        .run(json!({"pattern": "*", "path": old.display().to_string()}))
        .await;
    assert!(file.is_error && file.content.contains("is not a folder"));
}

#[tokio::test]
async fn list_dir_draws_a_tree_with_sizes() {
    let tree = Tree::new();
    tree.file("README.md", "hello\n");
    tree.file("src/main.rs", &"x".repeat(2048));
    tree.file("src/tools/mod.rs", "");
    let list = ListDir::new(open());
    let one = ok(list.run(json!({"path": tree.at()})).await);
    assert_eq!(one, format!("{}/\n  README.md (6 B)\n  src/\n", tree.at()));
    let three = ok(list.run(json!({"path": tree.at(), "depth": 3})).await);
    assert_eq!(
        three,
        format!(
            "{}/\n  README.md (6 B)\n  src/\n    main.rs (2.0 KB)\n    tools/\n      mod.rs (0 B)\n",
            tree.at()
        )
    );
    let bad = list.run(json!({"path": tree.at(), "depth": 4})).await;
    assert!(bad.is_error && bad.content.contains("from 1 to 3"));
    for n in 0..405 {
        tree.file(&format!("many/f{n:03}"), "");
    }
    let many = ok(list
        .run(json!({"path": tree.work.join("many").display().to_string()}))
        .await);
    assert!(
        many.ends_with("[5 more entries not shown; list a subfolder]\n"),
        "{many}"
    );
}
