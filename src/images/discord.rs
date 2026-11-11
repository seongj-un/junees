//! Pictures attached to Discord messages: which attachments are images,
//! and downloading them from Discord's CDN.
//!
//! Only Discord's own CDN hosts are fetched from, over https, and a
//! redirect is followed only to them (at most [`MAX_REDIRECTS`]); the
//! message's attachment list comes from the gateway, but a link could
//! still point anywhere. Each download has a time limit and stops at
//! [`MAX_IMAGE_BYTES`]. Errors name the file, never the URL (Discord signs
//! attachment URLs with query parameters).

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Url;
use reqwest::redirect::Policy;
use serde_json::Value;

use super::{Image, MAX_IMAGE_BYTES, human_bytes, too_large};
use crate::prompt_text::escape_hidden;

/// The hosts Discord serves attachments from (message object, attachment
/// `url` and `proxy_url`).
pub const CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];

/// Another place attachments may come from: only for tests and proxies,
/// so not a key of the settings file. https, or http on this machine.
pub const CDN_URL_ENV: &str = "JUNEES_DISCORD_CDN_URL";

/// Attachments of one message that are read; Discord allows 10.
pub const MAX_ATTACHMENTS: usize = 10;

/// Redirects followed for one download.
pub const MAX_REDIRECTS: usize = 3;

/// Time one download may take.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// An image attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The file name as sent, with hidden characters escaped.
    pub filename: String,
    pub url: String,
    /// Bytes, as Discord reports them.
    pub size: u64,
}

/// The image attachments of a message (`attachments` of a MESSAGE_CREATE),
/// and a note for each one left out: not an image, too large, past
/// [`MAX_ATTACHMENTS`].
pub fn attachments(message: &Value) -> (Vec<Attachment>, Vec<String>) {
    let mut images = Vec::new();
    let mut notes = Vec::new();
    let list = message
        .get("attachments")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for item in list {
        let text = |key: &str| item.get(key).and_then(Value::as_str);
        let filename = escape_hidden(text("filename").unwrap_or("attachment"));
        let image = match text("content_type") {
            Some(kind) => kind.starts_with("image/"),
            None => has_image_extension(&filename),
        };
        if !image {
            notes.push(format!(
                "`{filename}` is not an image; only images are read"
            ));
            continue;
        }
        let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
        if size > MAX_IMAGE_BYTES {
            notes.push(format!("`{filename}`: {}", too_large(size)));
            continue;
        }
        let Some(url) = text("url") else {
            notes.push(format!("`{filename}` has no download link"));
            continue;
        };
        if images.len() == MAX_ATTACHMENTS {
            notes.push(format!(
                "`{filename}` left out: at most {MAX_ATTACHMENTS} images per message"
            ));
            continue;
        }
        images.push(Attachment {
            filename,
            url: url.to_string(),
            size,
        });
    }
    (images, notes)
}

fn has_image_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Downloads attachments from the allowed places.
pub struct Fetcher {
    client: reqwest::Client,
    extra: Option<Url>,
}

impl Fetcher {
    /// A fetcher for Discord's CDN, and `extra` (a base URL, from
    /// [`CDN_URL_ENV`]) if given.
    pub fn new(extra: Option<&str>) -> Result<Self> {
        let extra = match extra {
            Some(base) => {
                crate::gateway::config::check_url(base, "https", "http").context(CDN_URL_ENV)?;
                Some(Url::parse(base).context(CDN_URL_ENV)?)
            }
            None => None,
        };
        let allowed = extra.clone();
        let policy = Policy::custom(move |attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if is_allowed(attempt.url(), allowed.as_ref()) {
                attempt.follow()
            } else {
                attempt.error("redirected away from Discord's CDN")
            }
        });
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(policy)
            .build()
            .context("cannot set up the HTTP client for attachments")?;
        Ok(Self { client, extra })
    }

    /// Whether `url` may be downloaded.
    pub fn allows(&self, url: &str) -> bool {
        Url::parse(url).is_ok_and(|url| is_allowed(&url, self.extra.as_ref()))
    }

    /// The attachment as an image block, or what went wrong, in words.
    pub async fn fetch(&self, attachment: &Attachment) -> Result<Image, String> {
        let name = &attachment.filename;
        if !self.allows(&attachment.url) {
            return Err(format!("`{name}` is not on Discord's CDN; not downloaded"));
        }
        let failed =
            |err: reqwest::Error| format!("`{name}` could not be downloaded: {}", describe(err));
        let mut response = self
            .client
            .get(&attachment.url)
            .send()
            .await
            .map_err(failed)?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("`{name}` could not be downloaded: HTTP {status}"));
        }
        if let Some(len) = response.content_length()
            && len > MAX_IMAGE_BYTES
        {
            return Err(format!("`{name}`: {}", too_large(len)));
        }
        let bytes = match read_capped(&mut response, MAX_IMAGE_BYTES).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(len)) => return Err(format!("`{name}`: {}", too_large(len))),
            Err(err) => return Err(failed(err)),
        };
        let (image, _) = Image::from_bytes(&bytes).map_err(|err| format!("`{name}`: {err}"))?;
        Ok(image)
    }
}

