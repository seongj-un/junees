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
//! - [`assets`]: the page itself, `web/dashboard/`, built into the binary.

mod actions;
mod api;
mod assets;
pub mod server;
mod session_view;

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
Usage: junees dashboard [--listen 127.0.0.1:7878] [--no-open]

Serves a read-only dashboard of this Junees (settings, sessions, usage,
scheduled jobs, webhooks, doctor) to the browser of this computer, until
Ctrl-C.

  --listen <ip:port>  where to listen; a loopback address only
                      (127.0.0.1 or ::1). Port 0 picks a free one.
  --no-open           print the address without opening a browser

The address it prints holds a token made for this run; the page needs it.
Anyone who can read the address can read the dashboard. On a computer
other users log in to, use --no-open: the opened browser's command line,
which they can list, holds the address.
";

/// The arguments of `junees dashboard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub listen: SocketAddr,
    pub open: bool,
}

/// Reads the arguments after `dashboard`; `None` for `--help`.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>> {
    let mut listen = None;
    let mut open = true;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--no-open" => open = false,
            "--listen" => {
                let value = args
                    .next()
                    .context("--listen needs an address (127.0.0.1:7878)")?;
                listen = Some(listen_address(&value)?);
            }
            other => match other.strip_prefix("--listen=") {
                Some(value) => listen = Some(listen_address(value)?),
                None => bail!("unknown argument `{other}`; see `junees dashboard --help`"),
            },
        }
    }
    let listen = match listen {
        Some(listen) => listen,
        None => listen_address(DEFAULT_LISTEN)?,
    };
    Ok(Some(Options { listen, open }))
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

/// The address the browser opens: the dashboard with the token after `#`,
/// which the browser does not send to the server or put in `Referer`.
pub fn page_url(address: SocketAddr, token: &str) -> String {
    let host = match address.ip() {
        IpAddr::V4(ip) => format!("{ip}:{}", address.port()),
        IpAddr::V6(ip) => format!("[{ip}]:{}", address.port()),
    };
    format!("http://{host}/#token={token}")
}

/// A new random token for one run.
pub fn new_token() -> Result<String> {
    crate::mcp::oauth::pkce::random_text(TOKEN_BYTES).map_err(anyhow::Error::msg)
}

/// Runs `junees dashboard` until Ctrl-C.
pub async fn main(options: Options) -> Result<ExitCode> {
    let token = new_token()?;
    let listener = tokio::net::TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("cannot listen on {}", options.listen))?;
    let address = listener
        .local_addr()
        .context("cannot read the address it listens on")?;
    let url = page_url(address, &token);
    // Not println!: it panics when stdout is closed (`| head -1`, a closed
    // terminal), and the server goes on without anyone reading it.
    let mut out = std::io::stdout();
    if writeln!(out, "Junees dashboard: {url}")
        .and_then(|()| {
            writeln!(
                out,
                "Read-only. The address holds a token for this run; Ctrl-C stops it."
            )
        })
        .and_then(|()| out.flush())
        .is_err()
    {
        eprintln!("Cannot print the dashboard's address; it serves on {address}.");
    }
    if options.open {
        open_browser(&url);
    }
    let env: Env = Arc::new(|key: &str| std::env::var(key).ok());
    let context = Arc::new(Context::new(token, address.ip(), address.port(), env));
    tokio::select! {
        served = serve(listener, context) => {
            served.context("the dashboard stopped serving")?;
        }
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(ExitCode::SUCCESS)
}

/// Opens `url` in the default browser, if a way to is found; the address
/// is printed either way.
fn open_browser(url: &str) {
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
        Ok(_) => {}
        Err(_) => eprintln!("Cannot run `{opener}`; open the address above in a browser."),
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
            })
        );
        assert_eq!(
            parse(args(&["--listen", "[::1]:0", "--no-open"])).unwrap(),
            Some(Options {
                listen: "[::1]:0".parse().unwrap(),
                open: false,
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
