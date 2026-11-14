use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::tools::{EditFile, FileAccess, Scope, Tool, WriteFile};

/// A store in a temporary folder and session `s1` in it.
fn session() -> (tempfile::TempDir, Store, SessionStore) {
    let home = tempfile::tempdir().unwrap();
    let store = Store::new(home.path());
    let session = store.session("s1").unwrap();
    (home, store, session)
}

async fn write(turn: &Arc<Turn>, path: &Path, content: &str) {
    let output = scope(
        Some(Arc::clone(turn)),
        WriteFile::default().run(json!({"path": path, "content": content})),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
}

async fn edit(turn: &Arc<Turn>, path: &Path, old: &str, new: &str) {
    let output = scope(
        Some(Arc::clone(turn)),
        EditFile::default().run(json!({"path": path, "old_string": old, "new_string": new})),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
}

/// One turn writing `content` to `path`, finished.
async fn turn_writing(session: &SessionStore, path: &Path, content: &str) -> u64 {
    let turn = Arc::new(Turn::new(session.clone()));
    write(&turn, path, content).await;
    turn.finish();
    turn.number().unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[tokio::test]
async fn records_what_a_turn_changed_first_content_only() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let made = work.path().join("new.txt");
    let changed = work.path().join("old.txt");
    fs::write(&changed, "one two").unwrap();
    fs::set_permissions(&changed, fs::Permissions::from_mode(0o640)).unwrap();

    let turn = Arc::new(Turn::new(session.clone()));
    assert_eq!(turn.number(), None, "no number before a change");
    write(&turn, &made, "made").await;
    edit(&turn, &changed, "one", "1").await;
    edit(&turn, &changed, "two", "2").await;
    turn.finish();

    let list = session.list().unwrap();
    assert_eq!(list.len(), 1);
    let manifest = &list[0];
    assert_eq!(manifest.turn, 1);
    assert_eq!(manifest.files.len(), 2, "one entry per file: {manifest:?}");
    let created = &manifest.files[0];
    assert_eq!(created.before, "absent");
    assert_eq!(created.change(), Change::Created);
    assert_eq!(
        created.after_sha256.as_deref(),
        Some(sha256(b"made").as_str())
    );
    let modified = &manifest.files[1];
    assert_eq!(modified.change(), Change::Modified);
    assert_eq!(modified.mode, Some(0o640));
    assert_eq!(
        modified.after_sha256.as_deref(),
        Some(sha256(b"1 2").as_str())
    );
    // The content of before the first change, not the second.
    let dir = session.dir().join("1");
    let content = modified.content.as_deref().unwrap();
    assert_eq!(fs::read(dir.join(content)).unwrap(), b"one two");
    assert!(modified.restorable && created.restorable);

    // Private: the folders 0700, the files 0600.
    assert_eq!(mode(session.dir()), 0o700);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join(MANIFEST)), 0o600);
    assert_eq!(mode(&dir.join(content)), 0o600);
}

#[tokio::test]
async fn a_turn_that_changes_no_file_leaves_nothing() {
    let (_home, _store, session) = session();
    let turn = Turn::new(session.clone());
    turn.finish();
    assert!(!session.dir().exists());
    assert!(session.list().unwrap().is_empty());
}

#[tokio::test]
async fn a_large_file_is_noted_and_not_restored() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let big = work.path().join("big.bin");
    fs::write(&big, vec![b'a'; MAX_FILE_BYTES as usize + 1]).unwrap();
    let number = turn_writing(&session, &big, "small").await;
    let manifest = &session.list().unwrap()[0];
    assert_eq!(manifest.files[0].before, "too_large");
    assert_eq!(manifest.files[0].size, Some(MAX_FILE_BYTES + 1));
    assert!(manifest.files[0].content.is_none());
    assert!(!manifest.files[0].restorable);

    let undone = session.undo(number, true, &FileAccess::default()).unwrap();
    assert!(undone.restored.is_empty());
    assert_eq!(undone.skipped[0].1, reason::TOO_LARGE);
    assert_eq!(fs::read_to_string(&big).unwrap(), "small");
}