/// A response body read chunk by chunk.
trait Body {
    type Error;
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, Self::Error>;
}

impl Body for reqwest::Response {
    type Error = reqwest::Error;

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, reqwest::Error> {
        Ok(self.chunk().await?.map(|chunk| chunk.to_vec()))
    }
}

/// The whole body, or `Err(bytes read)` as soon as more than `limit`
/// bytes came, without reading on: a server need not say the length,
/// and the length it says is checked apart.
async fn read_capped<B: Body>(body: &mut B, limit: u64) -> Result<Result<Vec<u8>, u64>, B::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > limit {
            return Ok(Err(bytes.len() as u64));
        }
    }
    Ok(Ok(bytes))
}

fn is_allowed(url: &Url, extra: Option<&Url>) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let cdn = url.scheme() == "https"
        && url.port().is_none()
        && url.host_str().is_some_and(|host| CDN_HOSTS.contains(&host));
    cdn || extra.is_some_and(|base| {
        url.scheme() == base.scheme()
            && url.host_str() == base.host_str()
            && url.port_or_known_default() == base.port_or_known_default()
            && url.path().starts_with(base.path())
    })
}

/// A download error without its URL.
fn describe(err: reqwest::Error) -> String {
    if err.is_timeout() {
        format!("timed out after {}s", TIMEOUT.as_secs())
    } else if err.is_redirect() {
        "a redirect away from Discord's CDN, or too many".to_string()
    } else if err.is_connect() {
        "cannot connect".to_string()
    } else {
        "the connection failed".to_string()
    }
}

/// Downloads `attachments` one after the other: the images that came, and
/// a note for each that did not.
pub async fn fetch_all(fetcher: &Fetcher, attachments: &[Attachment]) -> (Vec<Image>, Vec<String>) {
    let mut images = Vec::new();
    let mut notes = Vec::new();
    for attachment in attachments {
        match fetcher.fetch(attachment).await {
            Ok(image) => images.push(image),
            Err(note) => notes.push(note),
        }
    }
    (images, notes)
}

