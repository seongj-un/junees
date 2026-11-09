use std::os::unix::fs::PermissionsExt;

use serde_json::json;

use super::*;
use crate::tools::{Tool, ToolOutput};

fn memory(home: &Path) -> Memory {
    Memory::new(home, Path::new("/work/project"), Limits::default())
}

fn small(home: &Path, limit: usize) -> Memory {
    Memory::new(
        home,
        Path::new("/work/project"),
        Limits {
            user: limit,
            notes: limit,
            project: limit,
        },
    )
}

fn add(memory: &Memory, target: Target, text: &str) -> Result<Changed, String> {
    memory.change(target, Change::Add(text.into()))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn adds_replaces_and_removes_entries() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    let changed = add(&memory, Target::User, "Name: 성준").unwrap();
    assert_eq!(
        changed,
        Changed {
            what: "added",
            entries: 1,
            chars: 8,
            limit: DEFAULT_USER_CHARS
        }
    );
    add(
        &memory,
        Target::User,
        "  Prefers short answers in Korean.\n",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(memory.path(Target::User)).unwrap(),
        "Name: 성준\n§\nPrefers short answers in Korean.\n"
    );
    // The same entry again changes nothing.
    assert_eq!(
        add(&memory, Target::User, "Name: 성준").unwrap().what,
        "already saved"
    );

    let changed = memory
        .change(
            Target::User,
            Change::Replace {
                old: "short answers".into(),
                new: "Prefers detailed answers in Korean.".into(),
            },
        )
        .unwrap();
    assert_eq!((changed.what, changed.entries), ("replaced", 2));
    assert_eq!(
        memory.entries(Target::User).unwrap(),
        ["Name: 성준", "Prefers detailed answers in Korean."]
    );

    let changed = memory
        .change(Target::User, Change::Remove("성준".into()))
        .unwrap();
    assert_eq!((changed.what, changed.entries), ("removed", 1));
    assert_eq!(
        memory.entries(Target::User).unwrap(),
        ["Prefers detailed answers in Korean."]
    );
    // The other stores are separate files.
    assert!(memory.entries(Target::Notes).unwrap().is_empty());
    assert!(memory.entries(Target::Project).unwrap().is_empty());
}

#[test]
fn entries_span_lines_and_survive_a_reread() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    add(&memory, Target::Notes, "Build:\n  cargo build -j 1").unwrap();
    add(&memory, Target::Notes, "Docs are in Korean.").unwrap();
    assert_eq!(
        memory.entries(Target::Notes).unwrap(),
        ["Build:\n  cargo build -j 1", "Docs are in Korean."]
    );
    // A separator line inside an entry would split it.
    let err = add(&memory, Target::Notes, "a\n §\nb").unwrap_err();
    assert!(err.contains("separates entries"), "{err}");
    assert_eq!(
        add(&memory, Target::Notes, "  \n").unwrap_err(),
        "`content` is empty"
    );
}

#[test]
fn old_text_must_match_exactly_one_entry() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    add(&memory, Target::Notes, "cargo test runs slowly here").unwrap();
    add(&memory, Target::Notes, "cargo fmt before commits").unwrap();

    let err = memory
        .change(Target::Notes, Change::Remove("cargo".into()))
        .unwrap_err();
    assert!(
        err.contains("`old_text` is in 2 entries of notes memory"),
        "{err}"
    );
    assert!(err.contains("1. cargo test runs slowly here"), "{err}");
    assert!(err.contains("2. cargo fmt before commits"), "{err}");

    let err = memory
        .change(
            Target::Notes,
            Change::Replace {
                old: "npm".into(),
                new: "x".into(),
            },
        )
        .unwrap_err();
    assert!(
        err.starts_with("no entry of notes memory contains `old_text`"),
        "{err}"
    );
    let err = memory
        .change(Target::Notes, Change::Remove(" ".into()))
        .unwrap_err();
    assert!(err.contains("`old_text` is empty"), "{err}");
    // Nothing changed.
    assert_eq!(memory.entries(Target::Notes).unwrap().len(), 2);
}

