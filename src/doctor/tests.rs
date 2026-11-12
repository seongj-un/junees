//! Tests of `junees doctor`: every check against a temporary state folder,
//! a fake `PATH`, a fake `/proc/sys` and fake `claude` scripts.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;
use crate::claude_code::testing::{fake_claude, fake_claude_auth};
use crate::cron::schedule::Schedule;
use crate::mcp::oauth::store::Saved;
use crate::webhook::store::Secret;

/// A state folder `home` (0700), a `bin` folder that is the whole `PATH`,
/// a fake `/proc/sys` and the variables the checks see.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    sysctl: PathBuf,
    env: Vec<(String, String)>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (home, bin, sysctl) = (root.join("home"), root.join("bin"), root.join("sys"));
        for folder in [&home, &bin, &sysctl] {
            std::fs::create_dir_all(folder).unwrap();
        }
        mode(&home, 0o700);
        let mut fixture = Self {
            _dir: dir,
            env: Vec::new(),
            root,
            home,
            bin,
            sysctl,
        };
        fixture.set("JUNEES_HOME", &fixture.home.display().to_string());
        fixture.set("HOME", &fixture.root.display().to_string());
        fixture.set("PATH", &fixture.bin.display().to_string());
        // `--online` asks for the latest release: never GitHub in tests
        // (port 9 refuses the connection).
        fixture.set(crate::update::BASE_ENV, "http://127.0.0.1:9/releases");
        fixture
    }

    fn set(&mut self, key: &str, value: &str) {
        self.env.retain(|(name, _)| name != key);
        self.env.push((key.to_string(), value.to_string()));
    }

    /// Writes config.toml (0600).
    fn config(&self, text: &str) {
        self.write("config.toml", text, 0o600);
    }

    /// Writes `name` under the state folder with `mode`.
    fn write(&self, name: &str, text: &str, mode_bits: u32) -> PathBuf {
        let path = self.home.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        mode(&path, mode_bits);
        path
    }

    /// An executable file `name` on the fake PATH (never run).
    fn program(&self, name: &str) -> PathBuf {
        let path = self.bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        mode(&path, 0o755);
        path
    }

    /// Sets a switch of the fake `/proc/sys`.
    fn sysctl(&self, name: &str, value: &str) {
        let path = self.sysctl.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{value}\n")).unwrap();
    }

    fn settings(&self) -> (Settings, Option<anyhow::Error>) {
        let env = self.env.clone();
        Settings::load_for_setup(move |key| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        })
    }

    async fn run(&self, online: bool) -> Report {
        self.run_landlock(online, None).await
    }

    /// [`Self::run`] with the kernel's Landlock answer `landlock` (`None`:
    /// the real kernel's).
    async fn run_landlock(
        &self,
        online: bool,
        landlock: Option<crate::sandbox::Support>,
    ) -> Report {
        let (settings, broken) = self.settings();
        let probe = Probe {
            sysctl: self.sysctl.clone(),
            landlock,
            work: Some(self.root.clone()),
        };
        run_with(&settings, broken.as_ref(), online, &probe).await
    }
}

fn mode(path: &Path, bits: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
}

/// The check `area`/`name`; panics with the whole report when missing.
fn check<'a>(report: &'a Report, area: &str, name: &str) -> &'a Check {
    report
        .checks
        .iter()
        .find(|check| check.area == area && check.name == name)
        .unwrap_or_else(|| panic!("no check {area}/{name} in\n{}", report.text()))
}

/// Asserts the status of `area`/`name` and that its detail holds `part`.
fn expect(report: &Report, area: &str, name: &str, status: Status, part: &str) {
    let found = check(report, area, name);
    assert_eq!(found.status, status, "{area}/{name}\n{}", report.text());
    assert!(
        found.detail.contains(part),
        "{area}/{name}: `{}` lacks `{part}`",
        found.detail
    );
    assert_eq!(
        found.fix.is_some(),
        matches!(status, Status::Warn | Status::Fail),
        "a warning or failure says how to fix it: {found:?}"
    );
}

fn has(report: &Report, area: &str, name: &str) -> bool {
    report
        .checks
        .iter()
        .any(|check| check.area == area && check.name == name)
}

#[test]
fn parses_its_arguments() {
    let parse = |args: &[&str]| super::parse(args.iter().map(|arg| arg.to_string()));
    assert_eq!(parse(&[]).unwrap(), Some(Options::default()));
    assert_eq!(
        parse(&["--online", "--json"]).unwrap(),
        Some(Options {
            json: true,
            online: true
        })
    );
    assert_eq!(parse(&["--json", "-h"]).unwrap(), None);
    let err = parse(&["--fix"]).unwrap_err().to_string();
    assert!(err.contains("unknown argument `--fix`"), "{err}");
}

#[tokio::test]
async fn the_state_folder_and_its_files_must_be_private() {
    let fixture = Fixture::new();
    fixture.config("model = \"m\"\n");
    fixture.write("mcp.json", "{\"servers\": {}}", 0o644);
    fixture.write("cron/jobs.json", "{\"format\": 1, \"jobs\": []}", 0o640);
    fixture.write(
        "webhooks/routes.json",
        "{\"format\": 1, \"routes\": []}",
        0o620,
    );
    fixture.write("secrets/hook", "s3cret\n", 0o600);
    fixture.write("secrets/open", "s3cret\n", 0o644);
    mode(&fixture.home.join("secrets"), 0o700);
    mode(&fixture.home, 0o755);

    let report = fixture.run(false).await;
    let folder = check(&report, "state folder", "folder");
    assert_eq!(folder.status, Status::Fail, "{}", report.text());
    assert_eq!(
        folder.fix.as_deref(),
        Some(format!("chmod 700 {}", fixture.home.display()).as_str())
    );
    expect(
        &report,
        "state folder",
        "config.toml",
        Status::Ok,
        "mode 600",
    );
    // Others may read it, and it may hold tokens.
    expect(
        &report,
        "state folder",
        "mcp.json",
        Status::Fail,
        "mode 644",
    );
    // Readable by others only: a warning; changeable: a failure.
    expect(
        &report,
        "state folder",
        "cron/jobs.json",
        Status::Warn,
        "mode 640",
    );
    expect(
        &report,
        "state folder",
        "webhooks/routes.json",
        Status::Fail,
        "can change it (mode 620)",
    );
    expect(&report, "state folder", "secrets/", Status::Ok, "mode 700");
    expect(
        &report,
        "state folder",
        "secrets/hook",
        Status::Ok,
        "mode 600",
    );
    expect(
        &report,
        "state folder",
        "secrets/open",
        Status::Fail,
        "mode 644",
    );
    assert_eq!(
        check(&report, "state folder", "secrets/open")
            .fix
            .as_deref(),
        Some(format!("chmod 600 {}", fixture.home.join("secrets/open").display()).as_str())
    );
    assert!(report.failed());

    mode(&fixture.home, 0o700);
    let report = fixture.run(false).await;
    expect(
        &report,
        "state folder",
        "folder",
        Status::Ok,
        "private (mode 700)",
    );
}

