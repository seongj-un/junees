//! The sandbox with the real kernel's Landlock where it has one (tests
//! that need it say why they skip otherwise), and with a fake kernel
//! answer for the `auto`/`require` behavior. Every command runs through
//! the test sandbox of the bash tool ([`Bash::for_test`]) inside a
//! temporary folder.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::*;
use crate::tools::{Bash, SettingsFile, Tool, ToolOutput, ToolSettings};

/// Whether this kernel has Landlock; prints why a test skips if not.
fn landlock() -> bool {
    match Support::current() {
        Support::Abi(_) => true,
        Support::Unavailable(why) => {
            eprintln!("skipped: {why}");
            false
        }
    }
}

/// Settings with no temporary folders (the test folders are in one) and
/// the mode `require`, so a broken setup fails instead of running
/// unconfined.
fn settings() -> Settings {
    Settings {
        mode: Some(Mode::Require),
        write: Vec::new(),
        read: None,
        webhook_read: Vec::new(),
        network: true,
        temp: Vec::new(),
        unattended: false,
    }
}

/// A test folder: `work` (the working folder), `outside` with a file
/// `keep`, and a fake home with a Junees state folder.
struct Layout {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Layout {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let junees = root.join("home/.junees");
        for folder in ["work", "outside", "home/sub"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        for folder in ["secrets", "memory", "sessions"] {
            std::fs::create_dir_all(junees.join(folder)).unwrap();
        }
        std::fs::write(root.join("outside/keep"), "keep\n").unwrap();
        std::fs::write(root.join("home/notes.txt"), "notes\n").unwrap();
        std::fs::write(junees.join("config.toml"), "api_key = \"fake-key\"\n").unwrap();
        std::fs::write(junees.join("mcp.json"), "{}\n").unwrap();
        std::fs::write(junees.join("secrets/token"), "fake-secret\n").unwrap();
        std::fs::write(junees.join("memory/note.md"), "remembered\n").unwrap();
        std::fs::write(junees.join("sessions/s.jsonl"), "{}\n").unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, rest: &str) -> PathBuf {
        self.root.join(rest)
    }

    /// The settings file of the fake state folder.
    fn settings_file(&self) -> SettingsFile {
        SettingsFile::new(
            self.path("home/.junees/config.toml"),
            Some(self.path("home")),
        )
    }

    /// The sandbox the bash tool gets from these tool settings.
    fn sandbox(&self, settings: Settings) -> Sandbox {
        Sandbox::for_tools(&ToolSettings {
            settings_file: Some(self.settings_file()),
            sandbox: Some(settings),
            ..ToolSettings::default()
        })
        .unwrap()
    }
}

/// Runs `command` in `work` under `sandbox`.
async fn run(work: &Path, sandbox: Sandbox, command: &str) -> ToolOutput {
    Bash::for_test(work)
        .confined(Some(sandbox))
        .run(json!({ "command": command }))
        .await
}

#[tokio::test]
async fn writes_only_in_the_working_folder_and_the_configured_ones() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let work = layout.path("work");
    let extra = layout.path("home/sub");
    let outside = layout.path("outside");
    let command = format!(
        "echo a > {work}/made && mkdir {work}/dir && echo b > {extra}/made && echo inside-ok; \
         echo c > {outside}/new || echo new-refused; \
         echo d >> {outside}/keep || echo append-refused; \
         mkdir {outside}/dir || echo mkdir-refused; \
         mv {outside}/keep {work}/moved || echo move-refused; \
         cat {outside}/keep",
        work = work.display(),
        extra = extra.display(),
        outside = outside.display(),
    );
    let sandbox = layout.sandbox(Settings {
        write: vec![extra.clone()],
        ..settings()
    });
    let output = run(&work, sandbox, &command).await;
    for line in [
        "inside-ok",
        "new-refused",
        "append-refused",
        "mkdir-refused",
        "move-refused",
    ] {
        assert!(output.content.contains(line), "{line}: {}", output.content);
    }
    assert!(work.join("made").exists() && work.join("dir").is_dir());
    assert!(extra.join("made").exists());
    assert!(!outside.join("new").exists());
    assert!(!outside.join("dir").exists());
    assert!(!work.join("moved").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("keep")).unwrap(),
        "keep\n"
    );
}

#[tokio::test]
async fn the_settings_file_mcp_list_and_secrets_are_unreadable() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let work = layout.path("work");
    let junees = layout.path("home/.junees");
    let command = format!(
        "cat {j}/config.toml || echo config-refused; \
         cat {j}/mcp.json || echo mcp-refused; \
         cat {j}/secrets/token || echo secret-refused; \
         cat /proc/self/root{j}/config.toml || echo proc-root-refused; \
         ln {j}/secrets/token {w}/linked || echo link-refused; \
         cat {h}/notes.txt {j}/memory/note.md {j}/sessions/s.jsonl",
        j = junees.display(),
        w = work.display(),
        h = layout.path("home").display(),
    );
    let output = run(&work, layout.sandbox(settings()), &command).await;
    let text = &output.content;
    assert!(!text.contains("fake-key"), "{text}");
    assert!(!text.contains("fake-secret"), "{text}");
    for line in [
        "config-refused",
        "mcp-refused",
        "secret-refused",
        "proc-root-refused",
        "link-refused",
        "notes",
        "remembered",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert!(!work.join("linked").exists());
}

