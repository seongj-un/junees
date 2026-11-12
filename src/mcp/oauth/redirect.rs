//! Receiving the authorization response: a one-time HTTP listener on
//! `127.0.0.1` at a port the system picks (RFC 8252 section 7.3, loopback
//! redirection), or the address the browser ended on, pasted by the user
//! when the browser runs on another computer.
//!
//! MCP 2026-07-28, security-considerations.md, "Open Redirection": the
//! client SHOULD use and verify `state` and discard any result without it
//! or with another one. A request with another `state` is answered with an
//! error page and forgotten; the first one with the right `state` ends
//! the wait, and the listener closes with it.

use std::time::Duration;

use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The path of the redirect URI.
pub const PATH: &str = "/callback";
/// How long one browser request may take to arrive.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest request head read.
const MAX_HEAD: usize = 16 * 1024;

/// The query parameters of an authorization response.
#[derive(Default)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn parse(query: &str) -> Self {
        let mut url = Url::parse("http://query.invalid/").expect("valid");
        url.set_query(Some(query));
        Self(url.query_pairs().into_owned().collect())
    }
}

pub struct Listener {
    listener: TcpListener,
    redirect_uri: String,
}

impl Listener {
    /// Listens on `127.0.0.1` at a free port.
    pub async fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|err| format!("cannot listen on 127.0.0.1 for the browser's answer: {err}"))?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("cannot read the listening port: {err}"))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}{PATH}"),
        })
    }

    /// `http://127.0.0.1:<port>/callback`.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Answers requests until one to the callback path carries `state`,
    /// and returns its parameters.
    pub async fn wait(&self, state: &str) -> Params {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                // Out of descriptors, say: try again in a moment.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            if let Ok(Some(params)) =
                tokio::time::timeout(READ_TIMEOUT, answer(stream, state)).await
            {
                return params;
            }
        }
    }
}

/// Reads one request and answers it; the parameters if it is the
/// response with `state`.
async fn answer(mut stream: TcpStream, state: &str) -> Option<Params> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || head.len() + n > MAX_HEAD {
            return None;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    let line = String::from_utf8_lossy(&head);
    let target = line
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_default()
        .to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let (status, page, params) = if path != PATH {
        ("404 Not Found", "Not found.", None)
    } else {
        let params = Params::parse(query);
        if params.get("state") == Some(state) {
            (
                "200 OK",
                "Junees received the answer. You can close this tab and go back to the terminal.",
                Some(params),
            )
        } else {
            (
                "400 Bad Request",
                "This answer does not belong to the login junees is waiting for.",
                None,
            )
        }
    };
    let body =
        format!("<!doctype html><meta charset=\"utf-8\"><title>Junees</title><p>{page}</p>\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    params
}

/// The parameters of a pasted answer: the whole address the browser ended
/// on (the redirect URI and its query), or just the query.
pub fn pasted(text: &str, redirect_uri: &str) -> Result<Params, String> {
    let text = text.trim();
    let wrong = || {
        format!(
            "that is not the address the browser ended on; paste the whole address (it starts \
             with {redirect_uri}?)"
        )
    };
    if text.starts_with("http://") || text.starts_with("https://") {
        let url = Url::parse(text).map_err(|_| wrong())?;
        let mut base = url.clone();
        base.set_query(None);
        base.set_fragment(None);
        if base.as_str() != redirect_uri {
            return Err(wrong());
        }
        return Ok(Params::parse(url.query().unwrap_or_default()));
    }
    let query = text.strip_prefix('?').unwrap_or(text);
    let params = Params::parse(query);
    if params.get("state").is_none() {
        return Err(wrong());
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(uri: &str) -> String {
        let url = Url::parse(uri).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap()))
            .await
            .unwrap();
        let target = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_string(),
        };
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn waits_for_the_answer_with_the_right_state_only() {
        let listener = Listener::bind().await.unwrap();
        let uri = listener.redirect_uri().to_string();
        assert!(uri.starts_with("http://127.0.0.1:") && uri.ends_with("/callback"));
        let waiting = tokio::spawn(async move { listener.wait("good-state").await.0 });
        let other = get(&uri.replace("/callback", "/elsewhere?state=good-state")).await;
        assert!(other.starts_with("HTTP/1.1 404"), "{other}");
        let forged = get(&format!("{uri}?code=evil&state=bad-state")).await;
        assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
        let missing = get(&format!("{uri}?code=evil")).await;
        assert!(missing.starts_with("HTTP/1.1 400"), "{missing}");
        assert!(!waiting.is_finished());
        let good = get(&format!("{uri}?code=c%2B1&state=good-state&iss=x")).await;
        assert!(good.starts_with("HTTP/1.1 200"), "{good}");
        let params = Params(waiting.await.unwrap());
        assert_eq!(params.get("code"), Some("c+1"));
        assert_eq!(params.get("iss"), Some("x"));
    }

    #[tokio::test]
    async fn the_listener_answers_once_and_closes() {
        let listener = Listener::bind().await.unwrap();
        let uri = listener.redirect_uri().to_string();
        let waiting = tokio::spawn(async move { listener.wait("s").await });
        get(&format!("{uri}?code=1&state=s")).await;
        waiting.await.unwrap();
        let port = Url::parse(&uri).unwrap().port().unwrap();
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }

    #[test]
    fn pasted_answers_are_the_redirect_address_or_its_query() {
        let uri = "http://127.0.0.1:4321/callback";
        let params = pasted(&format!("  {uri}?code=abc&state=s1 \n"), uri)
            .ok()
            .unwrap();
        assert_eq!(params.get("code"), Some("abc"));
        assert_eq!(params.get("state"), Some("s1"));
        let params = pasted("?code=abc&state=s1", uri).ok().unwrap();
        assert_eq!(params.get("code"), Some("abc"));
        assert!(pasted("code=abc&state=s1", uri).is_ok());
        for bad in [
            "http://127.0.0.1:9999/callback?code=a&state=s",
            "https://evil.example/callback?code=a&state=s",
            "http://127.0.0.1:4321/other?code=a&state=s",
            "abc",
            "code=abc",
        ] {
            let message = pasted(bad, uri).err().unwrap();
            assert!(message.contains(uri), "{bad}: {message}");
        }
    }
}