#[tokio::test]
async fn a_missing_state_folder_is_a_warning() {
    let mut fixture = Fixture::new();
    let missing = fixture.root.join("not-yet");
    fixture.set("JUNEES_HOME", &missing.display().to_string());
    fixture.set("ANTHROPIC_API_KEY", "sk-test");
    fixture.program("git");
    let report = fixture.run(false).await;
    expect(
        &report,
        "state folder",
        "folder",
        Status::Warn,
        "does not exist yet",
    );
    expect(
        &report,
        "settings",
        "config.toml",
        Status::Skip,
        "does not exist",
    );
    expect(&report, "cron", "daemon", Status::Skip, "no scheduled jobs");
    assert!(!report.failed(), "{}", report.text());
    // Looking writes nothing.
    assert!(!missing.exists());
}

#[tokio::test]
async fn the_settings_file_must_read_and_every_value_too() {
    let fixture = Fixture::new();
    fixture.config("model = \"m\"\nwat = 1\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "settings",
        "config.toml",
        Status::Fail,
        "line 2: unknown setting `wat`",
    );

    for (text, expected) in [
        ("max_turns = 0\n", "JUNEES_MAX_TURNS must be greater than 0"),
        ("timezone = \"Mars/Base\"\n", "Mars/Base"),
        ("cron_max_time = \"soon\"\n", "JUNEES_CRON_MAX_TIME"),
        (
            "webhook_max_tokens = \"many\"\n",
            "JUNEES_WEBHOOK_MAX_TOKENS",
        ),
    ] {
        fixture.config(text);
        let report = fixture.run(false).await;
        expect(
            &report,
            "settings",
            "config.toml",
            Status::Ok,
            "every key known",
        );
        expect(&report, "settings", "values", Status::Fail, expected);
    }

    fixture.config("backend = \"codex\"\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "settings",
        "backend",
        Status::Fail,
        "unknown backend `codex`",
    );

    fixture.config("max_turns = 3\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "settings",
        "values",
        Status::Ok,
        "every setting reads",
    );
    expect(&report, "settings", "backend", Status::Ok, "api");
}

#[tokio::test]
async fn keys_say_whether_and_where_they_are_set() {
    let mut fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(
        &report,
        "keys",
        "Anthropic API key",
        Status::Fail,
        "the api backend needs it",
    );
    expect(&report, "keys", "GitHub token", Status::Skip, "not set");
    expect(&report, "keys", "notify webhook", Status::Skip, "not set");

    fixture.config("backend = \"claude-code\"\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "keys",
        "Anthropic API key",
        Status::Skip,
        "does not need it",
    );

    fixture.config(
        "api_key = \"sk-from-file\"\ngithub_token = \"ghp_x\"\n\
         notify_webhook = \"https://hooks.example/1/x\"\n",
    );
    let report = fixture.run(false).await;
    expect(
        &report,
        "keys",
        "Anthropic API key",
        Status::Ok,
        "set (config.toml)",
    );
    expect(
        &report,
        "keys",
        "GitHub token",
        Status::Ok,
        "set (config.toml)",
    );
    expect(
        &report,
        "keys",
        "notify webhook",
        Status::Ok,
        "posts to hooks.example",
    );

    fixture.set("ANTHROPIC_API_KEY", "sk-from-env");
    fixture.config("notify_webhook = \"http://hooks.example/1/x\"\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "keys",
        "Anthropic API key",
        Status::Ok,
        "set (environment)",
    );
    expect(
        &report,
        "keys",
        "notify webhook",
        Status::Fail,
        "must be an https URL",
    );
}

/// No value of a secret setting reaches the report, even where an error
/// would quote it (a token pasted as the allowed users).
#[tokio::test]
async fn secrets_never_show() {
    const API_KEY: &str = "sk-ant-doctor-secret-one";
    const GITHUB: &str = "ghp_doctorSecretTwo";
    const DISCORD: &str = "doctor.discord.secret3";
    const WEBHOOK: &str = "https://discord.com/api/webhooks/1/doctor-secret-four";
    let mut fixture = Fixture::new();
    fixture.config(&format!(
        "api_key = \"{API_KEY}\"\ngithub_token = \"{GITHUB}\"\ndiscord_token = \"{DISCORD}\"\n\
         discord_allowed_users = \"{DISCORD}\"\nnotify_webhook = \"{WEBHOOK}\"\n"
    ));
    fixture.set("JUNEES_GITHUB_TOKEN", GITHUB);
    let report = fixture.run(false).await;
    let text = report.text();
    let json = report.json().to_string();
    for secret in [API_KEY, GITHUB, DISCORD, WEBHOOK, "doctor-secret-four"] {
        assert!(!text.contains(secret), "{secret} in\n{text}");
        assert!(!json.contains(secret), "{secret} in {json}");
    }
    // The allowed-users error quoted the token; only its length is left.
    expect(
        &report,
        "discord",
        "gateway",
        Status::Fail,
        "got 22 characters (not shown)",
    );
    expect(&report, "keys", "Anthropic API key", Status::Ok, "set");
}

