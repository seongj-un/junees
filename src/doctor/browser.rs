//! The `browser` tool's program: found, and `--version` (offline: Chrome
//! prints its version and exits); and a profiles folder too long to hold
//! Chrome's socket.

use std::ffi::OsStr;

use crate::browser::{ENABLE_ENV, PATH_ENV, Settings, process};
use crate::config_file::Settings as SettingsFile;

use super::{Report, shown};

const AREA: &str = "browser";

pub(super) async fn check(
    report: &mut Report,
    settings: &SettingsFile,
    path: Option<&OsStr>,
    support: &crate::sandbox::Support,
) {
    let browser = match Settings::read(|key| settings.get(key)) {
        Ok(browser) => browser,
        // The settings check already names the bad value.
        Err(_) => return,
    };
    // A bad sandbox setting is the sandbox check's to report.
    if let Ok(sandbox) = crate::sandbox::Settings::read(|key| {
        settings.get(key).map(|value| value.trim().to_string())
    }) && let Some(warning) =
        crate::browser::unconfined_warning(&browser, Some(&sandbox), support)
    {
        report.warn(
            AREA,
            "pipe",
            warning,
            "use a kernel with Landlock (Linux 5.13 or later), or set sandbox = auto",
        );
    }
    if !browser.enabled && browser.path.is_none() {
        report.skip(
            AREA,
            "program",
            format!("the browser tool is off (browser = on, {ENABLE_ENV}, turns it on)"),
        );
        return;
    }
    if let Some(profiles) = browser.profiles.as_deref().filter(|_| browser.enabled)
        && !process::profiles_hold_socket(profiles)
    {
        report.warn(
            AREA,
            "profiles",
            format!(
                "the profiles folder {} is longer than {} bytes: Chrome's socket and temporary \
                 files go in a separate temporary folder, which sandboxed commands can reach",
                shown(profiles),
                process::MAX_PROFILES_FOLDER
            ),
            "use a shorter JUNEES_HOME",
        );
    }
    let fix = format!("install Chromium or Chrome, or point browser_path ({PATH_ENV}) at it");
    let program = match process::find(&browser, path) {
        Ok(program) => program,
        Err(err) => {
            report.problem(browser.enabled, AREA, "program", format!("{err:#}"), fix);
            return;
        }
    };
    match process::version(&program).await {
        Ok(version) => {
            let mut detail = format!("{version} ({})", shown(&program));
            if !browser.enabled {
                detail.push_str("; the browser tool is off");
            }
            if browser.no_sandbox {
                detail.push_str("; Chrome's sandbox off (browser_no_sandbox)");
            }
            report.ok(AREA, "program", detail);
        }
        Err(err) => report.problem(browser.enabled, AREA, "program", format!("{err:#}"), fix),
    }
}