#[tokio::test]
async fn undo_puts_back_content_and_bits_and_removes_what_was_made() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let old = work.path().join("old.sh");
    fs::write(&old, "echo old\n").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o750)).unwrap();
    let made = work.path().join("dir/made.txt");

    let turn = Arc::new(Turn::new(session.clone()));
    write(&turn, &old, "echo new\n").await;
    write(&turn, &made, "made").await;
    turn.finish();

    let undone = session.undo(1, false, &FileAccess::default()).unwrap();
    assert_eq!(undone.turns, vec![1]);
    assert!(undone.skipped.is_empty(), "{undone:?}");
    assert_eq!(undone.restored.len(), 2);
    assert_eq!(fs::read_to_string(&old).unwrap(), "echo old\n");
    assert_eq!(mode(&old), 0o750);
    assert!(!made.exists());
    assert!(
        undone
            .note()
            .starts_with("[되돌림] 변경 기록 1의 파일 변경을 되돌렸습니다: ")
    );
    assert!(undone.note().contains("bash로 바꾼 파일은 기록되지 않아"));
}

#[tokio::test]
async fn a_file_changed_since_is_skipped_unless_forced() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("a.txt");
    fs::write(&file, "before").unwrap();
    let number = turn_writing(&session, &file, "by the turn").await;
    // Someone (bash, a person) changed it afterwards.
    fs::write(&file, "by hand").unwrap();
    assert!(!session.list().unwrap()[0].files[0].restorable);

    let undone = session.undo(number, false, &FileAccess::default()).unwrap();
    assert_eq!(
        undone.skipped,
        vec![(file.display().to_string(), reason::CHANGED.to_string())]
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "by hand");
    assert!(undone.note().contains("(건너뜀: "));
    assert!(undone.note().contains("바뀐 뒤 수정됨"));
    assert_eq!(undone.checkpoint, None, "nothing changed, nothing recorded");

    let forced = session.undo(number, true, &FileAccess::default()).unwrap();
    assert!(forced.skipped.is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), "before");
}

#[tokio::test]
async fn a_made_file_changed_since_stays_even_forced() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("made.txt");
    let number = turn_writing(&session, &file, "made").await;
    fs::write(&file, "work of someone").unwrap();
    let undone = session.undo(number, true, &FileAccess::default()).unwrap();
    assert_eq!(undone.skipped[0].1, reason::KEPT_CREATED);
    assert_eq!(fs::read_to_string(&file).unwrap(), "work of someone");
}

#[tokio::test]
async fn a_target_turned_into_a_link_is_skipped() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("a.txt");
    let elsewhere = work.path().join("elsewhere.txt");
    fs::write(&file, "before").unwrap();
    let number = turn_writing(&session, &file, "after").await;
    fs::write(&elsewhere, "after").unwrap();
    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();

    let undone = session.undo(number, true, &FileAccess::default()).unwrap();
    assert_eq!(undone.skipped[0].1, reason::LINK);
    assert_eq!(
        fs::read_to_string(&elsewhere).unwrap(),
        "after",
        "not followed"
    );
}

#[tokio::test]
async fn the_write_rules_apply_to_an_undo() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("a.txt");
    fs::write(&file, "before").unwrap();
    let number = turn_writing(&session, &file, "after").await;

    // An access whose scope is `work` only, as an unattended run's.
    let access = FileAccess::default().with_scope(Scope::new(
        Some(work.path().to_path_buf()),
        Vec::new(),
        Vec::new(),
        None,
        None,
    ));
    let undone = session.undo(number, false, &access).unwrap();
    assert_eq!(undone.skipped[0].1, reason::REFUSED);
    assert_eq!(fs::read_to_string(&file).unwrap(), "after");
}