#[test]
fn changes_over_the_limit_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let memory = small(home.path(), 20);
    add(&memory, Target::Project, "0123456789").unwrap();
    // 10 + 3 for the separator + 8 = 21 characters.
    let err = add(&memory, Target::Project, "abcdefgh").unwrap_err();
    assert!(
        err.starts_with("not saved: project memory would hold 21 of its 20 characters"),
        "{err}"
    );
    assert!(err.contains("1. 0123456789 (10 characters)"), "{err}");
    assert_eq!(memory.entries(Target::Project).unwrap(), ["0123456789"]);
    // 7 more characters fit exactly; characters count, not bytes.
    let changed = add(&memory, Target::Project, "가나다라마바사").unwrap();
    assert_eq!((changed.chars, changed.limit), (20, 20));
    let err = memory
        .change(
            Target::Project,
            Change::Replace {
                old: "가나".into(),
                new: "가나다라마바사아".into(),
            },
        )
        .unwrap_err();
    assert!(err.contains("would hold 21 of its 20"), "{err}");
}

#[test]
fn a_store_over_its_limit_can_still_shrink() {
    let home = tempfile::tempdir().unwrap();
    let memory = small(home.path(), 10);
    let path = memory.path(Target::Notes);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "a long note written by hand\n§\nanother one\n").unwrap();
    let changed = memory
        .change(Target::Notes, Change::Remove("another".into()))
        .unwrap();
    assert_eq!(changed.chars, 27);
    let err = add(&memory, Target::Notes, "x").unwrap_err();
    assert!(err.contains("would hold 31 of its 10"), "{err}");
}

#[test]
fn secrets_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    let key = format!("sk-ant-api03-{}", "Ab1".repeat(10));
    let err = add(&memory, Target::User, &format!("API key: {key}")).unwrap_err();
    assert!(err.starts_with("not saved: the content looks like a secret (an Anthropic API key)"));
    assert!(
        !err.contains(&key),
        "the refusal must not repeat the secret"
    );
    let pem = format!("-----BEGIN RSA {} KEY-----\nMIIE", "PRIVATE");
    add(&memory, Target::Notes, "fine").unwrap();
    let err = memory
        .change(
            Target::Notes,
            Change::Replace {
                old: "fine".into(),
                new: pem,
            },
        )
        .unwrap_err();
    assert!(err.contains("(a private key)"), "{err}");
    assert!(!memory.path(Target::User).exists());
    assert_eq!(memory.entries(Target::Notes).unwrap(), ["fine"]);
}

#[test]
fn files_are_private() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    for target in Target::ALL {
        add(&memory, target, "x").unwrap();
        let path = memory.path(target);
        assert_eq!(mode(&path), 0o600, "{}", path.display());
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
    assert_eq!(mode(&home.path().join("projects")), 0o700);
    // A file made readable by hand becomes private again on the next write.
    let path = memory.path(Target::User);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    add(&memory, Target::User, "y").unwrap();
    assert_eq!(mode(&path), 0o600);
    // No temporary file is left behind.
    let names: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn project_memory_lives_under_a_hash_of_the_root() {
    let home = Path::new("/state");
    let memory = Memory::new(home, Path::new("/work/project"), Limits::default());
    // FNV-1a of the path bytes: fixed, so the folder never moves.
    assert_eq!(project_key(Path::new("")), "cbf29ce484222325");
    assert_eq!(project_key(Path::new("a")), "af63dc4c8601ec8c");
    let key = project_key(Path::new("/work/project"));
    assert_eq!(key.len(), 16);
    assert_ne!(key, project_key(Path::new("/work/project2")));
    assert_eq!(
        memory.path(Target::Project),
        Path::new("/state/projects").join(&key).join("notes.md")
    );
    assert_eq!(
        memory.path(Target::User),
        Path::new("/state/memory/user.md")
    );
    assert_eq!(
        memory.path(Target::Notes),
        Path::new("/state/memory/notes.md")
    );
}

#[test]
fn a_folder_in_a_git_repository_uses_the_repository_root() {
    let repo = crate::task::git::testing::repo();
    let root = repo.path().canonicalize().unwrap();
    let sub = root.join("src/deep");
    fs::create_dir_all(&sub).unwrap();
    let home = tempfile::tempdir().unwrap();
    let from_sub = Memory::for_folder(home.path(), &sub, Limits::default());
    assert_eq!(from_sub.project_root(), root);
    assert_eq!(
        from_sub.path(Target::Project),
        Memory::new(home.path(), &root, Limits::default()).path(Target::Project)
    );
    // Outside git the folder itself is the project.
    let plain = tempfile::tempdir().unwrap();
    let outside = Memory::for_folder(home.path(), plain.path(), Limits::default());
    assert_eq!(outside.project_root(), plain.path());
}

#[test]
fn clear_empties_one_store() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    assert_eq!(memory.clear(Target::User).unwrap(), 0);
    add(&memory, Target::User, "a").unwrap();
    add(&memory, Target::User, "b").unwrap();
    add(&memory, Target::Notes, "c").unwrap();
    assert_eq!(memory.clear(Target::User).unwrap(), 2);
    assert!(memory.entries(Target::User).unwrap().is_empty());
    assert_eq!(memory.entries(Target::Notes).unwrap(), ["c"]);
}