#[tokio::test]
async fn the_browser_profiles_are_out_of_reach() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let work = layout.path("work");
    // A running browser's profile, where `browser::process::Profile`
    // puts it.
    let profiles = layout
        .path("home/.junees")
        .join(crate::browser::PROFILES_FOLDER);
    let profile = profiles.join("0123456789abcdef");
    std::fs::create_dir_all(profile.join("Default")).unwrap();
    std::fs::write(profile.join("Default/Cookies"), "fake-cookie\n").unwrap();
    // Listing stays allowed (folder listing is not a right the sandbox
    // handles); the files' contents are what matter.
    let command = format!(
        "cat {p}/Default/Cookies || echo read-refused; \
         echo x >> {p}/Default/Cookies || echo append-refused; \
         echo x > {p}/Default/planted || echo write-refused; \
         mkdir {b}/other || echo mkdir-refused; \
         mv {p} {w}/moved || echo move-refused; \
         cp -r {b} {w}/copy || echo copy-refused",
        p = profile.display(),
        b = profiles.display(),
        w = work.display(),
    );
    let output = run(&work, layout.sandbox(settings()), &command).await;
    let text = &output.content;
    assert!(!text.contains("fake-cookie"), "{text}");
    for line in [
        "read-refused",
        "append-refused",
        "write-refused",
        "mkdir-refused",
        "move-refused",
        "copy-refused",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert_eq!(
        std::fs::read_to_string(profile.join("Default/Cookies")).unwrap(),
        "fake-cookie\n"
    );
    assert!(!profile.join("Default/planted").exists());
    assert!(!profiles.join("other").exists());
    assert!(!work.join("moved").exists());
    assert!(!work.join("copy/0123456789abcdef/Default/Cookies").exists());
}

#[tokio::test]
async fn a_working_folder_holding_the_state_folder_keeps_it_protected() {
    // Junees started in the home folder: everything there is writable
    // except the state folder's protected and data parts.
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let home = layout.path("home");
    let junees = layout.path("home/.junees");
    let command = format!(
        "echo a > {h}/sub/made && echo sub-ok; \
         cat {j}/config.toml || echo config-refused; \
         echo x >> {j}/config.toml || echo config-write-refused; \
         echo x > {j}/memory/new.md || echo memory-refused; \
         mv {j}/secrets/token {h}/sub/token || echo move-refused; \
         echo x > {j}/sessions/new.jsonl || echo sessions-refused",
        h = home.display(),
        j = junees.display(),
    );
    let output = run(&home, layout.sandbox(settings()), &command).await;
    let text = &output.content;
    for line in [
        "the working folder holds Junees's state folder",
        "sub-ok",
        "config-refused",
        "config-write-refused",
        "memory-refused",
        "move-refused",
        "sessions-refused",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert!(!text.contains("fake-key"), "{text}");
    // New entries right in the working folder are out of reach there.
    let top = run(
        &home,
        layout.sandbox(settings()),
        &format!("echo a > {}/top || echo top-refused", home.display()),
    )
    .await;
    assert!(top.content.contains("top-refused"), "{}", top.content);
    assert!(!home.join("top").exists());
    assert!(!junees.join("memory/new.md").exists());
    assert!(junees.join("secrets/token").exists());
    assert_eq!(
        std::fs::read_to_string(junees.join("config.toml")).unwrap(),
        "api_key = \"fake-key\"\n"
    );
}

#[tokio::test]
async fn a_linked_state_folder_keeps_its_link_when_the_working_folder_holds_it() {
    // `~/.junees` a link to the state kept elsewhere (a dotfile manager, a
    // volume): Junees reads its settings through the link, so a command
    // in the home folder must not swap the link for a folder of its own.
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let home = layout.path("home");
    std::fs::rename(home.join(".junees"), layout.path("state")).unwrap();
    std::os::unix::fs::symlink("../state", home.join(".junees")).unwrap();
    let command = "mv .junees moved || echo move-refused; \
         ln -sfn ../outside .junees || echo relink-refused; \
         mkdir fresh || echo mkdir-refused; \
         echo x >> .junees/config.toml || echo config-write-refused; \
         cat .junees/config.toml || echo config-refused; \
         echo x > .junees/memory/new.md || echo memory-refused; \
         echo a > sub/made && echo sub-ok";
    let output = run(&home, layout.sandbox(settings()), command).await;
    let text = &output.content;
    for line in [
        "the working folder holds Junees's state folder",
        "move-refused",
        "relink-refused",
        "mkdir-refused",
        "config-write-refused",
        "config-refused",
        "memory-refused",
        "sub-ok",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert!(!text.contains("fake-key"), "{text}");
    assert_eq!(
        std::fs::read_link(home.join(".junees")).unwrap(),
        Path::new("../state")
    );
    assert!(!home.join("moved").exists() && !home.join("fresh").exists());
    assert!(!layout.path("state/memory/new.md").exists());
    assert_eq!(
        std::fs::read_to_string(layout.path("state/config.toml")).unwrap(),
        "api_key = \"fake-key\"\n"
    );
}

#[tokio::test]
async fn a_folder_that_cannot_be_listed_on_the_way_leaves_commands_running() {
    // `/home` at 0711, or a home that cannot be listed: the walk to the
    // protected files cannot see the folder's other entries. They get no
    // rule; commands still run, and the protection holds.
    if !landlock() {
        return;
    }
    // SAFETY: geteuid only reads the process's user id.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root lists any folder");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let layout = Layout::new();
    let home = layout.path("home");
    let junees = layout.path("home/.junees");
    let command = format!(
        "echo ok > made && echo work-ok; \
         cat {h}/notes.txt || echo notes-refused; \
         cat {j}/config.toml || echo config-refused; \
         cat {j}/memory/note.md",
        h = home.display(),
        j = junees.display(),
    );
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o311)).unwrap();
    let output = run(&layout.path("work"), layout.sandbox(settings()), &command).await;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    let text = &output.content;
    assert!(!output.is_error, "{text}");
    for line in ["work-ok", "notes-refused", "config-refused", "remembered"] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert!(!text.contains("fake-key"), "{text}");
    assert!(layout.path("work/made").exists());
}

