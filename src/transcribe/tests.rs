use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// A request the fake got: its head and body.
#[derive(Clone)]
struct Seen {
    head: String,
    body: Vec<u8>,
}

/// A loopback transcription API answering `answers` in order (status,
/// extra header line or "", body), one per connection; the base URL and
/// what it got.
async fn fake(answers: Vec<(u16, &'static str, &'static str)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for (status, header, body) in answers {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut data = Vec::new();
            let mut buf = [0u8; 8192];
            let (head, length) = loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    data.drain(..end + 4);
                    break (head, length);
                }
            };
            while data.len() < length {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            log.lock().unwrap().push(Seen { head, body: data });
            let extra = if header.is_empty() {
                String::new()
            } else {
                format!("{header}\r\n")
            };
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });
    (base, seen)
}

fn settings(pairs: &[(&str, &str)]) -> Result<Option<Settings>> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Settings::read(move |key| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    })
}

fn voice() -> Audio {
    Audio {
        mime: "audio/ogg; codecs=opus".into(),
        name: "voice-message.ogg".into(),
        bytes: b"OggS\x00fake opus \r\n--junees bytes".to_vec(),
    }
}

#[test]
fn off_without_an_address() {
    assert!(settings(&[(API_KEY_ENV, "sk-fake")]).unwrap().is_none());
}

#[test]
fn checks_the_address_key_language_and_size() {
    let err = settings(&[(BASE_URL_ENV, "http://example.com/v1"), (API_KEY_ENV, "k")])
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("must use https"), "{err:#}");
    let err = settings(&[(BASE_URL_ENV, "https://user:pw@api.groq.com/openai/v1")])
        .err()
        .unwrap();
    assert!(!format!("{err:#}").contains("pw"), "{err:#}");
    let err = settings(&[(BASE_URL_ENV, "https://api.openai.com/v1")])
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("no key"), "{err:#}");
    // A server on this machine needs no key.
    let local = settings(&[(BASE_URL_ENV, "http://127.0.0.1:8000/v1/")])
        .unwrap()
        .unwrap();
    assert_eq!(
        local.endpoint(),
        "http://127.0.0.1:8000/v1/audio/transcriptions"
    );
    assert_eq!(
        (local.model.as_str(), local.max_bytes, local.has_key()),
        (DEFAULT_MODEL, DEFAULT_MAX_BYTES, false)
    );
    for (key, value) in [
        (LANGUAGE_ENV, "korean"),
        (LANGUAGE_ENV, "k1"),
        (MAX_BYTES_ENV, "0"),
        (MAX_BYTES_ENV, "25MB"),
        (MAX_BYTES_ENV, "999999999999"),
        (TIMEOUT_ENV, "5"),
        (TIMEOUT_ENV, "99999"),
        (TIMEOUT_ENV, "2m"),
    ] {
        let got = settings(&[(BASE_URL_ENV, "http://localhost:8000/v1"), (key, value)]);
        assert!(got.is_err(), "{key}={value}");
    }
    let set = settings(&[
        (BASE_URL_ENV, "https://api.groq.com/openai/v1"),
        (API_KEY_ENV, "gsk-fake"),
        (MODEL_ENV, "whisper-large-v3"),
        (LANGUAGE_ENV, "KO"),
        (MAX_BYTES_ENV, "1000"),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(set.language.as_deref(), Some("ko"));
    assert_eq!(
        (set.model.as_str(), set.max_bytes),
        ("whisper-large-v3", 1000)
    );
}

#[test]
fn the_file_is_named_by_its_type() {
    let named = |mime: &str, name: &str| {
        file_name(&Audio {
            mime: mime.into(),
            name: name.into(),
            bytes: Vec::new(),
        })
    };
    assert_eq!(named("audio/ogg", "voice"), "audio.ogg");
    assert_eq!(named("audio/mpeg", "song.MP3"), "audio.mp3");
    assert_eq!(named("audio/x-m4a", ""), "audio.m4a");
    assert_eq!(named("audio/x-unknown", "memo.AMR"), "audio.amr");
    assert_eq!(named("audio/x-unknown", "a\".\r\nb"), "audio.ogg");
    assert_eq!(part_type("audio/ogg; codecs=opus"), "audio/ogg");
    assert_eq!(
        part_type("audio/ogg\r\nX-Evil: 1"),
        "application/octet-stream"
    );
}

#[test]
fn the_form_holds_the_fields_and_the_bytes() {
    let settings = settings(&[
        (BASE_URL_ENV, "http://127.0.0.1:1/v1"),
        (LANGUAGE_ENV, "ko"),
    ])
    .unwrap()
    .unwrap();
    let audio = voice();
    let (boundary, body) = form(&settings, &audio).unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.starts_with(&format!("--{boundary}\r\n")));
    assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
    for part in [
        "name=\"model\"\r\n\r\nwhisper-1\r\n",
        "name=\"language\"\r\n\r\nko\r\n",
        "name=\"response_format\"\r\n\r\njson\r\n",
        "name=\"file\"; filename=\"audio.ogg\"\r\nContent-Type: audio/ogg\r\n\r\n",
    ] {
        assert!(text.contains(part), "{part:?} in {text}");
    }
    let at = body
        .windows(audio.bytes.len())
        .position(|window| window == audio.bytes)
        .unwrap();
    assert!(text[..at].ends_with("\r\n\r\n"));
}

