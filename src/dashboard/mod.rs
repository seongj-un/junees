//! `junees dashboard`: a web page about this Junees (settings, sessions
//! and their conversations, usage, scheduled jobs, webhooks, doctor),
//! served on a loopback address to the browser of this computer. Its only
//! writes pause and resume jobs and routes.
//!
//! - [`server`]: the HTTP side and its guards (bearer token, `Host`,
//!   `Origin`, size and time limits, security headers).
//! - [`api`]: the JSON of `/api/*`, built from the modules that own the
//!   data. The contract is in `docs/dashboard.md`.
//! - [`session_view`]: one session's conversation, a page at a time.
//! - [`actions`]: pause and resume, logged to `dashboard.log`.
//! - [`code`]: the one-time code an opened browser trades for the token.
//! - [`assets`]: the page itself, `web/dashboard/`, built into the binary.

mod actions;
mod api;
mod assets;
pub mod chat;
mod code;
#[cfg(test)]
mod page_files;
pub mod server;
pub(crate) mod session_view;

use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};

pub use server::{Context, Env, serve};

/// Where the dashboard listens without `--listen`.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7878";

/// Random bytes in the token: 32, which make 43 base64url characters.
const TOKEN_BYTES: usize = 32;

pub const HELP: &str = "\
Usage: junees dashboard [--listen 127.0.0.1:7878] [--no-open | --open] [--chat [--dir <folder>]]

Serves a dashboard of this Junees (settings, sessions and their
conversations, usage, scheduled jobs, webhooks, doctor) to the browser of
this computer, until Ctrl-C. Its only changes pause and resume scheduled
jobs and webhook routes.

  --listen <ip:port>  where to listen; a loopback address only
                      (127.0.0.1 or ::1). Port 0 picks a free one.
  --no-open           print an address to paste instead of opening a
                      browser (the default with --chat)
  --open              open a browser with --chat too (see below)
  --chat              also talk with Junees on the page, approval questions
                      answered with its buttons: the page then has the
                      power of a terminal conversation (api backend only)
  --dir <folder>      where the chat works (default: the current folder)

The page needs a token made for this run. An opened browser gets a code
that trades for it once, within a minute, and keeps the token in that tab
only; the token itself is printed only with --no-open, or when no browser
can be started. Anyone who can read that address can use the
dashboard.

With --chat no browser is opened unless --open is given: the one-time
code on a browser's command line can be read and traded by another
process of this computer before the browser does, and with the chat the
token runs commands.
";

/// The arguments of `junees dashboard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub listen: SocketAddr,
    pub open: bool,
    /// `--chat`: the chat API ([`chat`]) is served.
    pub chat: bool,
    /// `--dir`: where the chat works; `None` is the current folder.
    pub dir: Option<std::path::PathBuf>,
}

/// Reads the arguments after `dashboard`; `None` for `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>> {
    let mut listen = None;
    let mut open = None;
    let mut chat = false;
    let mut dir = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--no-open" => open = Some(false),
            "--open" => open = Some(true),
            "--chat" => chat = true,
            "--dir" => {
                let value = args.next().context("--dir needs a folder")?;
                dir = Some(std::path::PathBuf::from(value));
            }
            "--listen" => {
                let value = args
                    .next()
                    .context("--listen needs an address (127.0.0.1:7878)")?;
                listen = Some(listen_address(&value)?);
            }
            other => {
                if let Some(value) = other.strip_prefix("--listen=") {
                    listen = Some(listen_address(value)?);
                } else if let Some(value) = other.strip_prefix("--dir=") {
                    dir = Some(std::path::PathBuf::from(value));
                } else {
                    bail!("unknown argument `{other}`; see `junees dashboard --help`");
                }
            }
        }
    }
    let listen = match listen {
        Some(listen) => listen,
        None => listen_address(DEFAULT_LISTEN)?,
    };
    if dir.is_some() && !chat {
        bail!("--dir is the folder of the chat: use it with --chat");
    }
    Ok(Some(Options {
        listen,
        // The chat's token runs commands: no code on a command line unless
        // asked for.
        open: open.unwrap_or(!chat),
        chat,
        dir,
    }))
}

/// `text` as an address to listen on: an IP address and a port, on
/// loopback only, since the dashboard shows what only this user should
/// see.
pub fn listen_address(text: &str) -> Result<SocketAddr> {
    let address: SocketAddr = text.trim().parse().with_context(|| {
        format!("`{text}` is not an IP address and port (127.0.0.1:7878 or [::1]:7878)")
    })?;
    if !address.ip().is_loopback() {
        bail!(
            "the dashboard listens on a loopback address only (127.0.0.1 or ::1), not {}",
            address.ip()
        );
    }
    Ok(address)
}

/// `http://<address>/`.
pub fn base_url(address: SocketAddr) -> String {
    let host = match address.ip() {
        IpAddr::V4(ip) => format!("{ip}:{}", address.port()),
        IpAddr::V6(ip) => format!("[{ip}]:{}", address.port()),
    };
    format!("http://{host}/")
}

/// The page with `name=value` after `#`, which the browser does not send
/// to the server or put in `Referer`.
fn fragment_url(address: SocketAddr, name: &str, value: &str) -> String {
    format!("{}#{name}={value}", base_url(address))
}

/// The address to paste: the page with the token after `#`.
pub fn page_url(address: SocketAddr, token: &str) -> String {
    fragment_url(address, "token", token)
}

/// A new random token for one run.
pub fn new_token() -> Result<String> {
    crate::mcp::oauth::pkce::random_text(TOKEN_BYTES).map_err(anyhow::Error::msg)
}