#[tokio::test]
async fn a_webhook_run_cannot_read_git_and_github_credentials() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let hosts = layout.path("home/.config/gh/hosts.yml");
    std::fs::create_dir_all(hosts.parent().unwrap()).unwrap();
    std::fs::write(&hosts, "oauth_token: fake-gh-token\n").unwrap();
    let command = format!("cat {} || echo refused", hosts.display());
    webhook_hides(&layout, &command, "fake-gh-token").await;
}

#[tokio::test]
async fn a_webhook_run_cannot_read_claude_code_or_saved_sessions() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let login = layout.path("home/.claude/.credentials.json");
    std::fs::create_dir_all(login.parent().unwrap()).unwrap();
    std::fs::write(&login, "{\"token\": \"fake-claude-login\"}\n").unwrap();
    let command = format!("cat {} || echo refused", login.display());
    webhook_hides(&layout, &command, "fake-claude-login").await;
    let session = layout.path("home/.junees/sessions/s.jsonl");
    std::fs::write(&session, "{\"secret\": \"fake-session-output\"}\n").unwrap();
    let command = format!("cat {} || echo refused", session.display());
    webhook_hides(&layout, &command, "fake-session-output").await;
}

/// Runs `command`, which prints `secret` or `refused`, outside and inside
/// a webhook run: only the webhook run is refused.
async fn webhook_hides(layout: &Layout, command: &str, secret: &str) {
    let tools = |webhook| ToolSettings {
        settings_file: Some(layout.settings_file()),
        sandbox: Some(settings()),
        webhook,
        ..ToolSettings::default()
    };
    let work = layout.path("work");
    let plain = run(&work, Sandbox::for_tools(&tools(false)).unwrap(), command).await;
    assert!(plain.content.contains(secret), "{}", plain.content);
    let webhook = run(&work, Sandbox::for_tools(&tools(true)).unwrap(), command).await;
    assert!(!webhook.content.contains(secret), "{}", webhook.content);
    assert!(webhook.content.contains("refused"), "{}", webhook.content);
}

#[tokio::test]
async fn unattended_runs_cannot_read_home_credentials() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let home = layout.path("home");
    let files = [
        (".ssh/id_ed25519", "ssh-private"),
        (".ssh/deploy_key", "other-private"),
        (".ssh/id_ed25519.pub", "ssh-public"),
        (".ssh/known_hosts", "known-host"),
        (".aws/credentials", "aws-secret"),
    ];
    for (path, text) in files {
        std::fs::create_dir_all(home.join(path).parent().unwrap()).unwrap();
        std::fs::write(home.join(path), format!("{text}\n")).unwrap();
    }
    // A credential folder that is a link: its target is hidden too.
    std::fs::create_dir_all(layout.path("outside/kube")).unwrap();
    std::fs::write(layout.path("outside/kube/config"), "kube-token\n").unwrap();
    std::os::unix::fs::symlink(layout.path("outside/kube"), home.join(".kube")).unwrap();
    let mut command = String::new();
    for path in files.iter().map(|(path, _)| home.join(path)).chain([
        layout.path("outside/kube/config"),
        home.join(".kube/config"),
    ]) {
        command.push_str(&format!("cat {} 2>/dev/null; ", path.display()));
    }
    let tools = |webhook, unattended, read: Option<Vec<PathBuf>>| ToolSettings {
        settings_file: Some(layout.settings_file()),
        sandbox: Some(Settings {
            unattended,
            // The webhook runs' own list names the keys too.
            webhook_read: read.clone().unwrap_or_default(),
            read,
            ..settings()
        }),
        webhook,
        ..ToolSettings::default()
    };
    let work = layout.path("work");
    let shows = |output: &ToolOutput, texts: &[&str]| {
        for text in texts {
            assert!(output.content.contains(text), "{text}: {}", output.content);
        }
    };
    let hides = |output: &ToolOutput, texts: &[&str]| {
        for text in texts {
            assert!(!output.content.contains(text), "{text}: {}", output.content);
        }
    };
    let all = [
        "ssh-private",
        "other-private",
        "ssh-public",
        "known-host",
        "aws-secret",
        "kube-token",
    ];
    let attended = run(
        &work,
        Sandbox::for_tools(&tools(false, false, None)).unwrap(),
        &command,
    )
    .await;
    shows(&attended, &all);
    // A scheduled job keeps ssh's public files.
    let cron = run(
        &work,
        Sandbox::for_tools(&tools(false, true, None)).unwrap(),
        &command,
    )
    .await;
    shows(&cron, &["ssh-public", "known-host"]);
    hides(
        &cron,
        &["ssh-private", "other-private", "aws-secret", "kube-token"],
    );
    // Naming ~/.ssh in `sandbox_read` gives a job its keys back, and only
    // those; naming `/` gives nothing back.
    let read = Some(vec![PathBuf::from("/"), home.join(".ssh")]);
    let keys = run(
        &work,
        Sandbox::for_tools(&tools(false, true, read.clone())).unwrap(),
        &command,
    )
    .await;
    shows(&keys, &["ssh-private", "other-private", "ssh-public"]);
    hides(&keys, &["aws-secret", "kube-token"]);
    // A webhook run reads none of them, whatever `sandbox_read` and
    // `webhook_sandbox_read` say.
    let webhook = run(
        &work,
        Sandbox::for_tools(&tools(true, true, read)).unwrap(),
        &command,
    )
    .await;
    hides(&webhook, &all);
}