#[test]
fn the_message_puts_the_text_first() {
    assert_eq!(
        message("", &["안녕하세요".into()]),
        format!("{PREFIX}\n안녕하세요")
    );
    assert_eq!(
        message("이것 좀 봐", &["하나".into(), "둘".into()]),
        format!("이것 좀 봐\n\n{PREFIX}\n하나\n\n{PREFIX}\n둘")
    );
}

#[tokio::test]
async fn sends_the_form_with_the_key_and_reads_the_text() {
    let (base, seen) = fake(vec![(200, "", r#"{"text":"  회의는 세 시입니다. "}"#)]).await;
    let transcriber = Transcriber::new(
        settings(&[(BASE_URL_ENV, &base), (API_KEY_ENV, "sk-fake-transcribe")])
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let text = transcriber.transcribe(&voice()).await.unwrap();
    assert_eq!(text, "회의는 세 시입니다.");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let head = &seen[0].head;
    assert!(
        head.starts_with("POST /v1/audio/transcriptions HTTP/1.1"),
        "{head}"
    );
    assert!(
        head.contains("authorization: Bearer sk-fake-transcribe"),
        "{head}"
    );
    assert!(
        head.contains("content-type: multipart/form-data; boundary=junees-"),
        "{head}"
    );
    let body = String::from_utf8_lossy(&seen[0].body);
    assert!(body.contains("name=\"model\"\r\n\r\nwhisper-1"), "{body}");
    assert!(body.contains("fake opus"), "{body}");
}

#[tokio::test]
async fn retries_a_429_and_a_5xx_then_gives_up() {
    let (base, seen) = fake(vec![
        (
            429,
            "Retry-After: 0",
            r#"{"error":{"message":"slow down"}}"#,
        ),
        (503, "Retry-After: 0", "{}"),
        (200, "", r#"{"text":"됐다"}"#),
    ])
    .await;
    let local = || Transcriber::new(settings(&[(BASE_URL_ENV, &base)]).unwrap().unwrap()).unwrap();
    assert_eq!(local().transcribe(&voice()).await.unwrap(), "됐다");
    assert_eq!(seen.lock().unwrap().len(), 3);
    // No key on loopback: no Authorization header.
    assert!(
        !seen.lock().unwrap()[0]
            .head
            .to_ascii_lowercase()
            .contains("authorization")
    );

    let (base, seen) = fake(vec![
        (500, "Retry-After: 0", "{}"),
        (502, "Retry-After: 0", "{}"),
        (503, "Retry-After: 0", "{}"),
        (200, "", r#"{"text":"never asked"}"#),
    ])
    .await;
    let transcriber =
        Transcriber::new(settings(&[(BASE_URL_ENV, &base)]).unwrap().unwrap()).unwrap();
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(err.0.contains("kept failing (HTTP 503, 3 tries)"), "{err}");
    assert_eq!(seen.lock().unwrap().len(), MAX_ATTEMPTS as usize);
}

#[tokio::test]
async fn a_refused_key_says_so_without_retrying() {
    for status in [401, 403] {
        let (base, seen) = fake(vec![
            (
                status,
                "",
                r#"{"error":{"message":"Incorrect API key provided: sk-fake-tr***"}}"#,
            ),
            (200, "", r#"{"text":"never asked"}"#),
        ])
        .await;
        let transcriber = Transcriber::new(
            settings(&[(BASE_URL_ENV, &base), (API_KEY_ENV, "sk-fake-transcribe")])
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let err = transcriber.transcribe(&voice()).await.unwrap_err();
        assert!(err.0.contains("refused the key"), "{err}");
        assert!(err.0.contains("transcribe_api_key"), "{err}");
        assert!(!err.0.contains("sk-fake"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn other_refusals_and_empty_text_are_failures() {
    let (base, _) = fake(vec![
        (
            400,
            "",
            r#"{"error":{"message":"Invalid file format. Supported formats: ['ogg']"}}"#,
        ),
        (200, "", r#"{"text":"   "}"#),
        (200, "", r#"{"transcript":"x"}"#),
        (307, "Location: http://127.0.0.1:9/elsewhere", ""),
    ])
    .await;
    let transcriber =
        Transcriber::new(settings(&[(BASE_URL_ENV, &base)]).unwrap().unwrap()).unwrap();
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(err.0.contains("HTTP 400): Invalid file format"), "{err}");
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(err.0.contains("no speech"), "{err}");
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(err.0.contains("has no text"), "{err}");
    // A redirect is not followed: the key would go along.
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(err.0.contains("HTTP 307"), "{err}");
}

#[tokio::test]
async fn a_spent_quota_and_a_wrong_address_say_so_without_retrying() {
    let (base, seen) = fake(vec![
        (
            429,
            "Retry-After: 0",
            r#"{"error":{"message":"You exceeded your current quota.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        ),
        (404, "", r#"{"error":{"message":"Not Found"}}"#),
        (308, "Location: https://elsewhere.example/v1/audio/transcriptions", ""),
        (503, "Retry-After: 0", r#"{"error":{"message":"overloaded"}}"#),
        (503, "Retry-After: 0", r#"{"error":{"message":"overloaded"}}"#),
        (503, "Retry-After: 0", r#"{"error":{"message":"still overloaded"}}"#),
    ])
    .await;
    let transcriber =
        Transcriber::new(settings(&[(BASE_URL_ENV, &base)]).unwrap().unwrap()).unwrap();
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(
        err.0
            .contains("quota or billing limit is reached (HTTP 429): You exceeded"),
        "{err}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "a spent quota is not tried again"
    );
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(
        err.0
            .contains("HTTP 404): check transcribe_base_url, which usually ends in /v1"),
        "{err}"
    );
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(
        err.0.contains("redirect (HTTP 308), which is not followed"),
        "{err}"
    );
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(
        err.0.ends_with("(HTTP 503, 3 tries): still overloaded"),
        "{err}"
    );
    assert_eq!(seen.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn a_request_that_runs_out_of_time_is_not_sent_again() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&accepted);
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            // Read nothing back: a slow whisper server still at work.
            held.push(stream);
        }
    });
    let mut slow = settings(&[(BASE_URL_ENV, &base)]).unwrap().unwrap();
    slow.timeout = std::time::Duration::from_secs(1);
    let transcriber = Transcriber::new(slow).unwrap();
    let err = transcriber.transcribe(&voice()).await.unwrap_err();
    assert!(
        err.0
            .contains("no answer from the transcription service within 1 s"),
        "{err}"
    );
    assert!(err.0.contains("transcribe_timeout"), "{err}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}
