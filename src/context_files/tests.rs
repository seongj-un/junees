use std::path::Path;

use super::*;

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn defaults() -> Vec<String> {
    names(None).unwrap()
}

/// The file paths that go in, relative to `base`.
fn shown(files: &ContextFiles, base: &Path) -> Vec<String> {
    let base = fs::canonicalize(base).unwrap();
    files
        .files
        .iter()
        .map(|file| file.path.strip_prefix(&base).unwrap().display().to_string())
        .collect()
}

#[test]
fn walks_from_the_top_of_the_repository_down_to_the_working_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    // Above the repository: never read.
    write(&base.join("AGENTS.md"), "outside the repository\n");
    fs::create_dir_all(base.join("repo/.git")).unwrap();
    write(&base.join("repo/CLAUDE.md"), "root claude\n");
    write(&base.join("repo/AGENTS.md"), "root agents\n");
    write(&base.join("repo/sub/JUNEES.md"), "sub junees\n");
    write(&base.join("repo/other/AGENTS.md"), "a sibling folder\n");
    fs::create_dir_all(base.join("repo/sub/deep")).unwrap();

    let files = ContextFiles::load(&base.join("repo/sub/deep"), None, &defaults());
    assert_eq!(
        shown(&files, base),
        ["repo/AGENTS.md", "repo/CLAUDE.md", "repo/sub/JUNEES.md"]
    );
    let canonical = fs::canonicalize(base).unwrap();
    assert_eq!(
        files.levels,
        [
            canonical.join("repo/sub/deep"),
            canonical.join("repo/sub"),
            canonical.join("repo"),
        ]
    );
    let section = files.section();
    let at = |text: &str| section.find(text).unwrap();
    assert!(at("root agents") < at("root claude") && at("root claude") < at("sub junees"));
    assert!(!section.contains("outside the repository"));
    assert!(!section.contains("sibling"));
    assert!(
        section.starts_with("\n\n# Project instructions\n"),
        "{section}"
    );
    assert!(section.contains("They rank below the user's own messages"));
    assert!(section.contains(&format!(
        "<project-instructions path=\"{}\">\nroot agents\n</project-instructions>\n",
        canonical.join("repo/AGENTS.md").display()
    )));

    // A worktree's `.git` is a file; it is the top as well.
    write(&base.join("repo/sub/.git"), "gitdir: /elsewhere\n");
    let files = ContextFiles::load(&base.join("repo/sub/deep"), None, &defaults());
    assert_eq!(shown(&files, base), ["repo/sub/JUNEES.md"]);
    // The working folder may be the top itself.
    let files = ContextFiles::load(&base.join("repo"), None, &defaults());
    assert_eq!(shown(&files, base), ["repo/AGENTS.md", "repo/CLAUDE.md"]);
}

#[test]
fn stops_at_home_and_reads_only_the_folder_itself_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    write(&base.join("AGENTS.md"), "above home\n");
    write(&base.join("home/AGENTS.md"), "home agents\n");
    write(&base.join("home/notes/JUNEES.md"), "notes\n");
    fs::create_dir_all(base.join("home/notes/today")).unwrap();
    let home = base.join("home");

    let files = ContextFiles::load(&base.join("home/notes/today"), Some(&home), &defaults());
    assert_eq!(
        shown(&files, base),
        ["home/AGENTS.md", "home/notes/JUNEES.md"]
    );
    assert!(!files.section().contains("above home"));
    // HOME through a link counts the same.
    std::os::unix::fs::symlink(&home, base.join("home-link")).unwrap();
    let files = ContextFiles::load(
        &base.join("home/notes/today"),
        Some(&base.join("home-link")),
        &defaults(),
    );
    assert_eq!(
        shown(&files, base),
        ["home/AGENTS.md", "home/notes/JUNEES.md"]
    );

    // Neither in a repository nor under HOME: the folder's own files only
    // (unless the temporary folder itself is inside a repository).
    write(&base.join("shared/AGENTS.md"), "someone else's file\n");
    write(&base.join("shared/mine/AGENTS.md"), "mine\n");
    let canonical = fs::canonicalize(base).unwrap();
    if !canonical.ancestors().any(|dir| dir.join(".git").exists()) {
        let files = ContextFiles::load(&base.join("shared/mine"), Some(&home), &defaults());
        assert_eq!(shown(&files, base), ["shared/mine/AGENTS.md"]);
        assert_eq!(files.levels, [canonical.join("shared/mine")]);
    }
}

