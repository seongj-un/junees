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
         echo x > {j}/sessions/new.jsonl && echo sessions-ok",
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
        "sessions-ok",
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
async fn a_webhook_run_cannot_read_git_and_github_credentials() {
    if !landlock() {
        return;
    }
    let layout = Layout::new();
    let hosts = layout.path("home/.config/gh/hosts.yml");
    std::fs::create_dir_all(hosts.parent().unwrap()).unwrap();
    std::fs::write(&hosts, "oauth_token: fake-gh-token\n").unwrap();
    let command = format!("cat {} || echo refused", hosts.display());
    let tools = |webhook| ToolSettings {
        settings_file: Some(layout.settings_file()),
        sandbox: Some(settings()),
        webhook,
        ..ToolSettings::default()
    };
    let work = layout.path("work");
    let plain = run(&work, Sandbox::for_tools(&tools(false)).unwrap(), &command).await;
    assert!(plain.content.contains("fake-gh-token"), "{}", plain.content);
    let webhook = run(&work, Sandbox::for_tools(&tools(true)).unwrap(), &command).await;
    assert!(
        !webhook.content.contains("fake-gh-token"),
        "{}",
        webhook.content
    );
    assert!(webhook.content.contains("refused"), "{}", webhook.content);
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
    assert!(defaults.write.is_empty());
    assert_eq!(
        defaults.temp,
        [
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/dev/shm")
        ]
    );
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
    assert!(!set.network);
    assert_eq!(set.temp.last(), Some(&PathBuf::from("/scratch")));
    // A relative TMPDIR is not a folder to allow.
    let relative = read(&[("TMPDIR", "scratch")]).unwrap();
    assert_eq!(relative.temp.len(), 3);

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
            junees.join("secrets")
        ]
    );
    assert!(plain.read_only.contains(&junees.join("memory")));
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