#[tokio::test]
async fn unattended_runs_in_the_home_folder_cannot_change_what_runs_later() {
    // A scheduled job or webhook run working in the home folder: the
    // files the next shell, login, ssh or git runs stay as they are, and
    // ordinary files there stay writable. Without `~/.local` at all, the
    // command cannot make it (and so not `~/.local/bin` either).
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let home = layout.path("home");
    for (path, text) in [
        (".bashrc", "# rc"),
        (".profile", "# profile"),
        (".gitconfig", "[user]"),
        (".ssh/authorized_keys", "ssh-ed25519 mine"),
        (".ssh/known_hosts", "host"),
        (".config/other/settings", "other"),
    ] {
        std::fs::create_dir_all(home.join(path).parent().unwrap()).unwrap();
        std::fs::write(home.join(path), format!("{text}\n")).unwrap();
    }
    let refused = [
        ("echo x >> .bashrc", "bashrc"),
        ("echo x > .profile", "profile"),
        ("mv .bashrc moved", "move"),
        ("echo x > .zshrc", "zshrc"),
        ("echo x >> .gitconfig", "gitconfig"),
        ("mkdir -p .config/git", "config-git"),
        ("echo k >> .ssh/authorized_keys", "authorized-keys"),
        ("echo k > .ssh/authorized_keys2", "authorized-keys2"),
        (
            "mkdir -p .config/systemd/user && echo u > .config/systemd/user/evil.service",
            "systemd",
        ),
        ("mkdir -p .config/autostart", "autostart"),
        ("mkdir -p .local/share/systemd/user", "local-systemd"),
        ("mkdir -p .local/bin && echo x > .local/bin/ls", "local-bin"),
        ("mkdir bin", "bin"),
    ];
    let mut command = String::from(
        "echo a > sub/made && echo sub-ok; \
         echo a >> notes.txt && echo notes-ok; \
         echo a > .config/other/settings && echo other-ok; \
         echo a > .config/other/new && echo other-new-ok; ",
    );
    for (attempt, name) in refused {
        command.push_str(&format!(
            "{{ {attempt}; }} 2>/dev/null || echo {name}-refused; "
        ));
    }
    // The state folder elsewhere, so only these paths keep the home
    // folder from getting one rule as a whole.
    let tools = |unattended| ToolSettings {
        settings_file: Some(SettingsFile::new(
            layout.path("outside/.junees/config.toml"),
            Some(home.clone()),
        )),
        sandbox: Some(Settings {
            unattended,
            ..settings()
        }),
        ..ToolSettings::default()
    };
    let cron = run(&home, Sandbox::for_tools(&tools(true)).unwrap(), &command).await;
    let text = &cron.content;
    for line in ["sub-ok", "notes-ok", "other-ok", "other-new-ok"] {
        assert!(text.contains(line), "{line}: {text}");
    }
    for (_, name) in refused {
        assert!(text.contains(&format!("{name}-refused")), "{name}: {text}");
    }
    for (path, text) in [
        (".bashrc", "# rc\n"),
        (".profile", "# profile\n"),
        (".gitconfig", "[user]\n"),
        (".ssh/authorized_keys", "ssh-ed25519 mine\n"),
    ] {
        assert_eq!(std::fs::read_to_string(home.join(path)).unwrap(), text);
    }
    for path in [
        "moved",
        ".zshrc",
        ".config/git",
        ".config/systemd",
        ".config/autostart",
        ".ssh/authorized_keys2",
        ".local",
        "bin",
    ] {
        assert!(!home.join(path).exists(), "{path}");
    }
    assert_eq!(
        std::fs::read_to_string(home.join("sub/made")).unwrap(),
        "a\n"
    );
    // A folder on PATH inside the home folder (the daemon's PATH).
    std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
    let path = std::env::join_paths([home.join(".cargo/bin"), PathBuf::from("/usr/bin")]).unwrap();
    let sandbox = Sandbox::new(
        Settings {
            unattended: true,
            ..settings()
        },
        Vec::new(),
        unattended_no_write(&tools(true), Some(&path)),
    );
    let on_path = run(
        &home,
        sandbox,
        "echo x > .cargo/bin/git || echo path-refused; echo a > sub/also && echo also-ok",
    )
    .await;
    for line in ["path-refused", "also-ok"] {
        assert!(
            on_path.content.contains(line),
            "{line}: {}",
            on_path.content
        );
    }
    assert!(!home.join(".cargo/bin/git").exists());
    // Interactive runs are as before: the same command changes them all.
    let attended = run(&home, Sandbox::for_tools(&tools(false)).unwrap(), &command).await;
    for (_, name) in refused {
        assert!(
            !attended.content.contains(&format!("{name}-refused")),
            "{name}: {}",
            attended.content
        );
    }
    for path in [
        "moved",
        ".zshrc",
        ".config/systemd/user/evil.service",
        ".ssh/authorized_keys2",
        ".local/bin/ls",
        "bin",
    ] {
        assert!(home.join(path).exists(), "{path}");
    }
}