#[tokio::test]
async fn telegram_needs_a_token_and_allowed_users() {
    let fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(&report, "telegram", "gateway", Status::Skip, "not set up");
    // Built in pieces so no token-looking literal sits in the source.
    let token = ["4242", "doctor-telegram-secret_5"].join(":");
    fixture.config(&format!("telegram_token = \"{token}\"\n"));
    let report = fixture.run(false).await;
    expect(
        &report,
        "telegram",
        "gateway",
        Status::Fail,
        "no allowed Telegram users",
    );
    // The token pasted as the allowed users: the error quotes it.
    fixture.config(&format!(
        "telegram_token = \"{token}\"\ntelegram_allowed_users = \"{token}\"\n"
    ));
    let report = fixture.run(false).await;
    assert_hidden(&report, "doctor-telegram-secret");
    expect(&report, "telegram", "gateway", Status::Fail, "not shown");
    fixture.config(&format!(
        "telegram_token = \"{token}\"\ntelegram_allowed_users = \"12, 34\"\n"
    ));
    let report = fixture.run(false).await;
    expect(
        &report,
        "telegram",
        "gateway",
        Status::Ok,
        "token set (config.toml), 2 allowed users",
    );
    assert_hidden(&report, "doctor-telegram-secret");
}

#[tokio::test]
async fn discord_needs_a_token_and_allowed_users() {
    let fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(&report, "discord", "gateway", Status::Skip, "not set up");
    fixture.config("discord_token = \"abc.def\"\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "discord",
        "gateway",
        Status::Fail,
        "no allowed Discord users",
    );
    fixture.config("discord_token = \"abc.def\"\ndiscord_allowed_users = \"12, 34\"\n");
    let report = fixture.run(false).await;
    expect(
        &report,
        "discord",
        "gateway",
        Status::Ok,
        "token set (config.toml), 2 allowed users",
    );
}

#[tokio::test]
async fn claude_code_needs_claude_bubblewrap_and_user_namespaces() {
    let mut fixture = Fixture::new();
    fixture.config("backend = \"claude-code\"\n");
    let claude = fake_claude(&fixture.root, "");
    fixture.set("JUNEES_CLAUDE_BIN", &claude.display().to_string());
    let bwrap = fixture.program("bwrap");
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "claude", Status::Ok, "2.1.319");
    expect(
        &report,
        "claude-code",
        "sign-in",
        Status::Skip,
        "not checked offline",
    );
    expect(&report, "claude-code", "env scrub", Status::Ok, "on");
    expect(
        &report,
        "claude-code",
        "bubblewrap",
        Status::Ok,
        &bwrap.display().to_string(),
    );
    expect(
        &report,
        "claude-code",
        "user namespaces",
        Status::Ok,
        "allowed",
    );

    // This server: AppArmor restricts user namespaces, no bwrap.
    std::fs::remove_file(&bwrap).unwrap();
    fixture.sysctl("kernel/apparmor_restrict_unprivileged_userns", "1");
    let report = fixture.run(false).await;
    expect(
        &report,
        "claude-code",
        "bubblewrap",
        Status::Fail,
        "the scrub cannot run",
    );
    expect(
        &report,
        "claude-code",
        "user namespaces",
        Status::Warn,
        "kernel.apparmor_restrict_unprivileged_userns = 1",
    );
    assert!(report.failed());

    // A setuid bwrap needs no user namespace.
    let bwrap = fixture.program("bwrap");
    mode(&bwrap, 0o4755);
    let report = fixture.run(false).await;
    expect(
        &report,
        "claude-code",
        "bubblewrap",
        Status::Ok,
        "setuid root",
    );
    expect(
        &report,
        "claude-code",
        "user namespaces",
        Status::Ok,
        "setuid",
    );

    // The other switches count too.
    mode(&bwrap, 0o755);
    fixture.sysctl("kernel/apparmor_restrict_unprivileged_userns", "0");
    fixture.sysctl("user/max_user_namespaces", "0");
    let report = fixture.run(false).await;
    expect(
        &report,
        "claude-code",
        "user namespaces",
        Status::Warn,
        "user.max_user_namespaces = 0",
    );

    // With the scrub off, bubblewrap does not matter but the user is warned.
    fixture.set("CLAUDE_CODE_SUBPROCESS_ENV_SCRUB", "0");
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "env scrub", Status::Warn, "is off");
    assert!(!has(&report, "claude-code", "bubblewrap"));
}

#[tokio::test]
async fn claude_problems_fail_on_its_backend_only() {
    let mut fixture = Fixture::new();
    fixture.config("backend = \"claude-code\"\n");
    let old = fake_claude(
        &fixture.root,
        "[ \"$1\" = --version ] && echo '2.0.1 (Claude Code)' && exit 0",
    );
    fixture.set("JUNEES_CLAUDE_BIN", &old.display().to_string());
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "claude", Status::Fail, "is too old");

    let missing = fixture.root.join("no-claude");
    fixture.set("JUNEES_CLAUDE_BIN", &missing.display().to_string());
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "claude", Status::Fail, "cannot run");

    // On the api backend the same is a warning, and without any claude
    // the part is skipped.
    fixture.config("api_key = \"sk-test\"\n");
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "claude", Status::Warn, "cannot run");
    expect(
        &report,
        "claude-code",
        "bubblewrap",
        Status::Warn,
        "not on PATH",
    );
    fixture.env.retain(|(name, _)| name != "JUNEES_CLAUDE_BIN");
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "claude", Status::Skip, "not used");
    assert!(!has(&report, "claude-code", "bubblewrap"));
}