#[test]
fn reads_the_limits() {
    let none = |_: &str| None;
    assert_eq!(Limits::read(none).unwrap(), Limits::default());
    let set = |key: &str| match key {
        "JUNEES_MEMORY_USER_CHARS" => Some("100".to_string()),
        "JUNEES_MEMORY_PROJECT_CHARS" => Some("8000".to_string()),
        _ => None,
    };
    assert_eq!(
        Limits::read(set).unwrap(),
        Limits {
            user: 100,
            notes: DEFAULT_NOTES_CHARS,
            project: 8000
        }
    );
    for bad in ["0", "8001", "-1", "lots"] {
        let err =
            Limits::read(|key: &str| (key == "JUNEES_MEMORY_NOTES_CHARS").then(|| bad.to_string()))
                .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("JUNEES_MEMORY_NOTES_CHARS must be a whole number from 1 to 8000, got `{bad}`")
        );
    }
}

#[test]
fn the_prompt_section_shows_every_store_and_its_use() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    add(&memory, Target::User, "Name: 성준").unwrap();
    add(&memory, Target::User, "Answers in Korean.").unwrap();
    add(&memory, Target::Project, "Run cargo with -j 1.").unwrap();
    let section = memory.snapshot().section(Mode::Conversation);
    assert!(
        section.starts_with("\nMEMORY (persistent notes)\n"),
        "{section}"
    );
    assert!(section.contains("shows here only from the next session"));
    assert!(section.contains("tells you a preference or a fact about themselves"));
    assert!(section.contains("Do not save task progress, temporary state"));
    assert!(section.contains(
        "\n[About the user; user: 29/1500 characters]\nName: 성준\n§\nAnswers in Korean.\n"
    ));
    assert!(section.contains(
        "\n[Notes on the environment and conventions; notes: 0/2500 characters]\n(empty)\n"
    ));
    assert!(section.contains(
        "\n[Notes on this project (/work/project); project: 20/2500 characters]\nRun cargo with -j 1.\n"
    ));

    let task = memory.snapshot().section(Mode::Task);
    assert!(task.contains("you can only change the notes on this project (target project)"));
    assert!(task.contains("Name: 성준"));

    let read_only = memory.snapshot().section(Mode::ReadOnly);
    assert!(read_only.contains("they cannot be changed in this session"));
    assert!(read_only.contains("Run cargo with -j 1."));
    assert!(!read_only.contains("memory tool"));
    // Claude Code gets nothing while there is nothing to read.
    let empty = Memory::new(home.path(), Path::new("/elsewhere"), Limits::default());
    memory.clear(Target::User).unwrap();
    assert_eq!(empty.snapshot().section(Mode::ReadOnly), "");
    assert!(
        empty
            .snapshot()
            .section(Mode::Conversation)
            .contains("(empty)")
    );
}