#[tokio::test]
async fn unattended_jobs_in_the_home_folder_cannot_change_ssh_or_cargo_settings() {
    // What ssh runs (`ProxyCommand`, `Match exec`, an `Include`d file, a
    // script in ~/.ssh, `rc` for sshd) and cargo's settings stay as they
    // are, even through a link to a dotfiles folder; ssh may still add a
    // host to `known_hosts`, the job still reads `config`, and cargo's
    // caches stay writable.
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let home = layout.path("home");
    for (path, text) in [
        (".ssh/known_hosts", "host"),
        (".ssh/id_ed25519", "PRIVATE KEY"),
        (".ssh/proxy.sh", "#!/bin/sh"),
        (".ssh/config.d/extra", "Host extra"),
        ("dotfiles/ssh_config", "Host *"),
        (".cargo/config.toml", "[build]"),
        (".cargo/registry/cache/crate", "crate"),
    ] {
        std::fs::create_dir_all(home.join(path).parent().unwrap()).unwrap();
        std::fs::write(home.join(path), format!("{text}\n")).unwrap();
    }
    std::os::unix::fs::symlink(home.join("dotfiles/ssh_config"), home.join(".ssh/config")).unwrap();
    let refused = [
        ("echo 'ProxyCommand sh -c evil' >> .ssh/config", "config"),
        ("echo x >> dotfiles/ssh_config", "config-target"),
        ("echo evil > .ssh/rc", "rc"),
        ("echo A=b > .ssh/environment", "environment"),
        ("echo evil >> .ssh/proxy.sh", "proxy"),
        ("echo 'Match exec evil' >> .ssh/config.d/extra", "include"),
        ("echo x > .ssh/new", "new"),
        ("echo k > .ssh/authorized_keys", "authorized-keys"),
        ("echo x >> .cargo/config.toml", "cargo-config"),
        ("echo x > .cargo/config", "cargo-config-old"),
    ];
    let mut command = String::from(
        "echo other.host ssh-ed25519 k >> .ssh/known_hosts && echo known-ok; \
         grep -q 'Host [*]' .ssh/config && echo read-ok; \
         echo b > .cargo/registry/cache/crate && echo cargo-cache-ok; ",
    );
    for (attempt, name) in refused {
        command.push_str(&format!(
            "{{ {attempt}; }} 2>/dev/null || echo {name}-refused; "
        ));
    }
    let tools = |unattended| ToolSettings {
        settings_file: Some(SettingsFile::new(
            layout.path("outside/.junees/config.toml"),
            Some(home.clone()),
        )),
        sandbox: Some(Settings {
            unattended,
            ..settings()
        }),
        ..ToolSettings::default()
    };
    let cron = run(&home, Sandbox::for_tools(&tools(true)).unwrap(), &command).await;
    let text = &cron.content;
    for line in ["known-ok", "read-ok", "cargo-cache-ok"] {
        assert!(text.contains(line), "{line}: {text}");
    }
    for (_, name) in refused {
        assert!(text.contains(&format!("{name}-refused")), "{name}: {text}");
    }
    for (path, text) in [
        ("dotfiles/ssh_config", "Host *\n"),
        (".ssh/proxy.sh", "#!/bin/sh\n"),
        (".ssh/config.d/extra", "Host extra\n"),
        (".cargo/config.toml", "[build]\n"),
    ] {
        assert_eq!(std::fs::read_to_string(home.join(path)).unwrap(), text);
    }
    for path in [".ssh/rc", ".ssh/environment", ".ssh/new", ".cargo/config"] {
        assert!(!home.join(path).exists(), "{path}");
    }
    assert!(
        std::fs::read_to_string(home.join(".ssh/known_hosts"))
            .unwrap()
            .contains("other.host")
    );
    // Interactive runs are as before.
    let attended = run(&home, Sandbox::for_tools(&tools(false)).unwrap(), &command).await;
    for (_, name) in refused {
        assert!(
            !attended.content.contains(&format!("{name}-refused")),
            "{name}: {}",
            attended.content
        );
    }
}

#[tokio::test]
async fn reads_can_be_narrowed() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let work = layout.path("work");
    std::fs::write(work.join("own"), "own-file\n").unwrap();
    // The programs' folders, so bash still starts.
    let system = ["/usr", "/bin", "/lib", "/lib64", "/etc"]
        .iter()
        .map(PathBuf::from)
        .collect();
    let command = format!(
        "cat {w}/own; cat {h}/notes.txt || echo notes-refused",
        w = work.display(),
        h = layout.path("home").display()
    );
    let output = run(
        &work,
        layout.sandbox(Settings {
            read: Some(system),
            ..settings()
        }),
        &command,
    )
    .await;
    assert!(output.content.contains("own-file"), "{}", output.content);
    assert!(
        output.content.contains("notes-refused"),
        "{}",
        output.content
    );
}

