//! The `browser` tool's program: found, and `--version` (offline: Chrome
//! prints its version and exits).

use std::ffi::OsStr;

use crate::browser::{ENABLE_ENV, PATH_ENV, Settings, process};
use crate::config_file::Settings as SettingsFile;

use super::{Report, shown};

const AREA: &str = "browser";

pub(super) async fn check(report: &mut Report, settings: &SettingsFile, path: Option<&OsStr>) {
    let browser = match Settings::read(|key| settings.get(key)) {
        Ok(browser) => browser,
        // The settings check already names the bad value.
        Err(_) => return,
    };
    if !browser.enabled && browser.path.is_none() {
        report.skip(
            AREA,
            "program",
            format!("the browser tool is off (browser = on, {ENABLE_ENV}, turns it on)"),
        );
        return;
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
