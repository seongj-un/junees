use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;

use super::{FileAccess, Scope, temp_name};
use crate::tools::{EditFile, ReadFile, Tool, ToolOutput, ToolSettings, WriteFile};

/// A layout in a temporary folder: `work` (the working folder), `scratch`
/// (stands in for the temporary folders), `outside` and a fake `home`
/// with an ssh key and a `.bashrc`. The real temporary folder is not in
/// the scope: the test's own folder lives there.
struct Layout {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Layout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for folder in ["work", "scratch", "outside", "home/.ssh"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        std::fs::write(root.join("home/.ssh/id_ed25519"), "PRIVATE KEY\n").unwrap();
        std::fs::write(root.join("home/.bashrc"), "# rc\n").unwrap();
        std::fs::write(root.join("outside/notes.txt"), "outside text\n").unwrap();
        std::fs::write(root.join("work/a.txt"), "work text\n").unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    /// The scope of a run working in `work`, with `read` as `sandbox_read`.
    fn scope(&self, work: &str, read: &[&str]) -> FileAccess {
        FileAccess::default().with_scope(Scope::new(
            Some(self.path(work)),
            vec![self.path("scratch")],
            read.iter().map(|path| self.path(path)).collect(),
            Some(&self.path("home")),
            None,
        ))
    }
}

async fn write(access: &FileAccess, path: &Path, content: &str) -> ToolOutput {
    WriteFile::with_access(access.clone())
        .run(json!({"path": path, "content": content}))
        .await
}

async fn read(access: &FileAccess, path: &Path) -> ToolOutput {
    ReadFile::with_access(access.clone())
        .run(json!({"path": path}))
        .await
}

async fn edit(access: &FileAccess, path: &Path, old: &str, new: &str) -> ToolOutput {
    EditFile::with_access(access.clone())
        .run(json!({"path": path, "old_string": old, "new_string": new}))
        .await
}

#[tokio::test]
async fn unattended_writes_stay_in_the_working_and_temporary_folders() {
    let layout = Layout::new();
    let access = layout.scope("work", &[]);
    for path in ["work/new.txt", "work/deep/er/new.txt", "scratch/x.txt"] {
        let output = write(&access, &layout.path(path), "ok\n").await;
        assert!(!output.is_error, "{path}: {}", output.content);
        assert_eq!(std::fs::read_to_string(layout.path(path)).unwrap(), "ok\n");
    }
    let output = edit(&access, &layout.path("work/a.txt"), "work", "edited").await;
    assert!(!output.is_error, "{}", output.content);

    // A link in the working folder that leads out, and a `..` path.
    std::os::unix::fs::symlink(layout.path("outside/notes.txt"), layout.path("work/link")).unwrap();
    std::os::unix::fs::symlink(layout.path("outside"), layout.path("work/dirlink")).unwrap();
    for path in [
        layout.path("outside/notes.txt"),
        layout.path("work/../outside/notes.txt"),
        layout.path("work/link"),
        layout.path("work/dirlink/notes.txt"),
        layout.path("home/.bashrc"),
    ] {
        let output = write(&access, &path, "planted\n").await;
        assert!(output.is_error, "{}: {}", path.display(), output.content);
        assert!(
            output
                .content
                .contains("outside the folders this unattended run"),
            "{}",
            output.content
        );
        let output = edit(&access, &path, "text", "planted").await;
        assert!(output.is_error, "{}: {}", path.display(), output.content);
    }
    assert_eq!(
        std::fs::read_to_string(layout.path("outside/notes.txt")).unwrap(),
        "outside text\n"
    );
    assert_eq!(
        std::fs::read_to_string(layout.path("home/.bashrc")).unwrap(),
        "# rc\n"
    );
    // Missing folders outside are not made either.
    let output = write(&access, &layout.path("outside/new/dir/x.txt"), "x").await;
    assert!(output.is_error, "{}", output.content);
    assert!(!layout.path("outside/new").exists());
    let output = write(&access, &layout.path("work/dirlink/new/x.txt"), "x").await;
    assert!(output.is_error, "{}", output.content);
    assert!(!layout.path("outside/new").exists());
}

#[tokio::test]
async fn unattended_reads_stay_in_their_folders_and_off_credentials() {
    let layout = Layout::new();
    let access = layout.scope("work", &[]);
    let output = read(&access, &layout.path("work/a.txt")).await;
    assert!(output.content.contains("work text"), "{}", output.content);
    std::fs::write(layout.path("scratch/s.txt"), "scratch text\n").unwrap();
    let output = read(&access, &layout.path("scratch/s.txt")).await;
    assert!(
        output.content.contains("scratch text"),
        "{}",
        output.content
    );

    std::os::unix::fs::symlink(layout.path("home/.ssh/id_ed25519"), layout.path("work/key"))
        .unwrap();
    for path in ["outside/notes.txt", "home/.ssh/id_ed25519", "work/key"] {
        let output = read(&access, &layout.path(path)).await;
        assert!(output.is_error, "{path}: {}", output.content);
        assert!(
            !output.content.contains("PRIVATE KEY"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains("outside text"),
            "{}",
            output.content
        );
    }

    // `sandbox_read` opens more folders for reading only.
    let access = layout.scope("work", &["outside"]);
    let output = read(&access, &layout.path("outside/notes.txt")).await;
    assert!(
        output.content.contains("outside text"),
        "{}",
        output.content
    );
    let output = write(&access, &layout.path("outside/notes.txt"), "planted").await;
    assert!(output.is_error, "{}", output.content);

    // A job working in the home folder still keeps off the credentials.
    let access = layout.scope("home", &[]);
    let output = read(&access, &layout.path("home/.ssh/id_ed25519")).await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output.content.contains("credential file or folder"),
        "{}",
        output.content
    );
    let output = write(
        &access,
        &layout.path("home/.ssh/authorized_keys"),
        "ssh-ed25519 AAAA planted\n",
    )
    .await;
    assert!(output.is_error, "{}", output.content);
    assert!(!layout.path("home/.ssh/authorized_keys").exists());
    let output = read(&access, &layout.path("home/.bashrc")).await;
    assert!(!output.is_error, "{}", output.content);
}

/// Sandbox settings of a run without temporary folders, so the test's
/// own folder (in the real temporary folder) counts as outside.
fn tools(home: &Path) -> ToolSettings {
    ToolSettings {
        settings_file: Some(crate::tools::SettingsFile::new(
            home.join(".junees/config.toml"),
            Some(home.to_path_buf()),
        )),
        sandbox: Some(crate::sandbox::Settings {
            temp: Vec::new(),
            ..crate::sandbox::Settings::read(|_| None).unwrap()
        }),
        ..ToolSettings::default()
    }
}

#[tokio::test]
async fn scheduled_and_webhook_runs_get_the_scope_and_conversations_do_not() {
    let layout = Layout::new();
    let settings = tools(&layout.path("home"));
    let outside = layout.path("outside/notes.txt");
    // The working folder of a unit test is the crate, not `outside`.
    let cron = crate::cron::run::Trigger::Cron.tool_settings(&settings);
    let gh = tempfile::tempdir().unwrap();
    let work = layout.path("work");
    let webhook = crate::cron::run::Trigger::Webhook {
        route: "r",
        github_allow: &[],
        gh_config: gh.path(),
        context_dir: &work,
    }
    .tool_settings(&settings);
    for unattended in [&cron, &webhook] {
        let output = WriteFile::with_settings(unattended)
            .run(json!({"path": outside, "content": "planted"}))
            .await;
        assert!(output.is_error, "{}", output.content);
        let output = ReadFile::with_settings(unattended)
            .run(json!({"path": layout.path("home/.ssh/id_ed25519")}))
            .await;
        assert!(output.is_error, "{}", output.content);
        assert!(!output.content.contains("PRIVATE KEY"));
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "outside text\n");
    // A conversation asks instead (the approval gate), so the tool itself
    // does not refuse.
    let output = WriteFile::with_settings(&settings)
        .run(json!({"path": outside, "content": "changed"}))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "changed");
}