#[test]
fn cuts_each_file_and_all_of_them_at_their_limits() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".git")).unwrap();
    // 3-byte characters, so the cut falls inside one.
    let big = "가".repeat(7000);
    write(&repo.join("AGENTS.md"), &big);
    let files = ContextFiles::load(repo, None, &defaults());
    let file = &files.files[0];
    assert_eq!(file.size, 21_000);
    assert_eq!(file.cut, Some(Cut::File));
    assert_eq!(file.text, "가".repeat(FILE_LIMIT / 3));
    let section = files.section();
    assert!(
        section.contains("가\n[cut here: the file has 21000 bytes; only the first 16 KiB go in]\n"),
        "{}",
        &section[section.len() - 200..]
    );

    // Three full files use up the room; the next one is left out.
    write(&repo.join("AGENTS.md"), &"a".repeat(FILE_LIMIT + 5));
    write(&repo.join("JUNEES.md"), &"j".repeat(FILE_LIMIT));
    write(&repo.join("CLAUDE.md"), &"c".repeat(FILE_LIMIT + 10));
    write(&repo.join("sub/AGENTS.md"), &"n".repeat(FILE_LIMIT - 4));
    let files = ContextFiles::load(&repo.join("sub"), None, &defaults());
    assert_eq!(files.files.len(), 3);
    assert_eq!(files.files[0].cut, Some(Cut::File));
    assert_eq!(files.files[1].cut, None);
    assert_eq!(files.files[2].cut, Some(Cut::File));
    let total: usize = files.files.iter().map(|file| file.text.len()).sum();
    assert!(total <= TOTAL_LIMIT, "{total}");
    assert_eq!(files.skipped.len(), 1);
    assert!(files.skipped[0].path.ends_with("sub/AGENTS.md"));
    assert_eq!(
        files.skipped[0].why,
        "left out: the 48 KiB limit of all files is used up"
    );
    assert!(!files.section().contains("nnnn"));

    // With a smaller first file the last one is cut at the total limit.
    write(&repo.join("AGENTS.md"), "short\n");
    let files = ContextFiles::load(&repo.join("sub"), None, &defaults());
    assert_eq!(files.files.len(), 4);
    assert_eq!(files.files[3].cut, Some(Cut::Total));
    assert_eq!(
        files
            .files
            .iter()
            .map(|file| file.text.len())
            .sum::<usize>(),
        TOTAL_LIMIT
    );
    assert!(files.section().contains(
        "[cut here: the file has 16380 bytes; the rest did not fit the limit of all project \
         instruction files]"
    ));
    let overview = files.overview();
    assert!(overview.contains("CLAUDE.md: 16384 of 16394 bytes (only the first 16 KiB go in)"));
    assert!(
        overview.starts_with("project instructions (49152 bytes;"),
        "{overview}"
    );
}

#[test]
fn a_section_fits_a_smaller_room() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".git")).unwrap();
    write(&repo.join("AGENTS.md"), &"a".repeat(10_000));
    write(&repo.join("CLAUDE.md"), &"😀".repeat(3_000));
    let files = ContextFiles::load(repo, None, &defaults());
    let full = files.section();
    assert!(full.len() > 20_000);
    assert_eq!(files.section_within(usize::MAX), full);
    for max in [15_000, 9_000, 1_000] {
        let section = files.section_within(max);
        assert!(section.len() <= max, "{max}: {}", section.len());
        assert!(section.contains("[cut here"), "{max}");
    }
    let section = files.section_within(9_000);
    assert!(section.contains("1 more files left out"), "{section}");
    assert_eq!(files.section_within(10), "");
}