/// `claude auth status` runs with --online only.
#[tokio::test]
async fn the_sign_in_is_asked_online_only() {
    let mut fixture = Fixture::new();
    fixture.config("backend = \"claude-code\"\n");
    fixture.program("bwrap");
    let marker = fixture.root.join("auth-asked");
    let claude = fake_claude_auth(
        &fixture.root,
        &format!(
            "touch '{}'\necho '{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}'; exit 0",
            marker.display()
        ),
        "",
    );
    fixture.set("JUNEES_CLAUDE_BIN", &claude.display().to_string());
    let report = fixture.run(false).await;
    expect(&report, "claude-code", "claude", Status::Ok, "2.1.319");
    assert!(!marker.exists(), "offline asked `claude auth status`");

    let report = fixture.run(true).await;
    assert!(marker.exists());
    expect(&report, "claude-code", "sign-in", Status::Ok, "signed in");
    // No key, so nothing to send to the API.
    expect(&report, "online", "API key", Status::Skip, "no key");

    let claude = fake_claude_auth(
        &fixture.root,
        "echo '{\"loggedIn\":false,\"authMethod\":\"none\"}'; exit 1",
        "",
    );
    fixture.set("JUNEES_CLAUDE_BIN", &claude.display().to_string());
    let report = fixture.run(true).await;
    expect(
        &report,
        "claude-code",
        "sign-in",
        Status::Fail,
        "not signed in",
    );
}

#[tokio::test]
async fn git_and_gh_are_looked_up_on_path() {
    let fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(&report, "programs", "git", Status::Warn, "not on PATH");
    expect(&report, "programs", "gh", Status::Skip, "optional");
    let git = fixture.program("git");
    fixture.program("gh");
    let report = fixture.run(false).await;
    expect(
        &report,
        "programs",
        "git",
        Status::Ok,
        &git.display().to_string(),
    );
    expect(&report, "programs", "gh", Status::Ok, "gh");
}

#[tokio::test]
async fn the_cron_daemon_is_seen_through_its_lock() {
    let fixture = Fixture::new();
    let store = crate::cron::store::Store::new(&fixture.home);
    let mut locked = store.lock().unwrap();
    locked.jobs.push(crate::cron::store::tests::job(
        "aaaaaaaa",
        Schedule::Every {
            secs: 3600,
            anchor: 0,
        },
        Some(4_000_000_000),
    ));
    locked.save().unwrap();
    drop(locked);
    let report = fixture.run(false).await;
    expect(
        &report,
        "cron",
        "daemon",
        Status::Warn,
        "1 jobs, 1 waiting to run; the daemon is not running",
    );
    let daemon = crate::cron::daemon::lock(&fixture.home).unwrap();
    let report = fixture.run(false).await;
    expect(
        &report,
        "cron",
        "daemon",
        Status::Ok,
        "the daemon is running",
    );
    drop(daemon);

    fixture.write("cron/jobs.json", "{\"format\": 1, \"jobs\": [", 0o600);
    let report = fixture.run(false).await;
    expect(&report, "cron", "jobs", Status::Fail, "is broken");
}

#[tokio::test]
async fn webhook_secrets_and_server_are_checked() {
    let mut fixture = Fixture::new();
    let store = crate::webhook::store::Store::new(&fixture.home);
    let secret = fixture.write("secrets/pr", "hook-secret-value\n", 0o600);
    let mut locked = store.lock().unwrap();
    let mut by_env = crate::webhook::store::tests::route("env");
    by_env.secret = Some(Secret::Env("DOCTOR_HOOK_SECRET".into()));
    let mut by_file = crate::webhook::store::tests::route("file");
    by_file.secret = Some(Secret::File(secret.clone()));
    let mut open = crate::webhook::store::tests::route("unsigned");
    open.secret = None;
    open.paused = true;
    locked.routes.extend([by_env, by_file, open]);
    locked.save().unwrap();
    drop(locked);

    let report = fixture.run(false).await;
    expect(
        &report,
        "webhooks",
        "route env",
        Status::Warn,
        "DOCTOR_HOOK_SECRET is not set here",
    );
    expect(
        &report,
        "webhooks",
        "route file",
        Status::Ok,
        "secret: file",
    );
    expect(
        &report,
        "webhooks",
        "route unsigned",
        Status::Ok,
        "unsigned",
    );
    expect(
        &report,
        "webhooks",
        "server",
        Status::Warn,
        "3 routes, 2 not paused; the server is not running",
    );

    fixture.set("DOCTOR_HOOK_SECRET", "env-secret-value");
    mode(&secret, 0o644);
    let server = crate::webhook::server::lock(&fixture.home).unwrap();
    let report = fixture.run(false).await;
    expect(
        &report,
        "webhooks",
        "route env",
        Status::Ok,
        "env DOCTOR_HOOK_SECRET",
    );
    expect(
        &report,
        "webhooks",
        "route file",
        Status::Fail,
        "must be yours and private",
    );
    expect(
        &report,
        "webhooks",
        "server",
        Status::Ok,
        "the server is running",
    );
    drop(server);
    for value in ["hook-secret-value", "env-secret-value"] {
        assert!(!report.text().contains(value));
    }
}