#[tokio::test]
async fn webhook_runs_do_not_read_saved_sessions() {
    let layout = Layout::new();
    let home = layout.path("home");
    let session = home.join(".junees/sessions/s.jsonl");
    std::fs::create_dir_all(session.parent().unwrap()).unwrap();
    std::fs::write(&session, "{\"tool_output\":\"earlier secret\"}\n").unwrap();
    // `sandbox_read` holds the home folder, so only the sessions rule
    // stands between the run and the file.
    let mut settings = tools(&home);
    if let Some(sandbox) = settings.sandbox.as_mut() {
        sandbox.read = Some(vec![home.clone()]);
    }
    let gh = tempfile::tempdir().unwrap();
    let webhook = crate::cron::run::Trigger::Webhook {
        route: "r",
        github_allow: &[],
        gh_config: gh.path(),
        context_dir: &home,
    }
    .tool_settings(&settings);
    let output = ReadFile::with_settings(&webhook)
        .run(json!({"path": session}))
        .await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        !output.content.contains("earlier secret"),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("saved sessions"),
        "{}",
        output.content
    );
    // A scheduled job (the user's own prompt) still may.
    let cron = crate::cron::run::Trigger::Cron.tool_settings(&settings);
    let output = ReadFile::with_settings(&cron)
        .run(json!({"path": session}))
        .await;
    assert!(
        output.content.contains("earlier secret"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn unattended_runs_in_the_home_folder_leave_startup_files_alone() {
    let layout = Layout::new();
    let access = layout.scope("home", &[]);
    for path in [
        "home/.bashrc",
        "home/.profile",
        "home/.zshrc",
        "home/.gitconfig",
        "home/.config/git/config",
        "home/.config/systemd/user/evil.service",
        "home/.config/autostart/evil.desktop",
    ] {
        let output = write(&access, &layout.path(path), "planted\n").await;
        assert!(output.is_error, "{path}: {}", output.content);
        assert!(
            output.content.contains("shell startup file"),
            "{path}: {}",
            output.content
        );
    }
    let output = edit(&access, &layout.path("home/.bashrc"), "# rc", "planted").await;
    assert!(output.is_error, "{}", output.content);
    assert_eq!(
        std::fs::read_to_string(layout.path("home/.bashrc")).unwrap(),
        "# rc\n"
    );
    assert!(!layout.path("home/.config/systemd").exists());
    // Reading them and writing other files there is fine.
    let output = read(&access, &layout.path("home/.bashrc")).await;
    assert!(output.content.contains("# rc"), "{}", output.content);
    let output = write(&access, &layout.path("home/notes.md"), "ok\n").await;
    assert!(!output.is_error, "{}", output.content);
}

#[test]
fn unattended_runs_in_the_home_or_state_folder_are_warned_about() {
    let layout = Layout::new();
    let home = layout.path("home");
    let state = home.join(".junees");
    let warn = |dir: &Path| super::unattended_folder_warning(dir, Some(&home), Some(&state));
    let warning = warn(&home).unwrap();
    assert!(warning.contains("the home folder"), "{warning}");
    let warning = warn(&layout.root).unwrap();
    assert!(warning.contains("state folder"), "{warning}");
    assert_eq!(warn(&layout.path("work")), None);
    assert_eq!(warn(&layout.path("home/.ssh")), None);
}

#[tokio::test]
async fn writes_refuse_read_only_files_like_a_direct_write() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.txt");
    std::fs::write(&path, "kept\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let output = write(&FileAccess::default(), &path, "changed\n").await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output.content.contains("Permission denied"),
        "{}",
        output.content
    );
    let output = edit(&FileAccess::default(), &path, "kept", "changed").await;
    assert!(output.is_error, "{}", output.content);
    assert!(
        output.content.contains("Permission denied"),
        "{}",
        output.content
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept\n");
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["ro.txt"]);
}

#[tokio::test]
async fn writes_refuse_paths_that_end_in_a_dot() {
    let layout = Layout::new();
    for access in [FileAccess::default(), layout.scope("work", &[])] {
        let output = write(&access, &layout.path("work/newdir/."), "x").await;
        assert!(output.is_error, "{}", output.content);
        assert!(!layout.path("work/newdir").exists());
        let output = write(&access, &layout.path("work/a.txt/."), "x").await;
        assert!(output.is_error, "{}", output.content);
        let output = edit(&access, &layout.path("work/a.txt/."), "work", "x").await;
        assert!(output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(layout.path("work/a.txt")).unwrap(),
            "work text\n"
        );
    }
}

#[tokio::test]
async fn write_file_keeps_hard_links_together_outside_unattended_runs() {
    let layout = Layout::new();
    let (first, second) = (layout.path("work/h1"), layout.path("work/h2"));
    std::fs::write(&first, "old\n").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let output = write(&FileAccess::default(), &first, "new\n").await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "new\n");
    // An unattended run breaks the link: the other name may lie outside
    // its scope.
    let output = write(&layout.scope("work", &[]), &first, "newer\n").await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "newer\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "new\n");
}