#[test]
fn follows_links_only_inside_the_top_folder_and_reads_a_file_once() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let repo = base.join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    write(&base.join("secret.md"), "outside text\n");
    write(&repo.join("AGENTS.md"), "shared rules\n");
    write(&repo.join("docs/rules.md"), "linked inside\n");
    let link = std::os::unix::fs::symlink;
    // CLAUDE.md as a link to AGENTS.md, the usual way to share one file.
    link("AGENTS.md", repo.join("CLAUDE.md")).unwrap();
    link("../secret.md", repo.join("JUNEES.md")).unwrap();
    fs::create_dir_all(repo.join("sub")).unwrap();
    link("../docs/rules.md", repo.join("sub/AGENTS.md")).unwrap();
    link("missing.md", repo.join("sub/CLAUDE.md")).unwrap();
    fs::create_dir_all(repo.join("sub/JUNEES.md")).unwrap();

    let files = ContextFiles::load(&repo.join("sub"), None, &defaults());
    assert_eq!(
        shown(&files, base),
        ["repo/AGENTS.md", "repo/sub/AGENTS.md"]
    );
    assert_eq!(files.files[1].text, "linked inside\n");
    let section = files.section();
    assert!(!section.contains("outside text"));
    assert_eq!(section.matches("shared rules").count(), 1);
    let skipped: Vec<(String, String)> = files
        .skipped
        .iter()
        .map(|skipped| {
            let name = skipped.path.strip_prefix(fs::canonicalize(base).unwrap());
            (name.unwrap().display().to_string(), skipped.why.clone())
        })
        .collect();
    let canonical = fs::canonicalize(base).unwrap();
    assert_eq!(
        skipped,
        [
            (
                "repo/JUNEES.md".to_string(),
                format!(
                    "a link to {}, outside {}",
                    canonical.join("secret.md").display(),
                    canonical.join("repo").display()
                )
            ),
            (
                "repo/CLAUDE.md".to_string(),
                format!(
                    "the same file as {}",
                    canonical.join("repo/AGENTS.md").display()
                )
            ),
            (
                "repo/sub/JUNEES.md".to_string(),
                "not a regular file".to_string()
            ),
            (
                "repo/sub/CLAUDE.md".to_string(),
                "a link to nothing".to_string()
            ),
        ]
    );
    let overview = files.overview();
    assert!(overview.contains("  skipped "), "{overview}");
    let summary = files.summary().unwrap();
    assert!(summary.starts_with("(project instructions: "), "{summary}");
    assert!(summary.contains("skipped "), "{summary}");
}

#[test]
fn a_fifo_is_skipped_without_waiting() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".git")).unwrap();
    let fifo = std::ffi::CString::new(repo.join("AGENTS.md").to_str().unwrap()).unwrap();
    // SAFETY: a valid C string; mkfifo only creates the node.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let files = ContextFiles::load(repo, None, &defaults());
    assert!(files.files.is_empty());
    assert_eq!(files.skipped[0].why, "not a regular file");
}

#[test]
fn hidden_characters_are_escaped() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".git")).unwrap();
    write(
        &repo.join("AGENTS.md"),
        "line\u{1b}[2K\rfake\u{202e}abc\n한글\ttab\r\n",
    );
    let section = ContextFiles::load(repo, None, &defaults()).section();
    assert!(
        section.contains("line\\u{1b}[2K\\rfake\\u{202e}abc\n한글\ttab\r\n"),
        "{section}"
    );
    assert!(!section.contains('\u{1b}') && !section.contains('\u{202e}'));
}

#[test]
fn the_setting_names_the_files_or_turns_them_off() {
    assert_eq!(
        names(None).unwrap(),
        ["AGENTS.md", "JUNEES.md", "CLAUDE.md"]
    );
    for off in ["", "  ", "off", "OFF", "none", "0", ","] {
        assert!(names(Some(off)).unwrap().is_empty(), "{off:?}");
    }
    assert_eq!(
        names(Some(" RULES.md, AGENTS.md ,RULES.md")).unwrap(),
        ["RULES.md", "AGENTS.md"]
    );
    for bad in ["../AGENTS.md", "docs/AGENTS.md", "..", "a\u{1b}b"] {
        let err = names(Some(bad)).unwrap_err().to_string();
        assert!(
            err.starts_with("JUNEES_CONTEXT_FILES takes file names"),
            "{err}"
        );
    }
    let setting = Setting::read(|key| match key {
        "JUNEES_CONTEXT_FILES" => Some("RULES.md".into()),
        "HOME" => Some(" /home/tester ".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(
        setting,
        Setting {
            names: vec!["RULES.md".into()],
            home: Some(PathBuf::from("/home/tester")),
        }
    );
    assert_eq!(Setting::read(|_| None).unwrap(), Setting::with_home(None));

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".git")).unwrap();
    write(&repo.join("AGENTS.md"), "agents\n");
    write(&repo.join("RULES.md"), "rules\n");
    let files = setting.load(repo);
    assert_eq!(shown(&files, repo), ["RULES.md"]);
    let off = Setting {
        names: Vec::new(),
        home: None,
    }
    .load(repo);
    assert!(off.is_off());
    assert_eq!(off.section(), "");
    assert_eq!(off.summary(), None);
    assert_eq!(
        off.overview(),
        "project instructions: off (JUNEES_CONTEXT_FILES)\n"
    );
    let empty = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty.path().join(".git")).unwrap();
    let none = Setting::with_home(None).load(empty.path());
    assert!(
        none.overview()
            .starts_with("project instructions: none found")
    );
}