#[tokio::test]
async fn a_missing_state_folder_is_made_private_and_protected() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let home = layout.path("sub-home");
    std::fs::create_dir_all(home.join("sub")).unwrap();
    // The settings file would be in sub-home/.junees, which does not exist
    // yet: a command run in the home folder must not make it with an
    // mcp.json of its own for the next run.
    let sandbox = Sandbox::for_tools(&ToolSettings {
        settings_file: Some(SettingsFile::new(
            home.join(".junees/config.toml"),
            Some(home.clone()),
        )),
        sandbox: Some(settings()),
        ..ToolSettings::default()
    })
    .unwrap();
    let command = "echo '{}' > .junees/mcp.json || echo mcp-refused; \
                   mkdir .junees/skills || echo mkdir-refused; \
                   mv .junees gone || echo move-refused; \
                   echo a > sub/made && echo sub-ok";
    let output = run(&home, sandbox, command).await;
    for line in ["mcp-refused", "mkdir-refused", "move-refused", "sub-ok"] {
        assert!(output.content.contains(line), "{line}: {}", output.content);
    }
    let state = home.join(".junees");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&state).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o700);
    assert!(!state.join("mcp.json").exists());
    assert!(home.join("sub/made").exists());
}

#[tokio::test]
async fn saved_sessions_are_read_only() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let sessions = layout.path("home/.junees/sessions");
    let command = format!(
        "echo forged >> {dir}/s.jsonl || echo append-refused; \
         echo x > {dir}/new.jsonl || echo create-refused; cat {dir}/s.jsonl",
        dir = sessions.display()
    );
    let output = run(&layout.path("work"), layout.sandbox(settings()), &command).await;
    for line in ["append-refused", "create-refused", "{}"] {
        assert!(output.content.contains(line), "{line}: {}", output.content);
    }
    assert_eq!(
        std::fs::read_to_string(sessions.join("s.jsonl")).unwrap(),
        "{}\n"
    );
}

#[tokio::test]
async fn unconfined_processes_environment_is_out_of_reach() {
    // Landlock's ptrace rule: a confined process cannot read the memory
    // or environment of a process outside its domain, such as this test
    // process (or Junees, or an MCP server).
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let work = layout.path("work");
    let command = format!(
        "tr '\\0' '\\n' < /proc/{}/environ > /dev/null && echo environ-read || \
         echo environ-refused",
        std::process::id()
    );
    let confined = run(&work, layout.sandbox(settings()), &command).await;
    assert!(
        confined.content.contains("environ-refused"),
        "{}",
        confined.content
    );
    let open = Bash::for_test(&work)
        .run(json!({ "command": command }))
        .await;
    assert!(open.content.contains("environ-read"), "{}", open.content);
}

#[test]
fn confined_commands_lose_the_session_bus_agent_and_tmux() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let sandbox = layout.sandbox(settings());
    let Prepared::Confined(confinement, _) = sandbox.prepare(&layout.path("work")) else {
        panic!("not confined");
    };
    let mut tokio_command = tokio::process::Command::new("true");
    let mut std_command = std::process::Command::new("true");
    for name in ESCAPE_ENV_VARS {
        tokio_command.env(name, "/run/user/1000/bus");
        std_command.env(name, "/run/user/1000/bus");
    }
    confinement.apply(&mut tokio_command);
    confinement.apply_std(&mut std_command);
    for envs in [tokio_command.as_std().get_envs(), std_command.get_envs()] {
        let envs: Vec<_> = envs.collect();
        for name in ESCAPE_ENV_VARS {
            assert!(
                envs.contains(&(std::ffi::OsStr::new(name), None)),
                "{name}: {envs:?}"
            );
        }
    }
}

#[tokio::test]
async fn confined_children_run_in_a_session_of_their_own() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let sandbox = layout.sandbox(settings());
    let Prepared::Confined(confinement, _) = sandbox.prepare(&layout.path("work")) else {
        panic!("not confined");
    };
    // Fields 5 to 7 of /proc/<pid>/stat: process group, session and
    // controlling terminal (0: none).
    let script = "read -r _ _ _ _ group session tty _ < /proc/$$/stat; \
                  echo \"$$ $group $session $tty\"";
    let mut tokio_command = tokio::process::Command::new("/bin/sh");
    let mut std_command = std::process::Command::new("/bin/sh");
    tokio_command.args(["-c", script]);
    std_command.args(["-c", script]);
    confinement.apply(&mut tokio_command);
    confinement.apply_std(&mut std_command);
    let lines = [
        tokio_command.output().await.unwrap().stdout,
        std_command.output().unwrap().stdout,
    ];
    for line in lines {
        let line = String::from_utf8(line).unwrap();
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "{line}");
        assert_eq!(fields[1], fields[0], "{line}");
        assert_eq!(fields[2], fields[0], "{line}");
        assert_eq!(fields[3], "0", "{line}");
    }
}

#[tokio::test]
async fn commands_run_with_no_new_privs() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let work = layout.path("work");
    let output = run(
        &work,
        layout.sandbox(settings()),
        "grep NoNewPrivs /proc/self/status",
    )
    .await;
    assert!(
        output.content.contains("NoNewPrivs:\t1"),
        "{}",
        output.content
    );
}

/// Whether bash can connect to a loopback listener (accepted on the
/// side) under a sandbox with `network`, the kernel answering `support`.
async fn connects(network: bool, support: Option<Support>) -> ToolOutput {
    let layout = Layout::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let command =
        format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected || echo connect-refused");
    let mut sandbox = layout.sandbox(Settings {
        network,
        mode: Some(Mode::Auto),
        ..settings()
    });
    if let Some(support) = support {
        sandbox = sandbox.with_support(support);
    }
    run(&layout.path("work"), sandbox, &command).await
}