#[tokio::test]
async fn writes_keep_the_group_when_the_owner_may_give_it() {
    use std::os::unix::fs::MetadataExt;
    // A supplementary group of this process other than its own.
    let mut groups = vec![0 as libc::gid_t; 256];
    // SAFETY: a buffer of `groups.len()` entries.
    let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
    // SAFETY: getegid has no preconditions.
    let own = unsafe { libc::getegid() };
    let Some(&group) = groups[..count.max(0) as usize]
        .iter()
        .find(|&&group| group != own)
    else {
        eprintln!("skipped: no supplementary group");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.txt");
    std::fs::write(&path, "old\n").unwrap();
    std::os::unix::fs::chown(&path, None, Some(group)).unwrap();
    let output = write(&FileAccess::default(), &path, "new\n").await;
    assert!(!output.is_error, "{}", output.content);
    let output = edit(&FileAccess::default(), &path, "new", "newer").await;
    assert!(!output.is_error, "{}", output.content);
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.gid(), group);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer\n");
}

#[tokio::test]
async fn no_file_tool_opens_proc_sys_or_dev() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::os::unix::fs::symlink("/proc/self/environ", root.join("env")).unwrap();
    std::os::unix::fs::symlink("/proc/self", root.join("me")).unwrap();
    let access = FileAccess::default();
    for path in [
        PathBuf::from("/proc/self/environ"),
        root.join("env"),
        root.join("me/environ"),
        root.join("me/../self/environ"),
        PathBuf::from("/sys/kernel/hostname"),
        PathBuf::from("/dev/null"),
    ] {
        for output in [
            read(&access, &path).await,
            write(&access, &path, "x").await,
            edit(&access, &path, "PATH=", "x").await,
        ] {
            assert!(output.is_error, "{}: {}", path.display(), output.content);
            assert!(
                output.content.contains("process, kernel and device files"),
                "{}: {}",
                path.display(),
                output.content
            );
            assert!(!output.content.contains("PATH="), "{}", output.content);
        }
    }
    // A folder to create under a link into /proc is refused too.
    let output = write(&access, &root.join("me/new/x"), "x").await;
    assert!(output.is_error, "{}", output.content);
}

