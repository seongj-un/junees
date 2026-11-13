//! Voice messages to text: the gateways hand a voice message or audio
//! file to an OpenAI-compatible `POST <base>/audio/transcriptions`
//! (OpenAI, Groq, a whisper server on this machine) and run the text as
//! the user's message ([`PREFIX`]).
//!
//! Off unless `transcribe_base_url` is set. The address is checked like
//! a model API's ([`crate::provider::check_base_url`]: https, or http on
//! loopback, no user name or password), the key goes only with the
//! address it was set beside ([`crate::config_file::PAIRED_KEYS`]), and
//! redirects are not followed, so the key and the audio reach only that
//! server. The audio stays in memory: it is never written to disk.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::redirect::Policy;
use serde_json::Value;

pub const BASE_URL_ENV: &str = "JUNEES_TRANSCRIBE_BASE_URL";
pub const API_KEY_ENV: &str = "JUNEES_TRANSCRIBE_API_KEY";
pub const MODEL_ENV: &str = "JUNEES_TRANSCRIBE_MODEL";
pub const LANGUAGE_ENV: &str = "JUNEES_TRANSCRIBE_LANGUAGE";
pub const MAX_BYTES_ENV: &str = "JUNEES_TRANSCRIBE_MAX_BYTES";

/// The model asked for when `transcribe_model` is not set.
pub const DEFAULT_MODEL: &str = "whisper-1";
/// The largest file sent when `transcribe_max_bytes` is not set: OpenAI's
/// limit for one file, 25 MB.
pub const DEFAULT_MAX_BYTES: u64 = 25 * 1024 * 1024;
/// The largest `transcribe_max_bytes` taken: a file is held in memory.
pub const MAX_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// What the transcribed text starts with in the user's message.
pub const PREFIX: &str = "[음성 메시지 받아쓰기]";

/// The note for a voice message while transcription is off.
pub const OFF_NOTE: &str = "(voice messages are not read here: transcription is off. Set \
     transcribe_base_url to an OpenAI-compatible transcription API, such as \
     https://api.openai.com/v1 or a whisper server on this machine; see docs/gateways.md)";

/// Tries of one transcription: the first and the retries after a 429, a
/// 5xx or a lost connection.
pub const MAX_ATTEMPTS: u32 = 3;
/// Time one request may take, upload and answer.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest `retry-after` waited for.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// The largest answer read.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;

const NAMES: crate::provider::AddressNames = crate::provider::AddressNames {
    env: BASE_URL_ENV,
    setting: "transcribe_base_url",
    key_env: API_KEY_ENV,
    key_setting: "transcribe_api_key",
};

/// Where and how to transcribe. Not `Debug`: it holds the key.
pub struct Settings {
    /// Without a trailing `/`.
    pub base_url: String,
    api_key: Option<String>,
    pub model: String,
    /// An ISO-639-1 code, sent as is.
    pub language: Option<String>,
    pub max_bytes: u64,
}

impl Settings {
    /// Reads the `JUNEES_TRANSCRIBE_*` settings from `get` (which trims and
    /// drops empty values, and gives the key only from where the address
    /// is set); `None` when no address is set (off). A server that is not
    /// on this machine needs a key.
    pub fn read(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let Some(base_url) = get(BASE_URL_ENV) else {
            return Ok(None);
        };
        crate::provider::check_base_url(&base_url, &NAMES)?;
        let language = get(LANGUAGE_ENV).map(|code| code.to_ascii_lowercase());
        if let Some(code) = &language
            && !(code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_lowercase()))
        {
            bail!(
                "{LANGUAGE_ENV} (`transcribe_language`) must be a two-letter ISO-639-1 code \
                 such as ko or en, got `{code}`"
            );
        }
        let max_bytes = match get(MAX_BYTES_ENV) {
            None => DEFAULT_MAX_BYTES,
            Some(text) => match text.parse::<u64>() {
                Ok(bytes) if (1..=MAX_MAX_BYTES).contains(&bytes) => bytes,
                _ => bail!(
                    "{MAX_BYTES_ENV} (`transcribe_max_bytes`) must be a whole number of bytes \
                     from 1 to {MAX_MAX_BYTES}, got `{text}`"
                ),
            },
        };
        let settings = Self {
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            api_key: get(API_KEY_ENV),
            model: get(MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            language,
            max_bytes,
        };
        if settings.api_key.is_none() && !settings.is_local() {
            bail!(
                "no key for the transcription API at {}: set transcribe_api_key or \
                 {API_KEY_ENV}, in the same place as the address (a server on this machine \
                 needs none)",
                settings.base_url
            );
        }
        Ok(Some(settings))
    }

