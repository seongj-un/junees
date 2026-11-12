//! `junees update` and `/update`: replace the binary with the latest
//! release from GitHub.
//!
//! [`fetch`] asks for the latest release and downloads its files (GitHub's
//! hosts only, size-capped), [`install`] checks the archive against
//! `SHA256SUMS`, takes the binary out and swaps it in with one `rename`.
//! What the update acts on (the program's path, the state folder, the
//! version) comes in a [`Place`], so tests replace a fake binary in a
//! temporary folder, never the running one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub mod fetch;
pub mod install;
#[cfg(test)]
mod tests;
pub mod version;

pub use fetch::{BASE_ENV, Client, Source};
pub use version::Version;

pub const USAGE: &str = "usage: junees update [--check] [--version <vX.Y.Z>]";

pub const HELP: &str = "\
junees update [--check] [--version <vX.Y.Z>]

Replaces this junees binary with the latest release, when that is newer
(https://github.com/seongj-un/junees/releases):
  1. asks GitHub which release is the latest (no account needed);
  2. downloads junees-<version>-<target>.tar.gz for this computer and
     SHA256SUMS (from github.com and GitHub's file hosts only);
  3. stops unless the archive's SHA-256 matches SHA256SUMS;
  4. writes the new binary next to the old one and renames it over it,
     so the old binary stays whole until the new one is in place (a link
     is followed to the file it points at).
Gateways, the cron daemon and the webhook server keep running the old
binary until they are restarted; the update says which ones run.

  --check             only say whether a newer release is out (also
                      /update in the conversation and `junees doctor
                      --online`)
  --version <vX.Y.Z>  install that release instead, even an older one

Release builds exist for Linux (x86_64, aarch64; static musl) and macOS
(x86_64, aarch64).
";

/// What `junees update` was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Only say whether a newer release is out.
    pub check: bool,
    /// Install this release instead of the latest.
    pub version: Option<Version>,
}

/// The arguments after `junees update`; `None` for `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--check" => options.check = true,
            "--version" => {
                let value = args
                    .next()
                    .with_context(|| format!("--version needs a version ({USAGE})"))?;
                options.version = Some(Version::parse(&value).map_err(anyhow::Error::msg)?);
            }
            other => bail!(
                "unknown argument `{}` for update ({USAGE})",
                crate::prompt_text::escape_hidden(other)
            ),
        }
    }
    if options.check && options.version.is_some() {
        bail!("--check and --version do not go together ({USAGE})");
    }
    Ok(Some(options))
}

/// What an update acts on.
#[derive(Debug, Clone)]
pub struct Place {
    /// The program to replace.
    pub exe: PathBuf,
    /// The version it is.
    pub current: Version,
    /// The release build that fits this computer.
    pub target: Option<&'static str>,
    /// The state folder, to see which Junees processes run.
    pub home: Option<PathBuf>,
    /// `cargo install`'s folder, to say when the binary came from there.
    pub cargo_bin: Option<PathBuf>,
}

impl Place {
    /// This running program.
    pub fn current(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot find the running junees program")?;
        let cargo_bin = lookup("CARGO_HOME")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                lookup("HOME")
                    .filter(|value| !value.trim().is_empty())
                    .map(|home| Path::new(&home).join(".cargo"))
            })
            .map(|cargo| cargo.join("bin"));
        Ok(Self {
            exe,
            current: this_version(),
            target: install::this_target(),
            home: crate::config_file::home_dir(lookup),
            cargo_bin,
        })
    }
}

/// The version of this build.
pub fn this_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is a version")
}

/// The latest release against `current`, in one line.
pub fn compare(current: &Version, latest: Option<&Version>) -> String {
    match latest {
        None => format!("no release of Junees is published yet (this is {current})"),
        Some(latest) if latest > current => {
            format!("junees {latest} is out (this is {current}): run `junees update`")
        }
        Some(latest) if latest == current => {
            format!("junees {current} is the latest release")
        }
        Some(latest) => {
            format!("this junees ({current}) is newer than the latest release ({latest})")
        }
    }
}

/// `junees update --check` and `/update`: whether a newer release is out.
pub async fn check(client: &Client, current: &Version) -> Result<String> {
    Ok(compare(current, client.latest().await?.as_ref()))
}

/// `junees update`: installs the latest release (or `options.version`)
/// at `place` when it differs. `say` gets progress lines; the result is
/// the closing message.
pub async fn run(
    options: &Options,
    client: &Client,
    place: &Place,
    say: &mut dyn FnMut(&str),
) -> Result<String> {
    if options.check {
        return check(client, &place.current).await;
    }
    let wanted = match &options.version {
        Some(version) => {
            if *version == place.current {
                return Ok(format!("junees {version} is already installed"));
            }
            if *version < place.current {
                say(&format!(
                    "{version} is older than this junees ({}); installing it as asked",
                    place.current
                ));
            }
            version.clone()
        }
        None => match client.latest().await? {
            Some(latest) if latest > place.current => latest,
            latest => return Ok(compare(&place.current, latest.as_ref())),
        },
    };
    let Some(target) = place.target else {
        bail!(
            "there is no release build for {}/{}; build it from source with `cargo install \
             --git https://github.com/seongj-un/junees`",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let stem = install::stem(&wanted, target);
    let archive_name = format!("{stem}.tar.gz");
    say(&format!(
        "downloading {archive_name} from {}",
        client.source().describe()
    ));
    let sums = client
        .download(&wanted, install::SUMS_FILE, install::MAX_SUMS_BYTES)
        .await?;
    let expected = install::checksum_of(&sums, &archive_name)?;
    let archive = client
        .download(&wanted, &archive_name, install::MAX_ARCHIVE_BYTES)
        .await?;
    install::verify(&archive_name, &archive, &expected)?;
    let exe = place.exe.clone();
    let member = format!("{stem}/junees");
    let path = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let binary = install::extract(&archive, &member)?;
        install::replace(&exe, &binary)
    })
    .await
    .context("the update stopped")??;
    let mut out = format!(
        "junees {} -> {wanted}: replaced {}",
        place.current,
        path.display()
    );
    for note in install::notes(&path, place.cargo_bin.as_deref(), place.home.as_deref()) {
        out.push_str("\n  ");
        out.push_str(&note);
    }
    Ok(out)
}
