//! `junees plugin list|enable|disable|install|remove`.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::manifest::{self, FILE_NAME, Manifest};
use super::state;
use crate::prompt_text::escape_hidden;

/// Files an install copies at most (a `.git` folder is left out).
pub const MAX_INSTALL_FILES: usize = 1000;
/// Bytes an install copies at most.
pub const MAX_INSTALL_BYTES: u64 = 100 * 1024 * 1024;

pub const USAGE: &str = "usage: junees plugin list | junees plugin install <folder> [--name <name>] \
                         | junees plugin enable <name> [--trust] | junees plugin disable <name> \
                         | junees plugin remove <name>";

/// A `junees plugin` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginCommand {
    List,
    /// Copies a plugin's folder into the plugins folder, disabled.
    Install {
        from: PathBuf,
        name: Option<String>,
    },
    /// Turns it on from the next run; `trust` lets its tools run without
    /// approval questions.
    Enable {
        name: String,
        trust: bool,
    },
    Disable(String),
    /// Disables it and deletes its folder.
    Remove(String),
}

/// The arguments after `junees plugin`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<PluginCommand> {
    let args: Vec<String> = args.into_iter().collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let name = |name: &str| -> Result<String> {
        manifest::check_name(name)?;
        Ok(name.to_string())
    };
    Ok(match words.as_slice() {
        [] | ["list"] => PluginCommand::List,
        ["install", from] => PluginCommand::Install {
            from: PathBuf::from(from),
            name: None,
        },
        ["install", from, "--name", new] | ["install", "--name", new, from] => {
            PluginCommand::Install {
                from: PathBuf::from(from),
                name: Some(name(new)?),
            }
        }
        ["enable", plugin] => PluginCommand::Enable {
            name: name(plugin)?,
            trust: false,
        },
        ["enable", plugin, "--trust"] | ["enable", "--trust", plugin] => PluginCommand::Enable {
            name: name(plugin)?,
            trust: true,
        },
        ["disable", plugin] => PluginCommand::Disable(name(plugin)?),
        ["remove", plugin] => PluginCommand::Remove(name(plugin)?),
        _ => bail!("{USAGE}"),
    })
}

/// Runs `command` on the state folder `home`; returns what to print.
pub fn run(command: &PluginCommand, home: Option<&Path>) -> Result<String> {
    let home = home.context(
        "neither JUNEES_HOME nor HOME is set, so there is no plugins folder ($JUNEES_HOME/plugins)",
    )?;
    let dir = super::dir(home);
    match command {
        PluginCommand::List => Ok(listing(&dir)),
        PluginCommand::Install { from, name } => install(&dir, from, name.as_deref()),
        PluginCommand::Enable { name, trust } => enable(&dir, name, *trust),
        PluginCommand::Disable(name) => {
            let mut state = state::load(&dir)?;
            let was = state.enabled.remove(name);
            state.trusted.remove(name);
            if !was {
                return Ok(format!("plugin {name} is not enabled\n"));
            }
            state::save(&dir, &state)?;
            Ok(format!(
                "disabled plugin {name}; it stays installed (junees plugin remove {name} \
                 deletes it)\n"
            ))
        }
        PluginCommand::Remove(name) => {
            let folder = dir.join(name);
            if !folder.is_dir() {
                bail!("no plugin {name} in {}", dir.display());
            }
            let mut state = state::load(&dir)?;
            if state.enabled.remove(name) | state.trusted.remove(name) {
                state::save(&dir, &state)?;
            }
            std::fs::remove_dir_all(&folder)
                .with_context(|| format!("cannot delete {}", folder.display()))?;
            Ok(format!("removed plugin {name} ({})\n", folder.display()))
        }
    }
}

/// Every installed plugin of the plugins folder `dir`: whether it is
/// enabled, and what it offers or why it cannot load.
pub fn listing(dir: &Path) -> String {
    let state = match state::load(dir) {
        Ok(state) => state,
        Err(err) => return format!("  {err:#}\n"),
    };
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| manifest::check_name(name).is_ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    let mut text = String::new();
    for name in &names {
        let enabled = state.enabled.contains(name);
        let mut line = format!("  {name}: {}", if enabled { "enabled" } else { "disabled" });
        match manifest::load(&dir.join(name)) {
            Ok(manifest) => {
                let plugin = super::Plugin {
                    name: name.clone(),
                    dir: dir.join(name),
                    trusted: state.trusted.contains(name),
                    manifest,
                };
                line.push_str(&format!(", {}", super::offers(&plugin)));
                if !plugin.manifest.description.is_empty() {
                    let description: String =
                        plugin.manifest.description.chars().take(200).collect();
                    line.push_str(&format!("\n     {}", escape_hidden(&description)));
                }
            }
            Err(err) => line.push_str(&format!(", cannot load: {err:#}")),
        }
        text.push_str(&line);
        text.push('\n');
    }
    for name in state.enabled.iter().filter(|name| !names.contains(name)) {
        text.push_str(&format!(
            "  {}: enabled, but not installed\n",
            escape_hidden(name)
        ));
    }
    if text.is_empty() {
        text = format!(
            "  (no plugins in {}; `junees plugin install <folder>` adds one)\n",
            dir.display()
        );
    }
    text
}