#[tokio::test]
async fn edit_file_refuses_files_with_nul_bytes_without_hints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("environ");
    std::fs::write(&path, b"ANTHROPIC_API_KEY=sk-ant-secret\0HOME=/home/x\0").unwrap();
    let output = edit(
        &FileAccess::default(),
        &path,
        "ANTHROPIC_API_KEY=sk-ant-secreX",
        "x",
    )
    .await;
    assert!(output.is_error);
    assert!(output.content.contains("binary file"), "{}", output.content);
    assert!(!output.content.contains("sk-ant"), "{}", output.content);
}

/// Swaps the link `link` between `a` and `b` until `stop` is set.
fn flip(
    link: PathBuf,
    a: PathBuf,
    b: PathBuf,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let next = link.with_extension("next");
        while !stop.load(Ordering::Relaxed) {
            for target in [&a, &b] {
                let _ = std::fs::remove_file(&next);
                std::os::unix::fs::symlink(target, &next).unwrap();
                std::fs::rename(&next, &link).unwrap();
            }
        }
    })
}

#[tokio::test]
async fn a_swapped_link_cannot_lead_past_the_checks() {
    // The settings file, for any run.
    let (_dir, root, file) = crate::tools::settings_file::testing::home();
    std::fs::write(root.join("ok.txt"), "fine\n").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flipper = flip(
        root.join("link"),
        root.join("ok.txt"),
        root.join(".junees/config.toml"),
        stop.clone(),
    );
    let tool = ReadFile::with_settings(&ToolSettings {
        settings_file: Some(file),
        ..ToolSettings::default()
    });
    for _ in 0..300 {
        let output = tool.run(json!({"path": root.join("link")})).await;
        assert!(!output.content.contains("secret-key"), "{}", output.content);
    }

    // A folder outside an unattended run.
    let layout = Layout::new();
    let access = layout.scope("work", &[]);
    std::fs::write(layout.path("work/ok.txt"), "fine\n").unwrap();
    let second = flip(
        layout.path("work/link"),
        layout.path("work/ok.txt"),
        layout.path("outside/notes.txt"),
        stop.clone(),
    );
    let mut written = 0;
    for _ in 0..300 {
        let output = write(&access, &layout.path("work/link"), "planted\n").await;
        written += usize::from(!output.is_error);
        let output = read(&access, &layout.path("work/link")).await;
        assert!(
            !output.content.contains("outside text"),
            "{}",
            output.content
        );
    }
    stop.store(true, Ordering::Relaxed);
    flipper.join().unwrap();
    second.join().unwrap();
    assert!(written > 0, "no write went through the link to ok.txt");
    assert_eq!(
        std::fs::read_to_string(layout.path("outside/notes.txt")).unwrap(),
        "outside text\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".junees/config.toml")).unwrap(),
        "api_key = \"secret-key\"\n"
    );
}

