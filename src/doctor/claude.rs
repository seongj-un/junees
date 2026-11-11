//! What the claude-code backend needs: the `claude` program, its sign-in
//! (`--online` only), and for the environment scrub bubblewrap and user
//! namespaces it may create.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::claude_code::{BIN_ENV, Backend, ClaudeCode, SCRUB_ENV, find_on_path};
use crate::config_file::Settings;
use crate::tools::ToolSettings;

use super::settings::Read;
use super::{Probe, Report, shown};

const AREA: &str = "claude-code";

/// The user namespace switches of the kernel that keep an unprivileged
/// bubblewrap from working: (file under `/proc/sys`, the value that
/// blocks, the name to show).
const USERNS_SWITCHES: &[(&str, &str, &str)] = &[
    (
        "kernel/apparmor_restrict_unprivileged_userns",
        "1",
        "kernel.apparmor_restrict_unprivileged_userns = 1 (AppArmor)",
    ),
    (
        "kernel/unprivileged_userns_clone",
        "0",
        "kernel.unprivileged_userns_clone = 0",
    ),
    (
        "user/max_user_namespaces",
        "0",
        "user.max_user_namespaces = 0",
    ),
];

pub(super) async fn check(
    report: &mut Report,
    settings: &Settings,
    read: &Read,
    path: Option<&OsStr>,
    probe: &Probe,
    online: bool,
) {
    let needed = read.backend == Backend::ClaudeCode;
    let claude = ClaudeCode::from_lookup(|key| settings.get(key));
    let found = locate(&claude.bin, path);
    if !needed && settings.get(BIN_ENV).is_none() && found.is_none() {
        report.skip(
            AREA,
            "claude",
            "not used (the backend is api) and `claude` is not on PATH",
        );
        return;
    }
    let tools = read
        .config
        .as_ref()
        .map_or_else(ToolSettings::default, |config| config.tools.clone());
    let fix = format!(
        "install Claude Code (https://code.claude.com/docs/en/setup), or point claude_bin \
         ({BIN_ENV}) at it"
    );
    match claude.version(Path::new("/"), &tools).await {
        Ok(version) => {
            let at = found.as_deref().unwrap_or(&claude.bin);
            report.ok(AREA, "claude", format!("{version} ({})", shown(at)));
        }
        Err(err) => report.problem(needed, AREA, "claude", format!("{err:#}"), fix),
    }
    if online {
        match claude.check_auth(&tools).await {
            Ok(()) => report.ok(AREA, "sign-in", "signed in with the Claude subscription"),
            Err(err) => report.problem(
                needed,
                AREA,
                "sign-in",
                format!("{err:#}"),
                "run `claude`, then /login (or `claude setup-token`)",
            ),
        }
    } else {
        report.skip(
            AREA,
            "sign-in",
            "not checked offline (`junees doctor --online` runs `claude auth status`)",
        );
    }
    if !claude.scrub {
        report.warn(
            AREA,
            "env scrub",
            format!(
                "{SCRUB_ENV} is off: the commands Claude Code runs see its environment, \
                 CLAUDE_CODE_OAUTH_TOKEN included if you set it"
            ),
            format!("unset {SCRUB_ENV} (Junees then sets it to 1) once bubblewrap works"),
        );
        return;
    }
    report.ok(
        AREA,
        "env scrub",
        format!(
            "on: Junees sets {SCRUB_ENV}=1, so Claude Code hides its credentials from the \
             commands it runs"
        ),
    );
    if cfg!(target_os = "linux") {
        sandbox(report, needed, path, &probe.sysctl);
    }
}

/// bubblewrap, and whether it may create a user namespace.
fn sandbox(report: &mut Report, needed: bool, path: Option<&OsStr>, sysctl: &Path) {
    let bwrap = find_on_path("bwrap", path);
    let setuid = bwrap
        .as_ref()
        .and_then(|bwrap| std::fs::metadata(bwrap).ok())
        .is_some_and(|metadata| metadata.permissions().mode() & 0o4000 != 0);
    match &bwrap {
        Some(bwrap) => report.ok(
            AREA,
            "bubblewrap",
            format!(
                "{}{}",
                shown(bwrap),
                if setuid { " (setuid root)" } else { "" }
            ),
        ),
        None => report.problem(
            needed,
            AREA,
            "bubblewrap",
            format!(
                "`bwrap` is not on PATH: with {SCRUB_ENV}=1 Claude Code needs bubblewrap on \
                 Linux and refuses to start without it, so the scrub cannot run and \
                 claude-code sessions fail"
            ),
            format!(
                "install bubblewrap (Debian/Ubuntu: `sudo apt install bubblewrap`), or set \
                 {SCRUB_ENV}=0 to run without the scrub (the commands Claude Code runs then \
                 see its credentials)"
            ),
        ),
    }
    let limits = userns_limits(sysctl);
    if limits.is_empty() {
        report.ok(
            AREA,
            "user namespaces",
            "unprivileged user namespaces are allowed (bubblewrap can create its own)",
        );
    } else if setuid {
        report.ok(
            AREA,
            "user namespaces",
            format!(
                "restricted ({}), but bwrap is setuid root and needs none",
                limits.join(", ")
            ),
        );
    } else {
        report.warn(
            AREA,
            "user namespaces",
            format!(
                "restricted: {}. An unprivileged bwrap cannot create the user namespace it \
                 needs, so the scrub fails even with bubblewrap installed",
                limits.join(", ")
            ),
            format!(
                "let bwrap create user namespaces (AppArmor: a profile for bwrap with \
                 `userns,`; README, 점검), then try `bwrap --ro-bind / / true`; or set \
                 {SCRUB_ENV}=0"
            ),
        );
    }
}

/// The switches of [`USERNS_SWITCHES`] that are set to block, under
/// `sysctl` (`/proc/sys`).
pub(super) fn userns_limits(sysctl: &Path) -> Vec<&'static str> {
    USERNS_SWITCHES
        .iter()
        .filter(|(file, blocking, _)| {
            std::fs::read_to_string(sysctl.join(file)).is_ok_and(|value| value.trim() == *blocking)
        })
        .map(|&(_, _, name)| name)
        .collect()
}

/// Where `bin` is: itself when it names a path, else on `path`.
fn locate(bin: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    if bin.components().count() > 1 {
        return bin.exists().then(|| bin.to_path_buf());
    }
    find_on_path(&bin.to_string_lossy(), path)
}