fn enable(dir: &Path, name: &str, trust: bool) -> Result<String> {
    let folder = dir.join(name);
    if !folder.is_dir() {
        bail!(
            "no plugin {name} in {}; `junees plugin install <folder>` adds one",
            dir.display()
        );
    }
    let manifest = manifest::load(&folder).with_context(|| format!("plugin {name}"))?;
    let mut state = state::load(dir)?;
    state.enabled.insert(name.to_string());
    if trust {
        state.trusted.insert(name.to_string());
    }
    state::save(dir, &state)?;
    let mut text = format!("enabled plugin {name}; the next junees run uses it\n");
    text.push_str(&runs(&folder, &manifest));
    if trust {
        text.push_str("its tools run without approval questions (--trust)\n");
    } else if manifest.trust {
        text.push_str(&recommends_trust(name));
    }
    Ok(text)
}

/// What install and enable say about a manifest's `"trust": true`: a
/// recommendation of the plugin's author, which trusts nothing itself.
fn recommends_trust(name: &str) -> String {
    format!(
        "its plugin.json recommends trusting it (\"trust\": true); its tools still ask \
         before they run unless you enable it with `junees plugin enable {name} --trust`\n"
    )
}

/// The programs a plugin runs, for the user to check.
fn runs(folder: &Path, manifest: &Manifest) -> String {
    let mut text = String::new();
    let mut line = |what: String, command: &manifest::Command| {
        let mut words = vec![command.program(folder).display().to_string()];
        words.extend(command.argv[1..].iter().cloned());
        text.push_str(&format!("  {what}: {}\n", escape_hidden(&words.join(" "))));
    };
    for tool in &manifest.tools {
        line(format!("tool {}", tool.name), &tool.command);
    }
    for hook in &manifest.pre_tool {
        line("pre_tool hook".into(), hook);
    }
    for hook in &manifest.post_turn {
        line("post_turn hook".into(), hook);
    }
    text
}

fn install(dir: &Path, from: &Path, name: Option<&str>) -> Result<String> {
    let source =
        std::fs::canonicalize(from).with_context(|| format!("cannot read {}", from.display()))?;
    if !source.is_dir() {
        bail!("{} is not a folder", from.display());
    }
    let manifest = manifest::read(&source.join(FILE_NAME))?;
    let name = match name {
        Some(name) => name.to_string(),
        None => {
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            manifest::check_name(&name).context("name the plugin with --name")?;
            name
        }
    };
    let target = dir.join(&name);
    if std::fs::symlink_metadata(&target).is_ok() {
        bail!(
            "a plugin named {name} is already installed ({}); `junees plugin remove {name}` \
             first, or install it with --name",
            target.display()
        );
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    if std::fs::canonicalize(dir).is_ok_and(|dir| dir.starts_with(&source)) {
        bail!("cannot install a folder that holds the plugins folder itself");
    }
    let temp = dir.join(format!(".install-{name}-{}", std::process::id()));
    let copied = (|| -> Result<()> {
        let mut budget = Budget::default();
        copy_folder(&source, &temp, &mut budget)?;
        // The copy is checked as it will be read.
        manifest::load(&temp)?;
        std::fs::rename(&temp, &target)
            .with_context(|| format!("cannot move the copy to {}", target.display()))
    })();
    if let Err(err) = copied {
        if temp.exists() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        return Err(err);
    }
    let mut text = format!(
        "installed plugin {name} in {} (disabled: `junees plugin enable {name}` turns it on)\n",
        target.display()
    );
    text.push_str(&runs(&target, &manifest));
    if manifest.trust {
        text.push_str(&recommends_trust(&name));
    }
    Ok(text)
}

/// What an install may still copy.
#[derive(Default)]
struct Budget {
    files: usize,
    bytes: u64,
}

/// Copies the folder `from` to the new folder `to`: folders 0700, files
/// with only their owner's permission bits (plus read and write), the
/// manifest 0600. Links and special files are refused: a link could
/// point anywhere later.
fn copy_folder(from: &Path, to: &Path, budget: &mut Budget) -> Result<()> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(to)
        .with_context(|| format!("cannot create {}", to.display()))?;
    let mut entries: Vec<_> = std::fs::read_dir(from)
        .with_context(|| format!("cannot read {}", from.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("cannot read {}", from.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let target = to.join(&name);
        if metadata.is_dir() {
            copy_folder(&path, &target, budget)?;
        } else if metadata.is_file() {
            budget.files += 1;
            budget.bytes += metadata.len();
            if budget.files > MAX_INSTALL_FILES || budget.bytes > MAX_INSTALL_BYTES {
                bail!(
                    "the plugin is too large to install (more than {MAX_INSTALL_FILES} files or \
                     100 MiB)"
                );
            }
            let mode = if to.join(FILE_NAME) == target {
                0o600
            } else {
                (metadata.permissions().mode() & 0o700) | 0o600
            };
            let mut input = std::fs::File::open(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&target)
                .with_context(|| format!("cannot write {}", target.display()))?;
            std::io::copy(&mut input, &mut output)
                .with_context(|| format!("cannot copy {}", path.display()))?;
            // The umask may have taken bits away.
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
            output.sync_all()?;
        } else {
            bail!(
                "{} is a link or a special file; a plugin folder may hold only folders and \
                 regular files",
                path.display()
            );
        }
    }
    Ok(())
}