#[test]
fn the_prompt_section_is_bounded_by_the_limits() {
    let home = tempfile::tempdir().unwrap();
    let memory = small(home.path(), 5);
    let path = memory.path(Target::Notes);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "가나다라마바사\n").unwrap();
    let section = memory.snapshot().section(Mode::Conversation);
    assert!(section.contains(
        "notes: 7/5 characters]\n가나다라마\n[... cut at the 5-character limit of this memory: shorten it]\n"
    ), "{section}");
    // An unreadable store is named, not fatal.
    fs::write(
        memory.path(Target::User).parent().unwrap().join("user.md"),
        [0xff, 0xfe],
    )
    .unwrap();
    let section = memory.snapshot().section(Mode::Conversation);
    assert!(
        section.contains("(cannot read this memory: cannot read "),
        "{section}"
    );

    // At the highest limits the section still fits one argument (128 KiB)
    // in the widest characters.
    let big = Limits {
        user: MAX_CHARS,
        notes: MAX_CHARS,
        project: MAX_CHARS,
    };
    let memory = Memory::new(home.path(), Path::new("/big"), big);
    for target in Target::ALL {
        let path = memory.path(target);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "😀".repeat(MAX_CHARS * 2)).unwrap();
    }
    let section = memory.snapshot().section(Mode::ReadOnly);
    assert!(section.len() < 128 * 1024, "{}", section.len());
}

#[tokio::test]
async fn the_tool_changes_memory_and_reports_usage() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    let tool = MemoryTool::new(memory.clone(), Mode::Conversation);
    assert_eq!(tool.name(), "memory");
    assert_eq!(
        tool.input_schema()["properties"]["target"]["enum"],
        json!(["user", "notes", "project"])
    );
    let output = tool
        .run(json!({"target": "notes", "action": "add", "content": "Server has 2 vCPU."}))
        .await;
    assert_eq!(
        output,
        ToolOutput::ok(
            "added (notes memory: 1 entries, 18/2500 characters). The system prompt keeps \
             the memory this session started with; the change shows there from the next \
             session."
        )
    );
    let output = tool
        .run(json!({"target": "notes", "action": "replace", "old_text": "2 vCPU", "content": "Server has 4 vCPU."}))
        .await;
    assert!(
        output
            .content
            .starts_with("replaced (notes memory: 1 entries"),
        "{}",
        output.content
    );
    let output = tool
        .run(json!({"target": "notes", "action": "remove", "old_text": "vCPU"}))
        .await;
    assert!(
        output
            .content
            .starts_with("removed (notes memory: 0 entries, 0/2500")
    );
    assert!(memory.entries(Target::Notes).unwrap().is_empty());

    for (input, message) in [
        (
            json!({"action": "add"}),
            "missing required string argument `target`",
        ),
        (
            json!({"target": "all", "action": "add"}),
            "unknown target `all`; use user, notes or project",
        ),
        (
            json!({"target": "user", "action": "add"}),
            "missing required string argument `content`",
        ),
        (
            json!({"target": "user", "action": "replace", "content": "x"}),
            "missing required string argument `old_text`",
        ),
        (
            json!({"target": "user", "action": "forget"}),
            "unknown action `forget`; use add, replace or remove",
        ),
        (
            json!({"target": "user", "action": "remove", "old_text": "nothing"}),
            "no entry of user memory contains `old_text`. Current entries:\n(none)",
        ),
    ] {
        assert_eq!(tool.run(input).await, ToolOutput::error(message));
    }
}

#[tokio::test]
async fn task_sessions_change_only_the_project_notes() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    let tool = MemoryTool::new(memory.clone(), Mode::Task);
    assert_eq!(
        tool.input_schema()["properties"]["target"]["enum"],
        json!(["project"])
    );
    for target in ["user", "notes"] {
        let output = tool
            .run(json!({"target": target, "action": "add", "content": "x"}))
            .await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .starts_with("task sessions can only change the notes on this project"),
            "{}",
            output.content
        );
    }
    assert!(!memory.path(Target::User).exists());
    assert!(!memory.path(Target::Notes).exists());
    let output = tool
        .run(json!({"target": "project", "action": "add", "content": "Tests need -j 1."}))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(
        memory.entries(Target::Project).unwrap(),
        ["Tests need -j 1."]
    );
}

#[test]
fn a_snapshot_keeps_what_it_read() {
    let home = tempfile::tempdir().unwrap();
    let memory = memory(home.path());
    add(&memory, Target::User, "first").unwrap();
    let snapshot = memory.snapshot();
    add(&memory, Target::User, "second").unwrap();
    let before = snapshot.section(Mode::Conversation);
    assert!(before.contains("first") && !before.contains("second"));
    assert!(
        memory
            .snapshot()
            .section(Mode::Conversation)
            .contains("first\n§\nsecond")
    );
}
