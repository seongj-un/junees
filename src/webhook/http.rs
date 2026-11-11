//! A small HTTP/1.1 receiver for webhook requests: the request line, the
//! headers and a `Content-Length` body, nothing else. Chunked bodies are
//! refused (411), the head may take [`MAX_HEAD`] bytes (431) and the body
//! [`MAX_BODY`] (413), every connection closes after one answer (no
//! keep-alive). The caller puts a time limit around the reading.
//!
//! Senders of webhooks (GitHub and the like) only send small `POST`
//! requests with a length; a full HTTP server (`hyper`) would add more
//! code to trust than this.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The request line and the headers together.
pub const MAX_HEAD: usize = 16 * 1024;
/// The body.
pub const MAX_BODY: usize = 1024 * 1024;

/// The request line and headers of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    /// The path, without a query.
    pub path: String,
    /// Names in lower case, in the order sent.
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// An answer for a request that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub status: u16,
    pub message: &'static str,
}

fn error(status: u16, message: &'static str) -> Error {
    Error { status, message }
}

/// The reason phrase of `status`.
pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Reads the head of a request. Bytes read past it (the start of the
/// body) are left in `buf`. Each read is searched for the end of the head
/// from where the last search stopped, so a client sending one byte at a
/// time costs no more than one sending it all at once.
pub async fn read_head<R: AsyncRead + Unpin>(io: &mut R, buf: &mut Vec<u8>) -> Result<Head, Error> {
    let mut chunk = [0u8; 4096];
    // Where the search goes on: the end may start in the last 3 bytes.
    let mut searched = 0;
    let end = loop {
        if let Some(found) = buf[searched..].windows(4).position(|w| w == b"\r\n\r\n") {
            break searched + found;
        }
        searched = buf.len().saturating_sub(3);
        if buf.len() >= MAX_HEAD {
            return Err(error(431, "the request head is longer than 16 KiB"));
        }
        let n = io
            .read(&mut chunk)
            .await
            .map_err(|_| error(400, "cannot read the request"))?;
        if n == 0 {
            return Err(error(400, "the request ended early"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if end + 4 > MAX_HEAD {
        return Err(error(431, "the request head is longer than 16 KiB"));
    }
    let head = parse_head(&buf[..end])?;
    buf.drain(..end + 4);
    Ok(head)
}

fn parse_head(bytes: &[u8]) -> Result<Head, Error> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| error(400, "the request head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let parts: Vec<&str> = line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(error(400, "malformed request line"));
    };
    if !version.starts_with("HTTP/1.")
        || method.is_empty()
        || !method.bytes().all(|b| b.is_ascii_uppercase())
        || !target.starts_with('/')
    {
        return Err(error(400, "malformed request line"));
    }
    let path = target
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    let mut headers = Vec::new();
    for line in lines {
        // A continuation line (obsolete folding) or a bare line is refused.
        let Some((name, value)) = line.split_once(':') else {
            return Err(error(400, "malformed header"));
        };
        if name.is_empty() || !name.bytes().all(is_token) {
            return Err(error(400, "malformed header"));
        }
        headers.push((
            name.to_ascii_lowercase(),
            value.trim_matches([' ', '\t']).to_string(),
        ));
    }
    Ok(Head {
        method: method.to_string(),
        path,
        headers,
    })
}

/// A character of a header name (RFC 9110 `tchar`).
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The length of the body `head` announces: a `Content-Length` of at most
/// [`MAX_BODY`], and no `Transfer-Encoding`.
pub fn body_length(head: &Head) -> Result<usize, Error> {
    if head.header("transfer-encoding").is_some() {
        return Err(error(
            411,
            "chunked bodies are not accepted; send a Content-Length",
        ));
    }
    let lengths: Vec<&str> = head
        .headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .map(|(_, value)| value.as_str())
        .collect();
    let Some(&first) = lengths.first() else {
        return Err(error(411, "a Content-Length is required"));
    };
    if lengths.iter().any(|&value| value != first)
        || first.is_empty()
        || first.len() > 19
        || !first.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(error(400, "malformed Content-Length"));
    }
    let length: usize = first
        .parse()
        .map_err(|_| error(400, "malformed Content-Length"))?;
    if length > MAX_BODY {
        return Err(error(413, "the body is larger than 1 MiB"));
    }
    Ok(length)
}

/// Reads the body of `length` bytes ([`body_length`]) `head` announces,
/// after the bytes already in `buf`. Answers `Expect: 100-continue` before
/// reading, so the caller checks what it can before calling this.
pub async fn read_body<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    head: &Head,
    length: usize,
    mut buf: Vec<u8>,
) -> Result<Vec<u8>, Error> {
    if head
        .header("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
        && buf.len() < length
    {
        io.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|_| error(400, "cannot answer"))?;
    }
    let mut chunk = [0u8; 8192];
    while buf.len() < length {
        let n = io
            .read(&mut chunk)
            .await
            .map_err(|_| error(400, "cannot read the request"))?;
        if n == 0 {
            return Err(error(400, "the request ended early"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    // Anything after the body is ignored: the connection closes.
    buf.truncate(length);
    Ok(buf)
}

/// Writes a plain-text answer and closes the connection's sending side.
pub async fn respond<W: AsyncWrite + Unpin>(io: &mut W, status: u16, body: &str) {
    let _ = io.write_all(render(status, body).as_bytes()).await;
    let _ = io.shutdown().await;
}

/// A whole plain-text answer.
pub fn render(status: u16, body: &str) -> String {
    let body = format!("{body}\n");
    let mut text = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\n\
         connection: close\r\n",
        reason(status),
        body.len()
    );
    if status == 405 {
        text.push_str("allow: POST\r\n");
    }
    text.push_str("\r\n");
    text.push_str(&body);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads a whole request from `input` (written up front, then closed).
    async fn read(input: &[u8]) -> (Result<(Head, Vec<u8>), Error>, Vec<u8>) {
        let (mut client, mut server) = tokio::io::duplex(4 * MAX_BODY);
        client.write_all(input).await.unwrap();
        client.shutdown().await.unwrap();
        let mut buf = Vec::new();
        let result = match read_head(&mut server, &mut buf).await {
            Ok(head) => match body_length(&head) {
                Ok(length) => read_body(&mut server, &head, length, buf)
                    .await
                    .map(|body| (head, body)),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        drop(server);
        let mut written = Vec::new();
        client.read_to_end(&mut written).await.unwrap();
        (result, written)
    }

    fn status(result: &Result<(Head, Vec<u8>), Error>) -> u16 {
        match result {
            Ok(_) => 200,
            Err(err) => err.status,
        }
    }

    #[tokio::test]
    async fn reads_a_request_with_a_body() {
        let (result, written) = read(
            b"POST /hooks/pr?x=1 HTTP/1.1\r\nHost: a\r\nX-GitHub-Event:  ping \r\n\
              Content-Length: 5\r\n\r\nhello and more",
        )
        .await;
        let (head, body) = result.unwrap();
        assert_eq!(head.method, "POST");
        assert_eq!(head.path, "/hooks/pr");
        assert_eq!(head.header("x-github-event"), Some("ping"));
        assert_eq!(head.header("host"), Some("a"));
        assert_eq!(body, b"hello");
        assert!(written.is_empty(), "no 100 Continue unasked");
    }

    #[tokio::test]
    async fn answers_expect_continue_before_the_body() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client
            .write_all(
                b"POST /hooks/a HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n",
            )
            .await
            .unwrap();
        let reader = tokio::spawn(async move {
            let mut buf = Vec::new();
            let head = read_head(&mut server, &mut buf).await.unwrap();
            let length = body_length(&head).unwrap();
            read_body(&mut server, &head, length, buf).await.unwrap()
        });
        let mut reply = [0u8; 25];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"HTTP/1.1 100 Continue\r\n\r\n");
        client.write_all(b"ok").await.unwrap();
        assert_eq!(reader.await.unwrap(), b"ok");
    }

    #[tokio::test]
    async fn refuses_what_it_does_not_read() {
        let big_header = format!(
            "POST /hooks/a HTTP/1.1\r\nX-Big: {}\r\nContent-Length: 0\r\n\r\n",
            "a".repeat(MAX_HEAD)
        );
        let cases: Vec<(Vec<u8>, u16)> = vec![
            (big_header.into_bytes(), 431),
            // A head that never ends, longer than the limit.
            (vec![b'a'; MAX_HEAD + 10], 431),
            (
                format!(
                    "POST /hooks/a HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
                    MAX_BODY + 1
                )
                .into_bytes(),
                413,
            ),
            (
                b"POST /hooks/a HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
                    .to_vec(),
                411,
            ),
            // Both: the body would be read two ways (request smuggling).
            (
                b"POST /hooks/a HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello"
                    .to_vec(),
                411,
            ),
            (b"POST /hooks/a HTTP/1.1\r\n\r\n".to_vec(), 411),
            (
                b"POST /hooks/a HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab"
                    .to_vec(),
                400,
            ),
            (b"POST /hooks/a HTTP/1.1\r\nContent-Length: -1\r\n\r\n".to_vec(), 400),
            (b"POST /hooks/a HTTP/1.1\r\nContent-Length: 1e3\r\n\r\n".to_vec(), 400),
            // Cut off: in the head, and in the body.
            (b"POST /hooks/a HTTP/1.1\r\nContent-Le".to_vec(), 400),
            (b"POST /hooks/a HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc".to_vec(), 400),
            (b"POST /hooks/a\r\n\r\n".to_vec(), 400),
            (b"POST hooks/a HTTP/1.1\r\n\r\n".to_vec(), 400),
            (b"post /hooks/a HTTP/1.1\r\n\r\n".to_vec(), 400),
            (b"POST /hooks/a HTTP/2\r\n\r\n".to_vec(), 400),
            (b"POST /hooks/a HTTP/1.1\r\n folded\r\n\r\n".to_vec(), 400),
            (b"POST /hooks/a HTTP/1.1\r\nBad Name: x\r\n\r\n".to_vec(), 400),
            (b"POST /hooks/a HTTP/1.1\r\nX: \xff\r\n\r\n".to_vec(), 400),
        ];
        for (input, wanted) in cases {
            let (result, _) = read(&input).await;
            assert_eq!(
                status(&result),
                wanted,
                "{:?}: {result:?}",
                String::from_utf8_lossy(&input[..input.len().min(80)])
            );
        }
        // At the limits exactly: a 1 MiB body is read.
        let mut input =
            format!("POST /hooks/a HTTP/1.1\r\nContent-Length: {MAX_BODY}\r\n\r\n").into_bytes();
        input.extend(std::iter::repeat_n(b'x', MAX_BODY));
        let (result, _) = read(&input).await;
        assert_eq!(result.unwrap().1.len(), MAX_BODY);
    }

    #[tokio::test]
    async fn a_slow_client_is_read_piece_by_piece() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            for byte in b"POST /hooks/a HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc" {
                client.write_all(&[*byte]).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            client
        });
        let mut buf = Vec::new();
        let head = read_head(&mut server, &mut buf).await.unwrap();
        let length = body_length(&head).unwrap();
        assert_eq!(
            read_body(&mut server, &head, length, buf).await.unwrap(),
            b"abc"
        );
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn finds_the_end_of_the_head_across_reads() {
        // The end split over reads at every place, and a lone `\r\n\r`
        // before it that must not count.
        let request = b"POST /hooks/a HTTP/1.1\r\nX-A: \r\rb\r\n\r\nbody";
        for split in 1..request.len() {
            let (mut client, mut server) = tokio::io::duplex(1024);
            let (first, second) = request.split_at(split);
            let (first, second) = (first.to_vec(), second.to_vec());
            let writer = tokio::spawn(async move {
                client.write_all(&first).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                client.write_all(&second).await.unwrap();
                client
            });
            let mut buf = Vec::new();
            let head = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                read_head(&mut server, &mut buf),
            )
            .await
            .unwrap_or_else(|_| panic!("the end of the head is missed at {split}"))
            .unwrap();
            assert_eq!(head.header("x-a"), Some("\r\rb"), "split at {split}");
            writer.await.unwrap();
            // What came after the head stays for the body.
            let mut rest = buf.clone();
            if rest.len() < 4 {
                let mut more = vec![0u8; 4 - rest.len()];
                server.read_exact(&mut more).await.unwrap();
                rest.extend(more);
            }
            assert_eq!(rest, b"body", "split at {split}");
        }
    }

    #[tokio::test]
    async fn answers_close_the_connection() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        respond(&mut server, 405, "only POST").await;
        drop(server);
        let mut text = String::new();
        client.read_to_string(&mut text).await.unwrap();
        assert_eq!(
            text,
            "HTTP/1.1 405 Method Not Allowed\r\ncontent-type: text/plain; charset=utf-8\r\n\
             content-length: 10\r\nconnection: close\r\nallow: POST\r\n\r\nonly POST\n"
        );
    }
}