#[tokio::test]
async fn write_file_rewrites_in_place_when_the_folder_is_read_only() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes anywhere
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("f.txt"), "old\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let access = FileAccess::default();
    let replaced = write(&access, &locked.join("f.txt"), "new\n").await;
    let created = write(&access, &locked.join("g.txt"), "new\n").await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!replaced.is_error, "{}", replaced.content);
    assert_eq!(
        std::fs::read_to_string(locked.join("f.txt")).unwrap(),
        "new\n"
    );
    assert!(created.is_error, "{}", created.content);
    assert!(!locked.join("g.txt").exists());
}

#[tokio::test]
async fn writes_keep_permissions_and_links_and_leave_no_temporary_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.sh");
    std::fs::write(&path, "old\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
    let output = write(&FileAccess::default(), &path, "new\n").await;
    assert!(!output.is_error, "{}", output.content);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o751);
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["run.sh"]);
    // Writing through a link replaces its target and keeps the link.
    let link = dir.path().join("link.sh");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let output = write(&FileAccess::default(), &link, "newer\n").await;
    assert!(!output.is_error, "{}", output.content);
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer\n");
}

#[test]
fn temporary_names_are_recognisable_and_short_enough() {
    let name = |name: &str| {
        temp_name(std::ffi::OsStr::new(name))
            .to_str()
            .unwrap()
            .to_string()
    };
    let short = name("notes.md");
    assert!(short.starts_with(".notes.md.junees-edit-"), "{short}");
    assert!(short.ends_with(".tmp"), "{short}");
    for long in ["a".repeat(255), "한".repeat(85)] {
        let temp = name(&long);
        assert!(temp.len() <= 255, "{}", temp.len());
        assert!(temp.len() > 250, "{}", temp.len());
        assert!(temp.contains(".junees-edit-"), "{temp}");
    }
}