#[tokio::test]
async fn mcp_commands_are_looked_up_without_starting_them() {
    let fixture = Fixture::new();
    let marker = fixture.root.join("started");
    let server = fixture.bin.join("fake-mcp");
    std::fs::write(
        &server,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    mode(&server, 0o755);
    let local = fixture.root.join("srv");
    std::fs::write(&local, "").unwrap();
    mode(&local, 0o755);
    let servers = json!({"servers": {
        "found": {"command": "fake-mcp"},
        "missing": {"command": "no-such-mcp"},
        "off": {"command": "no-such-mcp", "enabled": false},
        "local": {"command": "./srv", "cwd": fixture.root.display().to_string()},
        "gone": {"command": "fake-mcp", "cwd": fixture.root.join("nope").display().to_string()},
        "web": {"url": "https://mcp.example.com/s3cret-path/mcp",
                "headers": {"Authorization": "Bearer env:WEB_TOKEN"}},
        "unset": {"url": "https://mcp.example.com/mcp",
                  "headers": {"X-Key": "env:UNSET_TOKEN"}},
    }});
    fixture.write("mcp.json", &servers.to_string(), 0o600);
    let mut fixture = fixture;
    fixture.set("WEB_TOKEN", "web-token-value");
    let report = fixture.run(false).await;
    expect(
        &report,
        "mcp",
        "server web",
        Status::Ok,
        "Streamable HTTP `https://mcp.example.com`, headers Authorization (from env WEB_TOKEN) \
         (not contacted)",
    );
    expect(
        &report,
        "mcp",
        "server unset",
        Status::Fail,
        "header `X-Key` takes its value from the variable UNSET_TOKEN, which is not set",
    );
    for secret in ["s3cret", "web-token-value"] {
        assert!(!report.text().contains(secret), "{}", report.text());
    }
    expect(
        &report,
        "mcp",
        "server found",
        Status::Ok,
        &server.display().to_string(),
    );
    expect(
        &report,
        "mcp",
        "server missing",
        Status::Fail,
        "`no-such-mcp` is not an executable file or on PATH",
    );
    expect(&report, "mcp", "server off", Status::Skip, "disabled");
    expect(
        &report,
        "mcp",
        "server local",
        Status::Ok,
        &format!("`./srv` is {}/./srv", fixture.root.display()),
    );
    expect(
        &report,
        "mcp",
        "server gone",
        Status::Fail,
        "is not a folder",
    );
    assert!(!marker.exists(), "an MCP server was started");

    fixture.write(
        "mcp.json",
        "{\"servers\": {\"x\": {\"command\": 1}}}",
        0o600,
    );
    let report = fixture.run(false).await;
    expect(&report, "mcp", "mcp.json", Status::Fail, "wrong type");
}

fn saved_login(server_url: &str, expires_at: Option<u64>, refresh: bool) -> Saved {
    Saved {
        format: crate::mcp::oauth::store::FORMAT,
        server_url: server_url.into(),
        resource: server_url.into(),
        issuer: "https://auth.example.com".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        client_id: "client-1".into(),
        client_secret: Some("client-pw-s3cret".into()),
        client_auth: "client_secret_basic".into(),
        access_token: "access-tok-s3cret".into(),
        refresh_token: refresh.then(|| "refresh-tok-s3cret".into()),
        expires_at,
        scope: Some("files:read".into()),
        wanted_scope: None,
        saved_at: 1,
    }
}

#[tokio::test]
async fn saved_oauth_logins_are_checked_offline_and_never_shown() {
    use crate::mcp::oauth::store;
    let fixture = Fixture::new();
    let servers = json!({"servers": {
        "docs": {"url": "https://mcp.example.com/mcp"},
        "stale": {"url": "https://stale.example.com/mcp"},
        "moved": {"url": "https://mcp.example.com/new"},
        "open": {"url": "https://open.example.com/mcp"},
        "none": {"url": "https://none.example.com/mcp"},
        "own": {"url": "https://own.example.com/mcp",
                "headers": {"Authorization": "Bearer env:OWN_TOKEN"}},
    }});
    fixture.write("mcp.json", &servers.to_string(), 0o600);
    let now = store::now();
    let home = &fixture.home;
    let save = |name: &str, saved: &Saved| store::write(home, name, saved).unwrap();
    // Half an hour past 2 h: the check reads the clock again and rounds
    // the time left down to hours, so an expiry of exactly 2 h shows as
    // "1 h" once a second has passed.
    save(
        "docs",
        &saved_login("https://mcp.example.com/mcp", Some(now + 7200 + 1800), true),
    );
    save(
        "stale",
        &saved_login("https://stale.example.com/mcp", Some(now - 5), false),
    );
    save(
        "moved",
        &saved_login("https://mcp.example.com/mcp", None, true),
    );
    save(
        "open",
        &saved_login("https://open.example.com/mcp", None, true),
    );
    mode(&store::path(home, "open"), 0o644);
    save(
        "own",
        &saved_login("https://own.example.com/mcp", None, true),
    );
    let report = fixture.run(false).await;
    expect(
        &report,
        "mcp",
        "login docs",
        Status::Ok,
        "saved, from https://auth.example.com. The access token is valid for 2 h; junees \
         renews it by itself. Scope: files:read.",
    );
    expect(
        &report,
        "mcp",
        "login stale",
        Status::Warn,
        "has expired; it cannot be renewed",
    );
    expect(
        &report,
        "mcp",
        "login moved",
        Status::Fail,
        "for another address",
    );
    expect(&report, "mcp", "login open", Status::Fail, "chmod 600");
    expect(
        &report,
        "mcp",
        "login none",
        Status::Skip,
        "`junees mcp login none` if the server asks for one",
    );
    assert!(!has(&report, "mcp", "login own"));
    for secret in [
        "access-tok-s3cret",
        "refresh-tok-s3cret",
        "client-pw-s3cret",
    ] {
        assert_hidden(&report, secret);
    }
}

#[tokio::test]
async fn online_finds_the_authorization_server_without_logging_in() {
    use crate::mcp::oauth::tests::{Fake, Knobs};
    let good = Fake::start(Knobs::default()).await;
    let no_pkce = Fake::start(Knobs::without_pkce()).await;
    let no_registration = Fake::start(Knobs::without_registration()).await;
    let mut fixture = Fixture::new();
    // No API key: the online API check skips.
    fixture.set("PATH", "/nonexistent");
    let servers = json!({"servers": {
        "good": {"url": good.url()},
        "nopkce": {"url": no_pkce.url()},
        "noreg": {"url": no_registration.url()},
    }});
    fixture.write("mcp.json", &servers.to_string(), 0o600);
    let report = fixture.run(true).await;
    expect(
        &report,
        "mcp",
        "login good (online)",
        Status::Ok,
        "(PKCE S256), a client registered at login",
    );
    expect(
        &report,
        "mcp",
        "login nopkce (online)",
        Status::Warn,
        "PKCE with S256",
    );
    expect(
        &report,
        "mcp",
        "login noreg (online)",
        Status::Warn,
        "(PKCE S256)",
    );
    let fix = check(&report, "mcp", "login noreg (online)")
        .fix
        .clone()
        .unwrap();
    assert!(
        fix.contains("register a client at the authorization server and set `oauth.client_id`"),
        "{fix}"
    );
    for fake in [&good, &no_pkce, &no_registration] {
        let paths = fake.paths();
        assert!(
            paths
                .iter()
                .all(|path| !path.contains("/register") && !path.contains("/token")),
            "{paths:?}"
        );
    }
    // Offline nothing is asked.
    let before = good.paths().len();
    fixture.run(false).await;
    assert_eq!(good.paths().len(), before);
}

#[tokio::test]
async fn enabled_plugins_need_valid_manifests() {
    let fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(&report, "plugins", "enabled", Status::Skip, "no plugin");
    let plugins = crate::plugins::dir(&fixture.home);
    for (name, manifest) in [
        ("good", json!({"description": "notes"}).to_string()),
        ("bad", "{\"description\": ".to_string()),
    ] {
        let folder = plugins.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        mode(&folder, 0o700);
        std::fs::write(folder.join("plugin.json"), manifest).unwrap();
        mode(&folder.join("plugin.json"), 0o600);
    }
    mode(&plugins, 0o700);
    let state = crate::plugins::state::State {
        enabled: ["good".to_string(), "bad".to_string()].into(),
        trusted: Default::default(),
    };
    crate::plugins::state::save(&plugins, &state).unwrap();
    let report = fixture.run(false).await;
    expect(&report, "plugins", "good", Status::Ok, "manifest valid");
    expect(
        &report,
        "plugins",
        "enabled",
        Status::Fail,
        "bad not loaded",
    );
}

#[tokio::test]
async fn sessions_are_counted() {
    let fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(
        &report,
        "sessions",
        "folder",
        Status::Skip,
        "no saved sessions",
    );
    fixture.write("sessions/a.jsonl", "0123456789", 0o600);
    fixture.write("sessions/a/sub-1.jsonl", "01234", 0o600);
    let report = fixture.run(false).await;
    expect(&report, "sessions", "folder", Status::Ok, "2 files, 15 B");
    assert_eq!(files::human_size(1536), "1.5 KiB");
    assert_eq!(files::human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    let mut report = Report::default();
    files::sessions_over(&mut report, &fixture.home, 14);
    expect(&report, "sessions", "folder", Status::Warn, "2 files, 15 B");
    assert!(
        check(&report, "sessions", "folder")
            .fix
            .as_deref()
            .unwrap()
            .starts_with("delete old sessions there")
    );
}

#[tokio::test]
async fn the_report_reads_as_text_and_json() {
    let fixture = Fixture::new();
    fixture.config("api_key = \"sk-test\"\n");
    fixture.program("git");
    let report = fixture.run(false).await;
    assert!(!report.failed(), "{}", report.text());
    let json = report.json();
    assert_eq!(json["ok"], true);
    assert_eq!(json["online"], false);
    assert_eq!(json["home"], fixture.home.display().to_string());
    let checks = json["checks"].as_array().unwrap();
    assert_eq!(checks.len(), report.checks.len());
    let total: u64 = ["ok", "warn", "fail", "skip"]
        .iter()
        .map(|status| json["counts"][status].as_u64().unwrap())
        .sum();
    assert_eq!(total as usize, checks.len());
    for check in checks {
        let status = check["status"].as_str().unwrap();
        assert!(["ok", "warn", "fail", "skip"].contains(&status), "{check}");
        assert!(check["area"].is_string() && check["name"].is_string());
        assert!(check["detail"].is_string());
        assert_eq!(
            check.get("fix").is_some(),
            matches!(status, "warn" | "fail")
        );
    }
    let text = report.text();
    assert!(text.starts_with(&format!(
        "junees doctor: {} (offline",
        fixture.home.display()
    )));
    assert!(text.contains("\nstate folder\n  ok    folder: "), "{text}");
    assert!(text.ends_with(" skipped\n"), "{text}");

    mode(&fixture.home.join("config.toml"), 0o644);
    let report = fixture.run(false).await;
    assert!(report.failed());
    assert_eq!(report.json()["ok"], false);
    let text = report.text();
    assert!(
        text.contains("  FAIL  config.toml: other users can read it"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "        fix: chmod 600 {}",
            fixture.home.join("config.toml").display()
        )),
        "{text}"
    );
    assert_eq!(report.json()["counts"]["fail"], Value::from(1));
}

#[tokio::test]
async fn names_with_control_characters_are_escaped_in_the_text() {
    let fixture = Fixture::new();
    fixture.write("secrets/evil\u{1b}[2K", "x", 0o644);
    mode(&fixture.home.join("secrets"), 0o700);
    let text = fixture.run(false).await.text();
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("secrets/evil\\u{1b}[2K"), "{text}");
}