/// Runs `junees dashboard` until Ctrl-C; with `chat` (`--chat`, set up
/// by the binary) the chat API too.
pub async fn main(options: Options, chat: Option<chat::Setup>) -> Result<ExitCode> {
    let token = new_token()?;
    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("cannot listen on {}", options.listen))?;
    let address = listener
        .local_addr()
        .context("cannot read the address it listens on")?;
    let env: Env = Arc::new(|key: &str| std::env::var(key).ok());
    let chat = chat.map(chat::Chat::start);
    let mut context = Context::new(token.clone(), address.ip(), address.port(), env);
    if let Some(chat) = &chat {
        chat.log_started();
        context = context.with_chat(Arc::clone(chat));
    }
    let context = Arc::new(context);
    // Not println!: it panics when stdout is closed (`| head -1`, a closed
    // terminal), and the server goes on without anyone reading it.
    let mut out = std::io::stdout();
    // The browser's command line, which other processes can read, gets a
    // one-time code; the token is printed only when no browser was started.
    if options.open && chat.is_some() {
        eprintln!(
            "  !! --open with --chat: the one-time code on the browser's command line can be read \
             by other processes of this computer and traded before the browser does; whoever \
             trades it can run commands through the chat. Without --open the address is printed \
             here only."
        );
    }
    let opened = options.open && {
        let code = new_token()?;
        if let Ok(mut codes) = context.codes.lock() {
            codes.issue(code.clone(), std::time::Instant::now());
        }
        let started = open_browser(&fragment_url(address, "code", &code));
        if !started && let Ok(mut codes) = context.codes.lock() {
            // On no command line: the code ends, the token is printed.
            codes.clear();
        }
        started
    };
    let printed = if opened {
        writeln!(
            out,
            "Junees dashboard: opening {} in the browser; Ctrl-C stops it.",
            base_url(address)
        )
        .and_then(|()| {
            writeln!(
                out,
                "If no browser opens within a minute, stop it and run `junees dashboard --no-open` \
                 for an address to paste."
            )
        })
    } else {
        writeln!(out, "Junees dashboard: {}", page_url(address, &token)).and_then(|()| {
            writeln!(
                out,
                "The address holds a token for this run; Ctrl-C stops it."
            )
        })
    };
    if let Some(chat) = &chat {
        let _ = writeln!(
            out,
            "The chat works in {}: the page can do what a terminal conversation there can.",
            chat.dir().display()
        );
    }
    if printed.and_then(|()| out.flush()).is_err() {
        eprintln!("Cannot print the dashboard's address; it serves on {address}.");
    }
    let served = tokio::select! {
        served = serve(listener, context) => served.context("the dashboard stopped serving"),
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    if let Some(chat) = &chat {
        chat.shutdown().await;
    }
    served?;
    Ok(ExitCode::SUCCESS)
}

/// Opens `url` in the default browser, if a way to is found; the address
/// is printed either way.
/// Whether the opener started: `xdg-open` (`open` on macOS) with `url`.
/// When it cannot run (no desktop, an SSH session), says so; the caller
/// prints an address to paste instead.
fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let spawned = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        // Not waited for: xdg-open may stay with the browser it starts.
        // The child is reaped when Junees exits.
        Ok(_) => true,
        Err(_) => {
            eprintln!("Cannot run `{opener}`; printing an address to paste instead.");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn parses_the_arguments() {
        assert_eq!(
            parse(args(&[])).unwrap(),
            Some(Options {
                listen: "127.0.0.1:7878".parse().unwrap(),
                open: true,
                chat: false,
                dir: None,
            })
        );
        assert_eq!(
            parse(args(&["--listen", "[::1]:0", "--no-open"])).unwrap(),
            Some(Options {
                listen: "[::1]:0".parse().unwrap(),
                open: false,
                chat: false,
                dir: None,
            })
        );
        assert_eq!(
            parse(args(&["--listen=127.0.0.2:9000"]))
                .unwrap()
                .unwrap()
                .listen,
            "127.0.0.2:9000".parse().unwrap()
        );
        assert_eq!(parse(args(&["--help"])).unwrap(), None);
        let chat = parse(args(&["--chat", "--dir", "/w"])).unwrap().unwrap();
        assert!(chat.chat);
        assert_eq!(chat.dir.as_deref(), Some(std::path::Path::new("/w")));
        assert!(!chat.open, "--chat prints the address");
        assert!(parse(args(&["--chat", "--open"])).unwrap().unwrap().open);
        assert!(parse(args(&["--open"])).unwrap().unwrap().open);
        assert!(parse(args(&["--dir=/w"])).is_err());
        assert!(parse(args(&["--chat", "--dir"])).is_err());
        assert!(parse(args(&["--listen"])).is_err());
        assert!(parse(args(&["--port", "1"])).is_err());
    }

    #[test]
    fn refuses_addresses_off_loopback() {
        for address in [
            "0.0.0.0:7878",
            "[::]:7878",
            "192.168.1.2:7878",
            "8.8.8.8:80",
        ] {
            let err = listen_address(address).unwrap_err().to_string();
            assert!(err.contains("loopback"), "{address}: {err}");
        }
        for address in ["localhost:7878", "127.0.0.1", "7878", ""] {
            assert!(listen_address(address).is_err(), "{address}");
        }
    }

    #[test]
    fn the_page_url_carries_the_token_after_the_hash() {
        let token = new_token().unwrap();
        assert_eq!(token.len(), 43);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_ne!(token, new_token().unwrap());
        assert_eq!(
            page_url("127.0.0.1:7878".parse().unwrap(), "abc"),
            "http://127.0.0.1:7878/#token=abc"
        );
        assert_eq!(
            page_url("[::1]:9".parse().unwrap(), "abc"),
            "http://[::1]:9/#token=abc"
        );
    }
}
