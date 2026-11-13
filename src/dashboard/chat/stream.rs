//! `GET /api/chat/<id>/events`: a chat's events as `text/event-stream`,
//! read with `fetch` (the request carries the bearer token, which
//! `EventSource` cannot send).
//!
//! Each event is `id: <seq>` and one `data:` line of JSON. The stream
//! starts after `after` (all kept events without it), then waits for new
//! ones; a `: keepalive` comment goes out every [`KEEPALIVE`]. Numbers
//! come from one counter of the process, so they rise across chats and a
//! chat opened again. It ends when the page closes the connection (seen at
//! once, which frees its place), a write stalls for [`WRITE_TIME`], or the
//! chat is closed.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;

use super::Room;

/// How often a quiet stream says it is alive.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long one write may wait for a slow reader.
pub const WRITE_TIME: Duration = Duration::from_secs(30);

/// An accepted event stream.
pub struct Stream {
    pub(super) room: Arc<Room>,
    pub(super) after: u64,
    pub(super) _permit: OwnedSemaphorePermit,
}

/// The head of the answer: 200, the event-stream type and `headers`.
fn head(headers: &[(&str, &str)]) -> Vec<u8> {
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\nconnection: close\r\n",
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    head.into_bytes()
}

/// The events of `seq`/`data` pairs as sent.
pub fn render(events: &[(u64, String)]) -> String {
    events
        .iter()
        .map(|(seq, data)| format!("id: {seq}\ndata: {data}\n\n"))
        .collect()
}

async fn write<W: AsyncWrite + Unpin>(io: &mut W, bytes: &[u8]) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIME, io.write_all(bytes)).await,
        Ok(Ok(()))
    ) && matches!(
        tokio::time::timeout(WRITE_TIME, io.flush()).await,
        Ok(Ok(()))
    )
}

/// Serves `stream`, writing to `io` and watching `input` for the page
/// closing the connection, until the reader goes away.
pub async fn serve<R, W>(input: &mut R, io: &mut W, stream: Stream, headers: &[(&str, &str)])
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if !write(io, &head(headers)).await {
        return;
    }
    let mut seen = stream.room.seq.subscribe();
    let mut after = stream.after;
    let mut scratch = [0u8; 256];
    loop {
        seen.borrow_and_update();
        if stream.room.is_closed() {
            return;
        }
        let events = stream.room.after(after);
        if let Some((last, _)) = events.last() {
            after = *last;
            if !write(io, render(&events).as_bytes()).await {
                return;
            }
            continue;
        }
        tokio::select! {
            waited = tokio::time::timeout(KEEPALIVE, seen.changed()) => match waited {
                Ok(Ok(())) => {}
                // The chat is gone.
                Ok(Err(_)) => return,
                Err(_) => {
                    if !write(io, b": keepalive\n\n").await {
                        return;
                    }
                }
            },
            // A request has no more to say: anything read is ignored, and
            // the end (or an error) is the page going away.
            read = input.read(&mut scratch) => {
                if !matches!(read, Ok(n) if n > 0) {
                    return;
                }
            }
        }
    }
}