/// A fake API that answers each connection with the next status and
/// returns the requests.
async fn fake_api(statuses: Vec<u16>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for status in statuses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            requests.push(String::from_utf8_lossy(&buf).into_owned());
            let body = "{}";
            let response = format!(
                "HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
        requests
    });
    (url, handle)
}

#[tokio::test]
async fn online_asks_the_api_whether_it_takes_the_key() {
    let mut fixture = Fixture::new();
    fixture.config("api_key = \"sk-doctor-online-key\"\n");
    let (url, server) = fake_api(vec![200, 401, 500]).await;
    fixture.set("ANTHROPIC_BASE_URL", &format!("{url}/"));
    let report = fixture.run(true).await;
    expect(
        &report,
        "online",
        "API key",
        Status::Ok,
        "127.0.0.1 accepts the key",
    );
    let report = fixture.run(true).await;
    expect(
        &report,
        "online",
        "API key",
        Status::Fail,
        "refused the key (HTTP 401)",
    );
    let report = fixture.run(true).await;
    expect(&report, "online", "API key", Status::Warn, "HTTP 500");
    let requests = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("the fake API got fewer requests")
        .unwrap();
    for request in &requests {
        assert!(
            request.starts_with("GET /v1/models?limit=1 HTTP/1.1\r\n"),
            "{request}"
        );
        let head = request.to_lowercase();
        assert!(
            head.contains("x-api-key: sk-doctor-online-key\r\n"),
            "{request}"
        );
        assert!(
            head.contains("anthropic-version: 2023-06-01\r\n"),
            "{request}"
        );
    }
    assert!(!report.text().contains("sk-doctor-online-key"));
}