    /// Whether the server runs on this machine (a loopback address).
    pub fn is_local(&self) -> bool {
        reqwest::Url::parse(&self.base_url)
            .is_ok_and(|url| crate::mcp::http::target::is_loopback(&url))
    }

    pub fn has_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Where requests go.
    pub fn endpoint(&self) -> String {
        format!("{}/audio/transcriptions", self.base_url)
    }
}

/// A voice message or audio file, downloaded.
pub struct Audio {
    /// The MIME type the platform gave (`audio/ogg`).
    pub mime: String,
    /// The file name as sent, if any: only its extension is used.
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Why a transcription failed, in words for the user and the log. Never
/// holds the key, the audio or the address's query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure(pub String);

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Sends audio to the transcription API.
pub struct Transcriber {
    client: reqwest::Client,
    settings: Settings,
}

impl Transcriber {
    pub fn new(settings: Settings) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .context("cannot set up the HTTP client for transcription")?;
        Ok(Self { client, settings })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The text of `audio`, trimmed; a [`Failure`] when the API refuses
    /// it, keeps failing after [`MAX_ATTEMPTS`], or hears nothing.
    pub async fn transcribe(&self, audio: &Audio) -> Result<String, Failure> {
        let (boundary, body) = form(&self.settings, audio)?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = self
                .client
                .post(self.settings.endpoint())
                .header(
                    CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body.clone());
            if let Some(key) = &self.settings.api_key {
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| {
                        Failure("transcribe_api_key holds characters a header cannot".into())
                    })?;
                value.set_sensitive(true);
                request = request.header(AUTHORIZATION, value);
            }
            let retry_after = match request.send().await {
                Ok(response) if response.status().is_success() => return answer(response).await,
                Ok(response) => {
                    let status = response.status();
                    let wait = retry_after(&response);
                    if !retryable(status) || attempt >= MAX_ATTEMPTS {
                        return Err(refused(status, response, attempt).await);
                    }
                    wait
                }
                Err(err) => {
                    let worth = err.is_connect() || err.is_timeout() || err.is_request();
                    if !worth || attempt >= MAX_ATTEMPTS {
                        return Err(Failure(format!(
                            "cannot reach the transcription service ({})",
                            describe(&err)
                        )));
                    }
                    None
                }
            };
            let wait = retry_after.unwrap_or(Duration::from_millis(500 * 2u64.pow(attempt - 1)));
            tokio::time::sleep(wait.min(MAX_RETRY_AFTER)).await;
        }
    }
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// A refused request, in words: a key problem, the service's own short
/// message, or a status.
async fn refused(status: StatusCode, response: reqwest::Response, attempts: u32) -> Failure {
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Failure(format!(
            "the transcription service refused the key (HTTP {}): check transcribe_api_key \
             and transcribe_base_url",
            status.as_u16()
        ));
    }
    if retryable(status) {
        return Failure(format!(
            "the transcription service kept failing (HTTP {}, {attempts} tries)",
            status.as_u16()
        ));
    }
    let message = read_capped(response)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|json| {
            json.pointer("/error/message")
                .or_else(|| json.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        // The service's words, unless they look like a secret.
        .filter(|text| crate::secrets::find_secret(text).is_none())
        .map(|text| {
            let short: String = text.chars().take(200).collect();
            format!(": {}", crate::prompt_text::escape_hidden(&short))
        })
        .unwrap_or_default();
    Failure(format!(
        "the transcription service refused the audio (HTTP {}){message}",
        status.as_u16()
    ))
}

async fn answer(response: reqwest::Response) -> Result<String, Failure> {
    let bytes = read_capped(response).await.map_err(|why| {
        Failure(format!(
            "the transcription service's answer could not be read ({why})"
        ))
    })?;
    let text = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|json| json.get("text").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| Failure("the transcription service's answer has no text".into()))?;
    let text = text.trim();
    if text.is_empty() {
        return Err(Failure("no speech was heard in it".into()));
    }
    Ok(text.to_string())
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| describe(&err))? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_ANSWER_BYTES {
            return Err("the answer is too long".into());
        }
    }
    Ok(bytes)
}

