//! The Landlock sandbox of bash's commands ([`crate::sandbox`]): the
//! setting, the kernel's support, and a tiny confined test command.

use std::path::Path;

use super::Report;
use crate::config_file::Settings;
use crate::sandbox::{Mode, NETWORK_ABI, Sandbox, Support, TRUNCATE_ABI, Trial};
use crate::tools::{SettingsFile, ToolSettings};

const AREA: &str = "sandbox";

/// Checks the sandbox settings and the kernel's `support`, and tries a
/// confined child in `work` (the folder bash would run in).
pub(super) fn check(report: &mut Report, settings: &Settings, support: Support, work: &Path) {
    let get = |key: &str| settings.get(key).map(|value| value.trim().to_string());
    let sandbox = match crate::sandbox::Settings::read(get) {
        Ok(sandbox) => sandbox,
        Err(err) => {
            report.fail(
                AREA,
                "settings",
                format!("{err:#}"),
                "fix sandbox, sandbox_write, sandbox_read or sandbox_network",
            );
            return;
        }
    };
    let mode = sandbox.mode();
    if mode == Mode::Off {
        report.skip(
            AREA,
            "landlock",
            "sandbox = off: bash commands run unconfined (the command rules still apply)",
        );
        return;
    }
    let explicit = sandbox.mode.is_some();
    match &support {
        Support::Abi(abi) => {
            report.ok(AREA, "landlock", support.describe());
            if *abi < TRUNCATE_ABI {
                let unattended = if explicit {
                    ""
                } else {
                    "; scheduled jobs and webhook runs (which default to sandbox = require) \
                     refuse every bash command"
                };
                report.problem(
                    mode == Mode::Require,
                    AREA,
                    "truncate",
                    format!(
                        "Landlock ABI {abi} cannot stop bash commands from emptying the \
                         settings file, mcp.json, the secrets and the data folders (truncation \
                         needs ABI {TRUNCATE_ABI}); they cannot read them (sandbox = require \
                         refuses every command){unattended}"
                    ),
                    "use a newer kernel (6.2 or later)",
                );
            }
            if !sandbox.network && *abi < NETWORK_ABI {
                report.problem(
                    mode == Mode::Require,
                    AREA,
                    "network",
                    format!(
                        "sandbox_network = 0 needs Landlock ABI {NETWORK_ABI}; this kernel has \
                         {abi}, so TCP stays open (sandbox = require refuses every command)"
                    ),
                    "use a newer kernel (6.7 or later), or set sandbox_network = 1",
                );
            }
        }
        Support::Unavailable(why) => {
            let detail = if explicit {
                format!("{why}; sandbox = {}", mode.name())
            } else {
                format!(
                    "{why}; bash commands run unconfined, and scheduled jobs and webhook runs \
                     (which default to sandbox = require) refuse them"
                )
            };
            report.problem(
                explicit && mode == Mode::Require,
                AREA,
                "landlock",
                detail,
                "use a kernel with Landlock (5.13 or later, `landlock` in the `lsm=` boot \
                 parameter), or set sandbox = auto (or off) to run commands unconfined",
            );
            return;
        }
    }
    let file = crate::config_file::path(get).map(|path| {
        SettingsFile::new(path, get("HOME").map(std::path::PathBuf::from))
            .with_config_home(get("XDG_CONFIG_HOME").map(std::path::PathBuf::from))
    });
    let tools = ToolSettings {
        settings_file: file,
        sandbox: Some(sandbox),
        ..ToolSettings::default()
    };
    let Some(sandbox) = Sandbox::for_tools(&tools) else {
        return;
    };
    match sandbox.with_support(support).try_child(work) {
        Trial::Confined { tried: Some(file) } => report.ok(
            AREA,
            "test command",
            format!(
                "a confined `sh` ran in {} and could not read {}",
                super::shown(work),
                super::shown(&file)
            ),
        ),
        Trial::Confined { tried: None } => report.ok(
            AREA,
            "test command",
            format!("a confined `sh` ran in {}", super::shown(work)),
        ),
        Trial::Unconfined => {}
        Trial::Failed(why) => report.fail(
            AREA,
            "test command",
            why,
            "check sandbox_read (it must cover /usr, /lib and /etc for bash to start) and \
             sandbox_write, or set sandbox = off",
        ),
    }
}
