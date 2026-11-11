//! Programs (git, gh), the scheduled jobs and their daemon, the webhook
//! routes and their server, the MCP servers' commands and the enabled
//! plugins.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::claude_code::find_on_path;
use crate::config_file::Settings;
use crate::plugins::Plugins;
use crate::webhook::store::Secret;

use super::{Report, shown};

/// git and gh on PATH.
pub(super) fn programs(report: &mut Report, path: Option<&OsStr>) {
    const AREA: &str = "programs";
    match find_on_path("git", path) {
        Some(git) => report.ok(AREA, "git", shown(&git)),
        None => report.warn(
            AREA,
            "git",
            "`git` is not on PATH: task mode, its checkpoints and the GitHub flows need it",
            "install git",
        ),
    }
    match find_on_path("gh", path) {
        Some(gh) => report.ok(AREA, "gh", shown(&gh)),
        None => report.skip(
            AREA,
            "gh",
            "not on PATH (optional: the agent's commands may use it; Junees's github tool \
             does not)",
        ),
    }
}

/// The jobs of `junees cron` and whether a daemon runs them.
pub(super) fn cron(report: &mut Report, home: &Path) {
    const AREA: &str = "cron";
    let path = crate::cron::store::Store::new(home).path();
    let jobs = match crate::cron::store::read(&path) {
        Ok(jobs) => jobs,
        Err(err) => {
            report.fail(
                AREA,
                "jobs",
                format!("{err:#}"),
                format!(
                    "fix {} (or move it away and add the jobs again)",
                    shown(&path)
                ),
            );
            return;
        }
    };
    let waiting = jobs
        .iter()
        .filter(|job| !job.paused && job.next_run.is_some())
        .count();
    let running = crate::cron::daemon::is_running(home);
    let detail = format!(
        "{} jobs, {waiting} waiting to run; the daemon is {}",
        jobs.len(),
        if running { "running" } else { "not running" }
    );
    if waiting > 0 && !running {
        report.warn(
            AREA,
            "daemon",
            detail,
            "start `junees cron daemon` (or its systemd unit; README, 예약 작업); the jobs \
             wait until one runs",
        );
    } else if jobs.is_empty() && !running {
        report.skip(AREA, "daemon", "no scheduled jobs");
    } else {
        report.ok(AREA, "daemon", detail);
    }
}

/// The webhook routes, their secrets (there and private, never shown)
/// and whether a server answers them.
pub(super) fn webhooks(report: &mut Report, home: &Path, settings: &Settings) {
    const AREA: &str = "webhooks";
    let path = crate::webhook::store::Store::new(home).path();
    let routes = match crate::webhook::store::read(&path) {
        Ok(routes) => routes,
        Err(err) => {
            report.fail(
                AREA,
                "routes",
                format!("{err:#}"),
                format!(
                    "fix {} (or move it away and add the routes again)",
                    shown(&path)
                ),
            );
            return;
        }
    };
    for route in &routes {
        let name = format!("route {}", route.name);
        match &route.secret {
            None => report.ok(
                AREA,
                &name,
                "no secret: unsigned, answered for requests from this machine only",
            ),
            Some(Secret::Env(var)) if settings.env(var).is_none() => report.warn(
                AREA,
                &name,
                format!("secret: {var} is not set here (the server's environment may differ)"),
                format!(
                    "set {var} in the environment of `junees webhook serve` (a 0600 \
                     EnvironmentFile; README, 웹훅)"
                ),
            ),
            Some(secret) => match secret.read(&|key| settings.env(key)) {
                Ok(_) => report.ok(AREA, &name, format!("secret: {}", secret.describe())),
                Err(err) => report.fail(
                    AREA,
                    &name,
                    format!("secret {}: {err:#}", secret.describe()),
                    match secret {
                        Secret::File(file) => {
                            format!("write the secret to {} (yours, chmod 600)", shown(file))
                        }
                        Secret::Env(var) => format!("give {var} a value"),
                    },
                ),
            },
        }
    }
    let active = routes.iter().filter(|route| !route.paused).count();
    let serving = crate::webhook::is_serving(home);
    let detail = format!(
        "{} routes, {active} not paused; the server is {}",
        routes.len(),
        if serving { "running" } else { "not running" }
    );
    if active > 0 && !serving {
        report.warn(
            AREA,
            "server",
            detail,
            "start `junees webhook serve` (or its systemd unit; README, 웹훅); requests \
             meanwhile get no answer",
        );
    } else if routes.is_empty() && !serving {
        report.skip(AREA, "server", "no webhook routes");
    } else {
        report.ok(AREA, "server", detail);
    }
}

/// The servers of mcp.json: the file reads, and each enabled server's
/// command is there (none is started).
pub(super) fn mcp(report: &mut Report, home: &Path, path: Option<&OsStr>) {
    const AREA: &str = "mcp";
    let file = home.join(crate::mcp::config::FILE_NAME);
    if !file.exists() {
        report.skip(AREA, "mcp.json", "no MCP servers (no mcp.json)");
        return;
    }
    let servers = match crate::mcp::config::load(&file) {
        Ok(servers) => servers,
        Err(err) => {
            report.fail(
                AREA,
                "mcp.json",
                format!("{err:#}"),
                format!("fix {} (README, MCP 서버)", shown(&file)),
            );
            return;
        }
    };
    if servers.is_empty() {
        report.skip(AREA, "mcp.json", "lists no servers");
    }
    for server in servers {
        let name = format!("server {}", server.name);
        if !server.enabled {
            report.skip(AREA, &name, "disabled");
            continue;
        }
        if let Some(cwd) = &server.cwd
            && !cwd.is_dir()
        {
            report.fail(
                AREA,
                &name,
                format!("its cwd {} is not a folder", shown(cwd)),
                "create that folder, or change `cwd` in mcp.json",
            );
            continue;
        }
        let command = &server.command;
        let found = if server.command.contains('/') {
            let program = server.program();
            std::fs::metadata(&program)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                .then_some(program)
        } else {
            // A PATH of its own in `env` is the one the server is looked up in.
            let own = server.env.get("PATH").map(OsStr::new);
            find_on_path(&server.command, own.or(path))
        };
        match found {
            Some(program) => report.ok(AREA, &name, format!("`{command}` is {}", shown(&program))),
            None => report.fail(
                AREA,
                &name,
                format!("`{command}` is not an executable file or on PATH; it was not started"),
                "install it, give `command` as an absolute path, or set `\"enabled\": false`",
            ),
        }
    }
}

/// The enabled plugins: their manifests and prompt files read.
pub(super) fn plugins(report: &mut Report, home: &Path) {
    const AREA: &str = "plugins";
    let plugins = Plugins::load(Some(home), &[], Path::new("/"));
    for problem in plugins.problems() {
        report.fail(
            AREA,
            "enabled",
            problem.clone(),
            "fix the plugin (`junees plugin list` shows them), or `junees plugin disable \
             <name>`",
        );
    }
    for plugin in plugins.plugins() {
        report.ok(
            AREA,
            &plugin.name,
            format!(
                "enabled, manifest valid ({})",
                crate::plugins::offers(plugin)
            ),
        );
    }
    if plugins.plugins().is_empty() && plugins.problems().is_empty() {
        report.skip(AREA, "enabled", "no plugin is enabled");
    }
}