/// A request error without its URL.
fn describe(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        format!("timed out after {} s", REQUEST_TIMEOUT.as_secs())
    } else if err.is_connect() {
        "cannot connect".to_string()
    } else if err.is_redirect() {
        "it answered with a redirect, which is not followed".to_string()
    } else {
        "the connection failed".to_string()
    }
}

/// The file's name in the request: `audio.<extension>`, the extension from
/// the MIME type, else from the name sent. The service tells the format
/// by it; the user's own file name is not sent.
pub fn file_name(audio: &Audio) -> String {
    let mime = audio
        .mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let known = match mime.as_str() {
        "audio/ogg" | "audio/opus" | "application/ogg" => Some("ogg"),
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" => Some("m4a"),
        "audio/wav" | "audio/x-wav" | "audio/wave" => Some("wav"),
        "audio/webm" => Some("webm"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        _ => None,
    };
    let extension = known.map(str::to_string).or_else(|| {
        let (_, ext) = audio.name.rsplit_once('.')?;
        let ext = ext.to_ascii_lowercase();
        (1..=5)
            .contains(&ext.len())
            .then_some(ext)
            .filter(|ext| ext.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    });
    format!("audio.{}", extension.as_deref().unwrap_or("ogg"))
}

/// The MIME type sent with the file: the platform's, when it is a plain
/// `type/subtype`; else `application/octet-stream`.
fn part_type(mime: &str) -> &str {
    let mime = mime.split(';').next().unwrap_or("").trim();
    let plain = mime.len() <= 64
        && mime.split_once('/').is_some_and(|(kind, sub)| {
            let ok = |part: &str| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b".+-_".contains(&byte))
            };
            ok(kind) && ok(sub)
        });
    if plain {
        mime
    } else {
        "application/octet-stream"
    }
}

/// The multipart/form-data body: `model`, `language` if set,
/// `response_format=json` and `file`; and its boundary, which the audio
/// does not contain.
pub fn form(settings: &Settings, audio: &Audio) -> Result<(String, Vec<u8>), Failure> {
    let boundary = loop {
        let random = crate::mcp::oauth::pkce::random_text(24).map_err(Failure)?;
        let boundary = format!("junees-{}", random.replace(['-', '_'], "0"));
        let found = audio
            .bytes
            .windows(boundary.len())
            .any(|window| window == boundary.as_bytes());
        if !found {
            break boundary;
        }
    };
    let mut body = Vec::with_capacity(audio.bytes.len() + 1024);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("model", &settings.model.replace(['\r', '\n', '"'], ""));
    if let Some(language) = &settings.language {
        field("language", language);
    }
    field("response_format", "json");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n\
             Content-Type: {}\r\n\r\n",
            file_name(audio),
            part_type(&audio.mime)
        )
        .as_bytes(),
    );
    body.extend_from_slice(&audio.bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((boundary, body))
}

/// The user's message for transcribed `texts`: the text that came with
/// them first, then each transcription after [`PREFIX`].
pub fn message(text: &str, texts: &[String]) -> String {
    let mut out = String::new();
    if !text.trim().is_empty() {
        out.push_str(text);
        out.push_str("\n\n");
    }
    let parts: Vec<String> = texts
        .iter()
        .map(|said| format!("{PREFIX}\n{said}"))
        .collect();
    out.push_str(&parts.join("\n\n"));
    out
}

#[cfg(test)]
mod tests;