#[tokio::test]
async fn online_says_whether_a_newer_release_is_out() {
    let mut fixture = Fixture::new();
    let report = fixture.run(true).await;
    expect(&report, "online", "release", Status::Warn, "latest release");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut paths = Vec::new();
        for tag in ["v999.0.0", env!("CARGO_PKG_VERSION")] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = vec![0u8; 4096];
            let n = socket.read(&mut head).await.unwrap();
            let head = String::from_utf8_lossy(&head[..n]).into_owned();
            paths.push(head.split(' ').nth(1).unwrap_or("").to_string());
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: /o/junees/releases/tag/{}{tag}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n",
                if tag.starts_with('v') { "" } else { "v" }
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
        paths
    });
    fixture.set(crate::update::BASE_ENV, &format!("{base}/releases"));
    let report = fixture.run(true).await;
    expect(
        &report,
        "online",
        "release",
        Status::Warn,
        "junees 999.0.0 is out",
    );
    assert_eq!(
        check(&report, "online", "release").fix.as_deref(),
        Some("run `junees update`")
    );
    let report = fixture.run(true).await;
    expect(
        &report,
        "online",
        "release",
        Status::Ok,
        "is the latest release",
    );
    let paths = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("the fake release server got fewer requests")
        .unwrap();
    assert_eq!(paths, ["/releases/latest", "/releases/latest"]);
    // Offline, nothing is asked.
    assert!(!has(&fixture.run(false).await, "online", "release"));
}

#[tokio::test]
async fn offline_sends_nothing() {
    let mut fixture = Fixture::new();
    fixture.config("api_key = \"sk-test\"\n");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    fixture.set("ANTHROPIC_BASE_URL", &url);
    let report = fixture.run(false).await;
    assert!(!has(&report, "online", "API key"));
    let err = listener.accept().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
}

/// Asserts that neither the text nor the JSON report shows `secret`.
fn assert_hidden(report: &Report, secret: &str) {
    let (text, json) = (report.text(), report.json().to_string());
    assert!(!text.contains(secret), "{secret} in\n{text}");
    assert!(!json.contains(secret), "{secret} in {json}");
}

#[tokio::test]
async fn a_bot_prefixed_token_never_shows() {
    const TOKEN: &str = "abc.def.probesecret123";
    let fixture = Fixture::new();
    // The token with `Bot `, and the same token where the allowed users go
    // (their error quotes it); then the token and a piece of it where an
    // error quotes them without `got`.
    for (extra, backend) in [
        (format!("discord_allowed_users = \"{TOKEN}\"\n"), None),
        (
            format!("discord_allowed_users = \"1\"\nbackend = \"{TOKEN}\"\n"),
            Some("unknown backend `[hidden]`"),
        ),
        (
            "discord_allowed_users = \"1\"\nbackend = \"def.probesecret1\"\n".to_string(),
            Some("unknown backend `[hidden]`"),
        ),
    ] {
        fixture.config(&format!("discord_token = \"Bot {TOKEN}\"\n{extra}"));
        let report = fixture.run(false).await;
        assert_hidden(&report, "probesecret");
        match backend {
            Some(detail) => expect(&report, "settings", "backend", Status::Fail, detail),
            None => expect(
                &report,
                "discord",
                "gateway",
                Status::Fail,
                "got 22 characters (not shown)",
            ),
        }
    }
}

#[tokio::test]
async fn a_key_in_another_setting_never_shows() {
    // Built in pieces so no key-looking literal sits in the source.
    let key = ["sk", "ant", "api03", "Probe0123456789abcdefXYZ"].join("-");
    let mut fixture = Fixture::new();
    fixture.set("JUNEES_MAX_TOKENS", &key);
    let report = fixture.run(false).await;
    expect(
        &report,
        "settings",
        "values",
        Status::Fail,
        &format!(
            "JUNEES_MAX_TOKENS must be a positive integer, got {} characters (not shown)",
            key.len()
        ),
    );
    assert_hidden(&report, "Probe0123456789");
    // Quoted without `got`, it still looks like a key and is cut out.
    fixture.config(&format!("backend = \"{key}\"\n"));
    let report = fixture.run(false).await;
    expect(
        &report,
        "settings",
        "backend",
        Status::Fail,
        "unknown backend [hidden] use `api`",
    );
    assert_hidden(&report, "Probe0123456789");
    assert_eq!(crate::secrets::find_secret(&report.text()), None);
}

#[tokio::test]
async fn credentials_in_the_environment_never_show() {
    const VALUE: &str = "env-probe-credential";
    for var in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "JUNEES_GITHUB_TOKEN",
    ] {
        let mut fixture = Fixture::new();
        fixture.set(var, VALUE);
        // An error that quotes the value without `got`.
        fixture.config(&format!("backend = \"{VALUE}\"\n"));
        let report = fixture.run(false).await;
        assert_hidden(&report, "probe-credential");
        expect(
            &report,
            "settings",
            "backend",
            Status::Fail,
            "unknown backend `[hidden]`",
        );
    }
}

#[tokio::test]
async fn webhook_secrets_never_show() {
    let mut fixture = Fixture::new();
    let store = crate::webhook::store::Store::new(&fixture.home);
    let file = fixture.write("secrets/pr", "file-probe-hook-secret\n", 0o600);
    let mut by_env = crate::webhook::store::tests::route("env");
    by_env.secret = Some(Secret::Env("DOCTOR_HOOK_SECRET".into()));
    let mut by_file = crate::webhook::store::tests::route("file");
    by_file.secret = Some(Secret::File(file));
    let mut locked = store.lock().unwrap();
    locked.routes.extend([by_env, by_file]);
    locked.save().unwrap();
    drop(locked);
    fixture.set("DOCTOR_HOOK_SECRET", "env-probe-hook-secret");
    for value in ["env-probe-hook-secret", "file-probe-hook-secret"] {
        fixture.config(&format!("backend = \"{value}\"\n"));
        let report = fixture.run(false).await;
        expect(
            &report,
            "webhooks",
            "route env",
            Status::Ok,
            "env DOCTOR_HOOK_SECRET",
        );
        expect(
            &report,
            "webhooks",
            "route file",
            Status::Ok,
            "secret: file",
        );
        assert_hidden(&report, "probe-hook");
    }
}