#[tokio::test]
async fn tcp_is_refused_without_network() {
    match Support::current() {
        Support::Abi(abi) if abi >= NETWORK_ABI => {}
        other => {
            eprintln!("skipped: {}", other.describe());
            return;
        }
    }
    let refused = connects(false, None).await;
    assert!(
        refused.content.contains("connect-refused"),
        "{}",
        refused.content
    );
    let allowed = connects(true, None).await;
    assert!(allowed.content.contains("connected"), "{}", allowed.content);
    assert!(!allowed.content.contains("refused"), "{}", allowed.content);
}

#[tokio::test]
async fn an_older_abi_without_tcp_rules_says_so_or_refuses() {
    if !landlock() {
        return;
    }
    // ABI 3: files only. `auto` confines the files and says TCP is open.
    let output = connects(false, Some(Support::Abi(3))).await;
    assert!(output.content.contains("connected"), "{}", output.content);
    assert!(
        output.content.contains("TCP is not restricted"),
        "{}",
        output.content
    );
    let layout = Layout::new();
    let work = layout.path("work");
    let outside = layout.path("outside/new");
    let sandbox = layout
        .sandbox(Settings {
            network: false,
            ..settings()
        })
        .with_support(Support::Abi(3));
    let command = format!("echo x > {}", outside.display());
    let output = run(&work, sandbox, &command).await;
    assert!(output.is_error);
    assert!(
        output
            .content
            .starts_with("not run: the sandbox is required (sandbox = require) but"),
        "{}",
        output.content
    );
    assert!(!outside.exists());
    // With the network allowed, ABI 3 confines files.
    let sandbox = layout.sandbox(settings()).with_support(Support::Abi(3));
    let output = run(&work, sandbox, &command).await;
    assert!(output.is_error, "{}", output.content);
    assert!(!outside.exists());
}

#[tokio::test]
async fn an_abi_without_truncation_says_so_or_refuses() {
    if !landlock() {
        return;
    }
    // ABI 2 does not handle truncate(2): `require` refuses, `auto`
    // confines the rest and says once what is left open.
    let layout = Layout::new();
    let work = layout.path("work");
    let command = "echo ran".to_string();
    let refused = run(
        &work,
        layout.sandbox(settings()).with_support(Support::Abi(2)),
        &command,
    )
    .await;
    assert!(refused.is_error);
    assert_eq!(
        refused.content,
        "not run: the sandbox is required (sandbox = require) but the kernel's Landlock ABI 2 \
         cannot stop commands from emptying (truncating) files; that needs ABI 3 (Linux 6.2)"
    );
    let auto = Settings {
        mode: Some(Mode::Auto),
        network: false,
        ..settings()
    };
    let bash =
        Bash::for_test(&work).confined(Some(layout.sandbox(auto).with_support(Support::Abi(2))));
    let first = bash.run(json!({ "command": command })).await;
    assert_eq!(
        first.content,
        "ran\n[exit code 0]\n[sandbox: the kernel's Landlock ABI 2 cannot stop commands from \
         emptying (truncating) files; that needs ABI 3 (Linux 6.2); the settings file and the \
         data folders can be emptied, not read]\n[sandbox: sandbox_network = 0 needs Landlock \
         ABI 4, the kernel has 2; TCP is not restricted]"
    );
    let second = bash.run(json!({ "command": command })).await;
    assert_eq!(second, ToolOutput::ok("ran\n[exit code 0]"));
    // Still confined otherwise.
    let outside = layout.path("outside/new");
    let write = format!("echo x > {}", outside.display());
    let output = bash.run(json!({ "command": write })).await;
    assert!(output.is_error, "{}", output.content);
    assert!(!outside.exists());
}

#[tokio::test]
async fn without_landlock_auto_runs_and_says_so_once_and_require_refuses() {
    let layout = Layout::new();
    let work = layout.path("work");
    let marker = layout.path("outside/ran");
    let command = format!("echo x > {} && echo ran", marker.display());
    let fake = Support::Unavailable("the kernel has no Landlock (fake)".into());

    let refused = run(
        &work,
        layout.sandbox(settings()).with_support(fake.clone()),
        &command,
    )
    .await;
    assert_eq!(
        refused,
        ToolOutput::error(
            "not run: the sandbox is required (sandbox = require) but the kernel has no \
             Landlock (fake)"
        )
    );
    assert!(!marker.exists());

    let bash = Bash::for_test(&work).confined(Some(
        layout
            .sandbox(Settings {
                mode: Some(Mode::Auto),
                ..settings()
            })
            .with_support(fake),
    ));
    let first = bash.run(json!({ "command": command })).await;
    assert!(!first.is_error, "{}", first.content);
    assert!(
        first.content.ends_with(
            "[exit code 0]\n[sandbox: the kernel has no Landlock (fake); commands run without \
             it (sandbox = auto)]"
        ),
        "{}",
        first.content
    );
    assert!(marker.exists());
    let second = bash.run(json!({ "command": command })).await;
    assert_eq!(second, ToolOutput::ok("ran\n[exit code 0]"));
}