#[tokio::test]
async fn turns_are_undone_newest_first_and_an_undo_can_be_undone() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("a.txt");
    fs::write(&file, "v0").unwrap();
    let first = turn_writing(&session, &file, "v1").await;
    let second = turn_writing(&session, &file, "v2").await;
    assert_eq!((first, second), (1, 2));

    // Turn 1 alone would find v2, not v1; but an undo from turn 1 undoes
    // turn 2 first, so the list marks both restorable.
    let list = session.list().unwrap();
    assert!(list[0].files[0].restorable && list[1].files[0].restorable);
    fs::write(&file, "by hand").unwrap();
    let list = session.list().unwrap();
    assert!(!list[0].files[0].restorable && !list[1].files[0].restorable);
    fs::write(&file, "v2").unwrap();

    // Turn 1 and every later one: v2 -> v1 (turn 2) -> v0 (turn 1).
    let undone = session.undo(first, false, &FileAccess::default()).unwrap();
    assert_eq!(undone.turns, vec![2, 1]);
    assert!(undone.skipped.is_empty(), "{undone:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "v0");
    assert_eq!(undone.checkpoint, Some(3));
    assert!(
        undone
            .note()
            .starts_with("[되돌림] 변경 기록 1, 2의 파일 변경을")
    );

    let list = session.list().unwrap();
    let undo_turn = list.last().unwrap();
    assert_eq!((undo_turn.turn, undo_turn.undid.clone()), (3, vec![2, 1]));

    // Undoing the undo brings the turns' work back.
    let again = session.undo(3, false, &FileAccess::default()).unwrap();
    assert!(again.skipped.is_empty(), "{again:?}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "v2");
}

#[tokio::test]
async fn limits_remove_the_oldest_turns() {
    let (home, store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("a.txt");
    fs::write(&file, vec![b'x'; 1000]).unwrap();
    let first = turn_writing(&session, &file, &"y".repeat(1000)).await;
    let second = turn_writing(&session, &file, "z").await;

    // Room for one turn of ~1 KB: the older goes, the one being kept stays.
    limit_session(session.dir(), 1500, &session.dir().join(second.to_string())).unwrap();
    let turns: Vec<u64> = session.list().unwrap().iter().map(|m| m.turn).collect();
    assert_eq!(turns, vec![second]);
    assert!(!session.dir().join(first.to_string()).exists());

    store.limit_total(0, Path::new("/nowhere")).unwrap();
    assert!(session.list().unwrap().is_empty());

    // Age: everything is older than nothing.
    let other = store.session("s2").unwrap();
    turn_writing(&other, &file, "w").await;
    std::thread::sleep(Duration::from_millis(20));
    store.remove_older_than(Duration::from_secs(3600)).unwrap();
    assert_eq!(other.list().unwrap().len(), 1, "recent turns stay");
    store.remove_older_than(Duration::ZERO).unwrap();
    assert!(other.list().unwrap().is_empty());
    drop(home);
}

#[tokio::test]
async fn turn_numbers_go_on_after_old_turns_are_removed() {
    let (_home, store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("a.txt");
    turn_writing(&session, &file, "1").await;
    store.limit_total(0, Path::new("/nowhere")).unwrap();
    assert_eq!(turn_writing(&session, &file, "2").await, 2);
}

#[test]
fn notes_are_taken_once() {
    let (_home, _store, session) = session();
    assert!(session.take_notes().is_empty());
    session.add_note("[되돌림] 하나").unwrap();
    session.add_note("[되돌림] 둘\n줄").unwrap();
    assert_eq!(
        session.take_notes(),
        vec!["[되돌림] 하나", "[되돌림] 둘 줄"]
    );
    assert!(session.take_notes().is_empty());
}

#[test]
fn only_session_ids_name_a_session() {
    let store = Store::new(Path::new("/h"));
    assert!(store.session("20261113-abc_1").is_some());
    for bad in ["", "..", "a/b", "a b", &"a".repeat(65)] {
        assert!(store.session(bad).is_none(), "{bad:?}");
    }
}

#[test]
fn the_setting_turns_recording_off() {
    assert!(enabled(None));
    assert!(enabled(Some("true")));
    for off in ["0", "false", "OFF", " no "] {
        assert!(!enabled(Some(off)), "{off}");
    }
}

#[tokio::test]
async fn a_change_between_two_turns_makes_the_older_one_not_restorable() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    // Turns 1 and 2 change another file; turn 3 makes c.txt, something
    // outside changes it, turn 4 changes it again; then an undo from 3.
    let other = work.path().join("a.txt");
    turn_writing(&session, &other, "a1").await;
    turn_writing(&session, &other, "a2").await;
    let file = work.path().join("c.txt");
    assert_eq!(turn_writing(&session, &file, "C1").await, 3);
    fs::write(&file, "C1x").unwrap();
    assert_eq!(turn_writing(&session, &file, "C2").await, 4);

    let list = session.list().unwrap();
    assert!(list[3].files[0].restorable, "turn 4 alone goes back");
    assert!(!list[2].files[0].restorable, "turn 3 would find C1x");

    let undone = session.undo(3, false, &FileAccess::default()).unwrap();
    // Turn 4 put back C1x, then turn 3 was skipped: the file changed, so
    // it is restored, part of the way.
    assert_eq!(fs::read_to_string(&file).unwrap(), "C1x");
    let path = file.display().to_string();
    assert_eq!(undone.restored, vec![path.clone()], "{undone:?}");
    assert!(undone.skipped.is_empty(), "{undone:?}");
    assert_eq!(
        undone.partial,
        vec![Partial {
            path: path.clone(),
            until_turn: 4,
            skipped_turn: 3,
            reason: reason::KEPT_CREATED.to_string(),
        }]
    );
    let note = undone.note();
    assert!(
        note.starts_with(&format!(
            "[되돌림] 변경 기록 3, 4의 파일 변경을 되돌렸습니다: {path} (변경 기록 4까지 되돌림, 변경 기록 3은 건너뜀: {})",
            reason::KEPT_CREATED
        )),
        "{note}"
    );
    assert_eq!(note.matches("c.txt").count(), 1, "{note}");
    // Left alone at every turn: skipped, once. (Turn 5 is the undo.)
    fs::write(&file, "by hand").unwrap();
    let undone = session.undo(5, false, &FileAccess::default()).unwrap();
    assert!(
        undone.restored.is_empty() && undone.partial.is_empty(),
        "{undone:?}"
    );
    assert_eq!(undone.skipped, vec![(path, reason::CHANGED.to_string())]);
}

#[tokio::test]
async fn a_forged_manifest_path_is_not_written() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("a.txt");
    fs::write(&file, "before").unwrap();
    let number = turn_writing(&session, &file, "after").await;
    let manifest_path = session.dir().join(number.to_string()).join(MANIFEST);
    let original = fs::read_to_string(&manifest_path).unwrap();
    let target = work.path().join("other.txt");
    for (path, content) in [
        ("relative.txt".to_string(), "0.bin"),
        (
            format!("{}/../other.txt", work.path().join("sub").display()),
            "0.bin",
        ),
        (file.display().to_string(), "../../../x.bin"),
    ] {
        let mut manifest: Manifest = serde_json::from_str(&original).unwrap();
        manifest.files[0].path = path.clone();
        manifest.files[0].content = Some(content.to_string());
        manifest.files[0].after_sha256 = None;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let undone = session.undo(number, true, &FileAccess::default()).unwrap();
        assert_eq!(undone.skipped[0].1, reason::BAD_RECORD, "{path} {content}");
    }
    assert!(!target.exists());
    assert_eq!(fs::read_to_string(&file).unwrap(), "after");
}

#[tokio::test]
async fn a_file_that_cannot_be_read_is_written_unrecorded() {
    let (_home, _store, session) = session();
    let work = tempfile::tempdir().unwrap();
    let file = work.path().join("write-only.txt");
    fs::write(&file, "old").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o200)).unwrap();
    let turn = Arc::new(Turn::new(session.clone()));
    let output = scope(
        Some(Arc::clone(&turn)),
        WriteFile::default().run(json!({"path": file, "content": "new"})),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.contains("not recorded for /undo"),
        "{}",
        output.content
    );
    assert_eq!(turn.number(), None);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "new");
}

#[test]
fn a_planted_link_at_the_temporary_name_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manifest.json");
    write_private(&path, b"one").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"one");
    assert_eq!(mode(&path), 0o600);
    // Nothing but the file itself is left behind.
    let names: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("manifest.json")]);
}

#[test]
fn a_partial_line_takes_the_particle_its_number_is_read_with() {
    let part = |skipped_turn| Partial {
        path: "c.txt".into(),
        until_turn: skipped_turn + 1,
        skipped_turn,
        reason: "r".into(),
    };
    assert_eq!(
        part(2).text(),
        "변경 기록 3까지 되돌림, 변경 기록 2는 건너뜀: r"
    );
    assert_eq!(
        part(3).text(),
        "변경 기록 4까지 되돌림, 변경 기록 3은 건너뜀: r"
    );
    assert_eq!(
        part(9).text(),
        "변경 기록 10까지 되돌림, 변경 기록 9는 건너뜀: r"
    );
}
