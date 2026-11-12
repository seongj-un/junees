//! The DevTools protocol over the browser's pipe: commands with ids,
//! answers matched to them, and events
//! (https://chromedevtools.github.io/devtools-protocol/; with flat
//! sessions a message for a page carries its `sessionId`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::pipe;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// The largest message read from the browser (a screenshot is a few MB of
/// base64).
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// How long a command may wait for its answer.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// An event the browser sent.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// The page session it belongs to; `None` for the browser itself.
    pub session: Option<String>,
}

type Answer = Result<Value, String>;

struct Inner {
    writer: tokio::sync::Mutex<pipe::Sender>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
    /// Set once the browser's side is closed: why.
    closed: Mutex<Option<String>>,
}

/// A connection to the browser. Cheap to clone; every clone sends on the
/// same pipe.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

/// Reads the browser's messages until the pipe closes.
pub struct Reader(JoinHandle<()>);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Connection {
    /// A connection over `pipes`; every event goes to the returned
    /// receiver, in order.
    pub fn new(
        from_browser: pipe::Receiver,
        to_browser: pipe::Sender,
    ) -> (Self, Reader, mpsc::UnboundedReceiver<Event>) {
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(to_browser),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: Mutex::new(None),
        });
        let (events, receiver) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_loop(Arc::clone(&inner), from_browser, events));
        (Self { inner }, Reader(reader), receiver)
    }

    /// Sends `method` to the browser (`session` `None`) or a page session
    /// and waits for its result, at most [`CALL_TIMEOUT`].
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Answer {
        self.call_within(method, params, session, CALL_TIMEOUT)
            .await
    }

    pub async fn call_within(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Answer {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (sender, receiver) = oneshot::channel();
        {
            // The reader marks the end under the same lock.
            let mut pending = self.pending();
            if let Some(why) = self.closed() {
                return Err(why);
            }
            pending.insert(id, sender);
        }
        // Removes the entry when this call ends early (timeout, dropped).
        let _forget = Forget(&self.inner, id);
        let mut bytes = message.to_string().into_bytes();
        bytes.push(0);
        {
            let mut writer = self.inner.writer.lock().await;
            if let Err(err) = writer.write_all(&bytes).await {
                // Its end of the pipe is closed: the browser is gone, even
                // if the reader has not seen the end of its own pipe yet.
                let why = format!("the browser is gone ({err})");
                let _pending = self.pending();
                self.inner
                    .closed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get_or_insert_with(|| why.clone());
                return Err(why);
            }
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(self
                .closed()
                .unwrap_or_else(|| "the browser is gone".into())),
            Err(_) => Err(format!(
                "the browser did not answer {method} within {} s",
                timeout.as_secs()
            )),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Answer>>> {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Why the browser's side is closed, if it is.
    pub fn closed(&self) -> Option<String> {
        self.inner
            .closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct Forget<'a>(&'a Inner, u64);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.1);
    }
}

async fn read_loop(
    inner: Arc<Inner>,
    mut from_browser: pipe::Receiver,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut buffer: Vec<u8> = Vec::new();
    // How much of `buffer` holds no message end: a large message arrives
    // in many reads, and each read looks only at its new bytes.
    let mut scanned = 0;
    let mut chunk = vec![0u8; 64 * 1024];
    let why = loop {
        // Every complete message in the buffer first.
        let mut start = 0;
        while let Some(at) = buffer[scanned..].iter().position(|&b| b == 0) {
            let end = scanned + at;
            dispatch(&inner, &events, &buffer[start..end]);
            start = end + 1;
            scanned = start;
        }
        buffer.drain(..start);
        scanned = buffer.len();
        if buffer.len() > MAX_MESSAGE {
            break "the browser sent a message over 64 MB".to_string();
        }
        match from_browser.read(&mut chunk).await {
            Ok(0) => break "the browser closed".to_string(),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(err) => break format!("cannot read from the browser: {err}"),
        }
    };
    let pending: Vec<_> = {
        let mut pending = inner.pending.lock().unwrap_or_else(PoisonError::into_inner);
        *inner.closed.lock().unwrap_or_else(PoisonError::into_inner) = Some(why.clone());
        pending.drain().collect()
    };
    for (_, sender) in pending {
        let _ = sender.send(Err(why.clone()));
    }
}

fn dispatch(inner: &Inner, events: &mpsc::UnboundedSender<Event>, message: &[u8]) {
    let Ok(message) = serde_json::from_slice::<Value>(message) else {
        return;
    };
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let sender = inner
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
        if let Some(sender) = sender {
            let answer = match message.get("error") {
                Some(error) => Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(answer);
        }
        return;
    }
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        let _ = events.send(Event {
            method: method.to_string(),
            params: message.get("params").cloned().unwrap_or(Value::Null),
            session: message
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    /// A large answer in many small writes, and several events (the last
    /// one split) in one write, all arrive whole and in order.
    #[tokio::test]
    async fn messages_arrive_whole_however_the_reads_cut_them() {
        let (mut browser_reads, to_browser) = std::io::pipe().unwrap();
        let (from_browser, mut browser_writes) = std::io::pipe().unwrap();
        let (connection, _reader, mut events) = Connection::new(
            pipe::Receiver::from_owned_fd(from_browser.into()).unwrap(),
            pipe::Sender::from_owned_fd(to_browser.into()).unwrap(),
        );
        let browser = std::thread::spawn(move || {
            let mut command = Vec::new();
            let mut byte = [0u8];
            while browser_reads.read_exact(&mut byte).is_ok() && byte[0] != 0 {
                command.push(byte[0]);
            }
            let command: Value = serde_json::from_slice(&command).unwrap();
            let mut answer =
                json!({"id": command["id"], "result": {"text": "가".repeat(1_000_000)}})
                    .to_string()
                    .into_bytes();
            answer.push(0);
            for piece in answer.chunks(4096) {
                browser_writes.write_all(piece).unwrap();
            }
            browser_writes
                .write_all(b"{\"method\":\"A\"}\0{\"method\":\"B\"}\0{\"method\":")
                .unwrap();
            browser_writes.write_all(b"\"C\"}\0").unwrap();
        });
        let answer = connection.call("Test.big", json!({}), None).await.unwrap();
        assert_eq!(answer["text"].as_str().unwrap().chars().count(), 1_000_000);
        for method in ["A", "B", "C"] {
            assert_eq!(events.recv().await.unwrap().method, method);
        }
        browser.join().unwrap();
    }
}