#[tokio::test]
async fn folders_in_secrets_are_checked_inside() {
    let fixture = Fixture::new();
    fixture.write("secrets/gh/token", "x\n", 0o600);
    fixture.write("secrets/gh/deeper/open", "x\n", 0o644);
    let secrets = fixture.home.join("secrets");
    mode(&secrets, 0o700);
    mode(&secrets.join("gh"), 0o700);
    mode(&secrets.join("gh/deeper"), 0o755);
    // A link back up is looked at, not walked again.
    std::os::unix::fs::symlink(secrets.join("gh"), secrets.join("gh/up")).unwrap();

    let report = fixture.run(false).await;
    expect(
        &report,
        "state folder",
        "secrets/gh/",
        Status::Ok,
        "mode 700",
    );
    expect(
        &report,
        "state folder",
        "secrets/gh/token",
        Status::Ok,
        "mode 600",
    );
    expect(
        &report,
        "state folder",
        "secrets/gh/up/",
        Status::Ok,
        "mode 700",
    );
    assert!(!has(&report, "state folder", "secrets/gh/up/token"));
    expect(
        &report,
        "state folder",
        "secrets/gh/deeper/",
        Status::Fail,
        "mode 755",
    );
    expect(
        &report,
        "state folder",
        "secrets/gh/deeper/open",
        Status::Fail,
        "mode 644",
    );

    mode(&secrets.join("gh/deeper"), 0o700);
    mode(&secrets.join("gh/deeper/open"), 0o600);
    let report = fixture.run(false).await;
    let failed: Vec<&Check> = report
        .checks
        .iter()
        .filter(|check| check.area == "state folder" && check.status != Status::Ok)
        .collect();
    assert!(failed.is_empty(), "{}", report.text());
}

#[tokio::test]
async fn the_sandbox_is_tried_with_a_confined_child() {
    use crate::sandbox::Support;
    let fixture = Fixture::new();
    fixture.config("model = \"m\"\n");
    match Support::current() {
        Support::Abi(_) => {
            let report = fixture.run(false).await;
            expect(&report, "sandbox", "landlock", Status::Ok, "Landlock ABI");
            let config = fixture.home.join("config.toml");
            expect(
                &report,
                "sandbox",
                "test command",
                Status::Ok,
                &format!("could not read {}", config.display()),
            );
            // Reads narrowed past what sh needs to start.
            fixture.config("sandbox_read = \"/nonexistent\"\n");
            let report = fixture.run(false).await;
            expect(
                &report,
                "sandbox",
                "test command",
                Status::Fail,
                "cannot start a confined `sh`",
            );
        }
        Support::Unavailable(why) => eprintln!("real kernel part skipped: {why}"),
    }

    let none = Some(Support::Unavailable("no Landlock (fake)".into()));
    fixture.config("model = \"m\"\n");
    let report = fixture.run_landlock(false, none.clone()).await;
    expect(
        &report,
        "sandbox",
        "landlock",
        Status::Warn,
        "scheduled jobs and webhook runs",
    );
    assert!(!has(&report, "sandbox", "test command"));
    fixture.config("sandbox = \"require\"\n");
    let report = fixture.run_landlock(false, none.clone()).await;
    expect(
        &report,
        "sandbox",
        "landlock",
        Status::Fail,
        "no Landlock (fake); sandbox = require",
    );
    fixture.config("sandbox = \"off\"\n");
    let report = fixture.run_landlock(false, none.clone()).await;
    expect(
        &report,
        "sandbox",
        "landlock",
        Status::Skip,
        "sandbox = off",
    );
    fixture.config("sandbox_network = \"0\"\n");
    let report = fixture.run_landlock(false, Some(Support::Abi(3))).await;
    expect(
        &report,
        "sandbox",
        "network",
        Status::Warn,
        "this kernel has 3",
    );
    fixture.config("sandbox = \"strict\"\n");
    let report = fixture.run_landlock(false, none).await;
    expect(
        &report,
        "sandbox",
        "settings",
        Status::Fail,
        "JUNEES_SANDBOX must be auto, require or off",
    );
}

#[tokio::test]
async fn the_browser_program_and_its_version() {
    let mut fixture = Fixture::new();
    let report = fixture.run(false).await;
    expect(
        &report,
        "browser",
        "program",
        Status::Skip,
        "the browser tool is off",
    );

    // On, but nothing on PATH.
    fixture.set("JUNEES_BROWSER", "on");
    let report = fixture.run(false).await;
    expect(
        &report,
        "browser",
        "program",
        Status::Fail,
        "no browser found",
    );

    // A program on PATH that prints no version.
    fixture.program("chromium");
    let report = fixture.run(false).await;
    expect(
        &report,
        "browser",
        "program",
        Status::Fail,
        "--version` failed",
    );

    // The configured program, run with `--version` only.
    let browser = fake_claude(
        &fixture.root,
        "[ \"$1\" = --version ] && echo 'Chromium 145.0.7632.6' && exit 0",
    );
    fixture.set("JUNEES_BROWSER_PATH", &browser.display().to_string());
    fixture.set("JUNEES_BROWSER_NO_SANDBOX", "1");
    let report = fixture.run(false).await;
    expect(
        &report,
        "browser",
        "program",
        Status::Ok,
        &format!("Chromium 145.0.7632.6 ({})", browser.display()),
    );
    expect(&report, "browser", "program", Status::Ok, "sandbox off");

    // Off, with a path set: still looked at, but only a warning.
    fixture.set("JUNEES_BROWSER", "off");
    fixture.set("JUNEES_BROWSER_PATH", "/nonexistent/chrome");
    let report = fixture.run(false).await;
    expect(&report, "browser", "program", Status::Warn, "is not a file");
}