/// How a received image is named in the log: format and size only.
pub fn log_line(image: &Image) -> String {
    let bytes = image.bytes().map_or(0, |bytes| bytes.len() as u64);
    format!("{} ({})", image.describe(), human_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::images::testing::png;

    #[test]
    fn picks_the_image_attachments() {
        let message = json!({"attachments": [
            {"filename": "shot.png", "content_type": "image/png", "size": 100,
             "url": "https://cdn.discordapp.com/attachments/1/2/shot.png?ex=1&hm=2"},
            {"filename": "notes.txt", "content_type": "text/plain", "size": 10,
             "url": "https://cdn.discordapp.com/attachments/1/3/notes.txt"},
            {"filename": "huge.jpg", "content_type": "image/jpeg", "size": MAX_IMAGE_BYTES + 1,
             "url": "https://cdn.discordapp.com/attachments/1/4/huge.jpg"},
            {"filename": "old.GIF", "size": 5,
             "url": "https://media.discordapp.net/attachments/1/5/old.GIF"},
            {"filename": "bad\u{202e}gnp.exe", "content_type": "application/x-msdownload",
             "size": 5, "url": "https://cdn.discordapp.com/x"},
            {"filename": "nolink.png", "content_type": "image/png", "size": 5}
        ]});
        let (images, notes) = attachments(&message);
        assert_eq!(
            images
                .iter()
                .map(|a| a.filename.as_str())
                .collect::<Vec<_>>(),
            ["shot.png", "old.GIF"]
        );
        assert_eq!(images[0].size, 100);
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert!(notes[0].contains("notes.txt` is not an image"));
        assert!(notes[1].contains("huge.jpg`") && notes[1].contains("up to 5 MB"));
        assert!(notes[2].contains("\\u{202e}"), "{}", notes[2]);
        assert!(notes[3].contains("no download link"));
        assert_eq!(attachments(&json!({"content": "hi"})), (vec![], vec![]));

        let many: Vec<Value> = (0..12)
            .map(|i| {
                json!({"filename": format!("{i}.png"), "content_type": "image/png",
                            "size": 1, "url": "https://cdn.discordapp.com/a"})
            })
            .collect();
        let (images, notes) = attachments(&json!({ "attachments": many }));
        assert_eq!(images.len(), MAX_ATTACHMENTS);
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn only_discord_cdn_urls_are_allowed() {
        let fetcher = Fetcher::new(None).unwrap();
        for url in [
            "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1",
            "https://media.discordapp.net/attachments/1/2/a.png",
        ] {
            assert!(fetcher.allows(url), "{url}");
        }
        for url in [
            "http://cdn.discordapp.com/attachments/1/2/a.png",
            "https://cdn.discordapp.com:8443/a.png",
            "https://cdn.discordapp.com.evil.example/a.png",
            "https://evil.example/cdn.discordapp.com/a.png",
            "https://user@cdn.discordapp.com/a.png",
            "https://discord.com/api/v10/x",
            "file:///etc/passwd",
            "http://127.0.0.1:1/a.png",
            "not a url",
        ] {
            assert!(!fetcher.allows(url), "{url}");
        }
        let fetcher = Fetcher::new(Some("http://127.0.0.1:4000/cdn/")).unwrap();
        assert!(fetcher.allows("http://127.0.0.1:4000/cdn/a.png"));
        assert!(!fetcher.allows("http://127.0.0.1:4001/cdn/a.png"));
        assert!(!fetcher.allows("http://127.0.0.1:4000/other/a.png"));
        assert!(fetcher.allows("https://cdn.discordapp.com/a.png"));
        assert!(Fetcher::new(Some("http://cdn.example/")).is_err());
    }

    /// A one-request-per-connection HTTP server on loopback that answers
    /// each path from `routes` (status, extra headers, body); a path not
    /// listed gets 404. Returns its base URL and the paths it was asked.
    async fn fake_cdn(
        routes: Vec<(&'static str, u16, String, Vec<u8>)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = socket.read(&mut buf).await else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let (status, headers, body) = routes
                    .iter()
                    .find(|(p, ..)| *p == path)
                    .map(|(_, s, h, b)| (*s, h.clone(), b.clone()))
                    .unwrap_or((404, String::new(), Vec::new()));
                // `x-no-length` leaves the length out: the body ends when
                // the connection closes.
                let length = if headers.contains("x-no-length") {
                    String::new()
                } else {
                    format!("content-length: {}\r\n", body.len())
                };
                let reply =
                    format!("HTTP/1.1 {status} X\r\n{length}connection: close\r\n{headers}\r\n");
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        (base, seen)
    }

    fn attachment(base: &str, path: &str) -> Attachment {
        Attachment {
            filename: path.trim_start_matches('/').to_string(),
            url: format!("{base}{path}?hm=signed"),
            size: 1,
        }
    }

    #[tokio::test]
    async fn downloads_images_and_refuses_the_rest() {
        let image = png(30, 20);
        let mut big = png(1, 1);
        big.resize(MAX_IMAGE_BYTES as usize + 10, 0);
        let (base, seen) = fake_cdn(vec![
            ("/cdn/a.png?hm=signed", 200, String::new(), image.clone()),
            (
                "/cdn/text.png?hm=signed",
                200,
                String::new(),
                b"hello".to_vec(),
            ),
            ("/cdn/big.png?hm=signed", 200, String::new(), big.clone()),
            (
                "/cdn/unsized.png?hm=signed",
                200,
                "x-no-length: 1\r\n".into(),
                big,
            ),
            ("/cdn/gone.png?hm=signed", 404, String::new(), Vec::new()),
            (
                "/cdn/away.png?hm=signed",
                302,
                "location: http://127.0.0.2:9/x.png\r\n".into(),
                Vec::new(),
            ),
            (
                "/cdn/hop.png?hm=signed",
                302,
                "location: /cdn/a.png?hm=signed\r\n".into(),
                Vec::new(),
            ),
            (
                "/cdn/loop.png?hm=signed",
                302,
                "location: /cdn/loop.png?hm=signed\r\n".into(),
                Vec::new(),
            ),
        ])
        .await;
        let fetcher = Fetcher::new(Some(&format!("{base}/cdn/"))).unwrap();

        let got = fetcher
            .fetch(&attachment(&base, "/cdn/a.png"))
            .await
            .unwrap();
        assert_eq!(got.bytes().unwrap(), image);
        assert_eq!(log_line(&got), "image/png, 30x20 px (45 B)");
        // A redirect within the allowed place is followed.
        let hop = fetcher
            .fetch(&attachment(&base, "/cdn/hop.png"))
            .await
            .unwrap();
        assert_eq!(hop, got);

        let err = |path: &'static str| {
            let fetcher = &fetcher;
            let base = base.clone();
            async move { fetcher.fetch(&attachment(&base, path)).await.unwrap_err() }
        };
        assert!(err("/cdn/text.png").await.contains("text.png`: not a PNG"));
        assert!(err("/cdn/big.png").await.contains("up to 5 MB"));
        // Without a length too.
        assert!(err("/cdn/unsized.png").await.contains("up to 5 MB"));
        assert!(err("/cdn/gone.png").await.ends_with("HTTP 404 Not Found"));
        let away = err("/cdn/away.png").await;
        assert!(away.contains("redirect"), "{away}");
        assert!(err("/cdn/loop.png").await.contains("redirect"));
        // Outside the allowed place nothing is asked.
        let before = seen.lock().unwrap().len();
        let outside = err("/elsewhere/a.png").await;
        assert!(outside.contains("not on Discord's CDN"), "{outside}");
        assert_eq!(seen.lock().unwrap().len(), before);
        // No error shows the signed URL.
        for path in ["/cdn/gone.png", "/cdn/away.png", "/elsewhere/a.png"] {
            let text = err(path).await;
            assert!(
                !text.contains("hm=signed") && !text.contains(&base),
                "{text}"
            );
        }

        let (images, notes) = fetch_all(
            &fetcher,
            &[
                attachment(&base, "/cdn/a.png"),
                attachment(&base, "/cdn/gone.png"),
            ],
        )
        .await;
        assert_eq!(images.len(), 1);
        assert_eq!(notes.len(), 1);
    }

    /// `chunks` chunks of 1 MiB, counting how many were read.
    struct Chunks {
        left: usize,
        read: usize,
    }

    impl Body for Chunks {
        type Error = ();
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ()> {
            if self.left == 0 {
                return Ok(None);
            }
            self.left -= 1;
            self.read += 1;
            Ok(Some(vec![0; 1024 * 1024]))
        }
    }

    #[tokio::test]
    async fn reading_stops_at_the_limit() {
        // Far more than the limit: reading stops right after passing it.
        let mut body = Chunks { left: 64, read: 0 };
        let read = read_capped(&mut body, MAX_IMAGE_BYTES).await.unwrap();
        assert_eq!(read, Err(6 * 1024 * 1024));
        assert_eq!(body.read, 6);
        // Up to the limit the whole body comes back.
        let mut body = Chunks { left: 5, read: 0 };
        let read = read_capped(&mut body, MAX_IMAGE_BYTES).await.unwrap();
        assert_eq!(read.unwrap().len(), 5 * 1024 * 1024);
    }

    #[tokio::test]
    async fn a_silent_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(socket);
        });
        let mut fetcher = Fetcher::new(Some(&base)).unwrap();
        fetcher.client = reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let err = fetcher
            .fetch(&attachment(&base, "/a.png"))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        held.abort();
    }
}