#[test]
fn reads_the_settings() {
    let read = |pairs: &[(&str, &str)]| {
        Settings::read(|key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    };
    let defaults = read(&[]).unwrap();
    assert_eq!(defaults.mode, None);
    assert_eq!(defaults.mode(), Mode::Auto);
    assert!(defaults.network);
    assert_eq!(defaults.read, None);
    assert!(defaults.webhook_read.is_empty());
    assert!(defaults.write.is_empty());
    assert_eq!(
        defaults.temp[..3],
        [
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/dev/shm")
        ]
    );
    // And this process's temporary folder, once.
    assert!(defaults.temp.contains(&std::env::temp_dir()));
    let mut unique = defaults.temp.clone();
    unique.dedup();
    assert_eq!(unique, defaults.temp);
    let unattended = Settings {
        unattended: true,
        ..defaults.clone()
    };
    assert_eq!(unattended.mode(), Mode::Require);
    let auto = Settings {
        mode: Some(Mode::Auto),
        ..unattended
    };
    assert_eq!(auto.mode(), Mode::Auto);

    let set = read(&[
        ("JUNEES_SANDBOX", "Require"),
        ("JUNEES_SANDBOX_WRITE", "/data/out, ,~/.cache"),
        ("HOME", "/home/tester"),
        ("JUNEES_SANDBOX_READ", "/usr,/etc"),
        ("JUNEES_WEBHOOK_SANDBOX_READ", "/srv/in, ~/inbox"),
        ("JUNEES_SANDBOX_NETWORK", "0"),
        ("TMPDIR", "/scratch"),
    ])
    .unwrap();
    assert_eq!(set.mode, Some(Mode::Require));
    assert_eq!(
        set.write,
        [
            PathBuf::from("/data/out"),
            PathBuf::from("/home/tester/.cache")
        ]
    );
    assert_eq!(
        set.read,
        Some(vec![PathBuf::from("/usr"), PathBuf::from("/etc")])
    );
    assert_eq!(
        set.webhook_read,
        [
            PathBuf::from("/srv/in"),
            PathBuf::from("/home/tester/inbox")
        ]
    );
    assert!(!set.network);
    assert!(set.temp.contains(&PathBuf::from("/scratch")));
    // A relative TMPDIR is not a folder to allow.
    let relative = read(&[("TMPDIR", "scratch")]).unwrap();
    assert!(!relative.temp.contains(&PathBuf::from("scratch")));

    let error = |pairs: &[(&str, &str)]| read(pairs).err().unwrap().to_string();
    // Without HOME, `~` stays relative.
    assert_eq!(
        error(&[("JUNEES_SANDBOX_READ", "~/x")]),
        "JUNEES_SANDBOX_READ takes absolute paths (or ~/...), got `~/x`"
    );
    assert_eq!(
        error(&[("JUNEES_SANDBOX", "strict")]),
        "JUNEES_SANDBOX must be auto, require or off, got `strict`"
    );
    assert_eq!(
        error(&[("JUNEES_SANDBOX_WRITE", "out")]),
        "JUNEES_SANDBOX_WRITE takes absolute paths (or ~/...), got `out`"
    );
    assert_eq!(
        error(&[("JUNEES_SANDBOX_NETWORK", "maybe")]),
        "JUNEES_SANDBOX_NETWORK must be 1 or 0, got `maybe`"
    );
}

#[test]
fn the_tools_sandbox_follows_the_settings() {
    let layout = Layout::new();
    let tools = |sandbox, webhook| ToolSettings {
        settings_file: Some(layout.settings_file()),
        sandbox,
        webhook,
        ..ToolSettings::default()
    };
    assert!(Sandbox::for_tools(&tools(None, false)).is_none());
    let off = Settings {
        mode: Some(Mode::Off),
        ..settings()
    };
    assert!(Sandbox::for_tools(&tools(Some(off), false)).is_none());
    let junees = layout.path("home/.junees");
    let plain = Sandbox::for_tools(&tools(Some(settings()), false)).unwrap();
    assert_eq!(
        plain.hidden,
        [
            junees.join("config.toml"),
            junees.join("mcp.json"),
            junees.join("secrets"),
            junees.join(crate::browser::PROFILES_FOLDER)
        ]
    );
    assert!(plain.read_only.contains(&junees.join("memory")));
    assert!(plain.read_only.contains(&junees.join("sessions")));
    assert_eq!(plain.state, Some(junees.clone()));
    assert!(plain.read_only.contains(&junees.join("skills")));
    let webhook = Sandbox::for_tools(&tools(Some(settings()), true)).unwrap();
    assert!(
        webhook
            .hidden
            .contains(&layout.path("home/.git-credentials")),
        "{:?}",
        webhook.hidden
    );
    assert!(webhook.hidden.contains(&layout.path("home/.config/gh")));
    assert!(webhook.hidden.contains(&layout.path("home/.claude")));
    assert!(webhook.hidden.contains(&junees.join("sessions")));
    assert!(!webhook.read_only.contains(&junees.join("sessions")));
}

#[test]
fn unattended_runs_default_to_require() {
    let tools = ToolSettings {
        sandbox: Some(Settings {
            mode: None,
            ..settings()
        }),
        ..ToolSettings::default()
    };
    let cron = crate::cron::run::Trigger::Cron.tool_settings(&tools);
    assert_eq!(cron.sandbox.unwrap().mode(), Mode::Require);
    assert_eq!(tools.sandbox.as_ref().unwrap().mode(), Mode::Auto);
}
