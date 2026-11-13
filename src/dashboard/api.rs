//! The JSON of `/api/*`. The contract (paths, fields, errors) is in
//! `docs/dashboard.md`; the integration tests in `tests/dashboard.rs`
//! check its shape.
//!
//! Every answer is read afresh from the settings and the state folder, so
//! the page shows what the files hold now. Nothing secret goes out: keys
//! and secrets only as set or not, session text only as a short preview
//! with secrets cut out ([`crate::doctor::hide_secrets`]).

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use super::server::{Answer, Context};
use crate::config_file::{Origin, Settings};
use crate::cron::store::LastRun;

/// `limit` of `/api/sessions` without one, and the most it may be.
const SESSIONS_LIMIT: (u32, u32) = (50, 200);
/// `days` of `/api/usage` without one, and the most it may be.
const USAGE_DAYS: (u32, u32) = (30, 365);
/// Characters of a session's preview and title.
const PREVIEW_CHARS: usize = 200;
const TITLE_CHARS: usize = 80;

/// What answers a request for an API path.
#[derive(Debug)]
pub(super) struct Failure {
    pub(super) status: u16,
    pub(super) code: &'static str,
    pub(super) message: String,
}

impl Failure {
    pub(super) fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub(super) fn bad_query(message: String) -> Self {
        Self {
            status: 400,
            code: "bad_query",
            message,
        }
    }

    pub(super) fn internal(err: &anyhow::Error) -> Self {
        Self {
            status: 500,
            code: "internal",
            message: format!("{err:#}"),
        }
    }
}

/// The answer to `GET <path>?<query>` under `/api/`, once authorized.
pub async fn answer(path: &str, query: &str, context: &Arc<Context>) -> Answer {
    let env = context.env.clone();
    let (settings, broken) = Settings::load_for_setup(move |key| env(key));
    let result = match path {
        "/api/status" => Ok(status(&settings, context)),
        "/api/sessions" => match number(query, "limit", SESSIONS_LIMIT) {
            Ok(limit) => {
                let context = context.clone();
                blocking(settings, move |settings| {
                    sessions(settings, limit, &context)
                })
                .await
            }
            Err(err) => Err(err),
        },
        "/api/usage" => match number(query, "days", USAGE_DAYS) {
            Ok(days) => blocking(settings, move |settings| usage(settings, days)).await,
            Err(err) => Err(err),
        },
        "/api/cron" => blocking(settings, cron).await,
        "/api/webhooks" => blocking(settings, webhooks).await,
        "/api/doctor" => run_doctor(settings, broken, context).await,
        _ => match path.strip_prefix("/api/sessions/") {
            Some(id) => {
                let id = id.to_string();
                match super::session_view::paging(query) {
                    Ok((offset, limit)) => {
                        blocking(settings, move |settings| {
                            super::session_view::detail(settings, &id, offset, limit)
                        })
                        .await
                    }
                    Err(err) => Err(err),
                }
            }
            None => return Answer::error(404, "not_found", "there is no such API path"),
        },
    };
    finish(result, context)
}

/// The answer for `result`: 200 with its JSON, or its failure with the
/// secrets cut out of the message.
pub(super) fn finish(result: Result<Value, Failure>, context: &Context) -> Answer {
    match result {
        Ok(value) => Answer::json(200, &value),
        Err(failure) => {
            let mut message = failure.message;
            // The settings are read again: the first went into the work.
            let env = context.env.clone();
            let (settings, _) = Settings::load_for_setup(move |key| env(key));
            crate::doctor::hide_secrets(&settings, std::iter::once(&mut message));
            Answer::error(failure.status, failure.code, &message)
        }
    }
}

/// Runs `work` off the async threads: it reads files.
pub(super) async fn blocking(
    settings: Settings,
    work: impl FnOnce(&Settings) -> Result<Value, Failure> + Send + 'static,
) -> Result<Value, Failure> {
    match tokio::task::spawn_blocking(move || work(&settings)).await {
        Ok(result) => result,
        Err(err) => Err(Failure::internal(&anyhow::anyhow!(
            "the request failed: {err}"
        ))),
    }
}

/// The value of `name` in `query`: a whole number in `1..=most`, `default`
/// when it is not given. Other names are ignored.
pub(super) fn number(query: &str, name: &str, (default, most): (u32, u32)) -> Result<u32, Failure> {
    let mut value = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, text) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            value = Some(text);
        }
    }
    let Some(text) = value else {
        return Ok(default);
    };
    match text.parse::<u32>() {
        Ok(number) if (1..=most).contains(&number) => Ok(number),
        _ => Err(Failure::bad_query(format!(
            "{name} must be a whole number from 1 to {most}"
        ))),
    }
}

/// The state folder the settings name.
pub(super) fn home(settings: &Settings) -> Option<std::path::PathBuf> {
    crate::config_file::home_dir(|key| settings.get(key))
}

/// RFC 3339 in UTC; `null` before 1970.
pub(super) fn time(unix_secs: i64) -> Value {
    u64::try_from(unix_secs).map_or(Value::Null, |secs| {
        Value::String(crate::session::format_utc_seconds(secs))
    })
}

/// Nano-dollars in dollars; `null` when nothing was priced.
fn dollars(totals: &crate::usage::Totals) -> Value {
    if totals.priced {
        json!(totals.cost as f64 / 1e9)
    } else {
        Value::Null
    }
}

fn status(settings: &Settings, context: &Context) -> Value {
    use crate::claude_code::Backend;
    use crate::provider::{Kind, PROVIDER_ENV};
    let lookup = |key: &str| settings.get(key);
    let backend = Backend::from_lookup(lookup).ok();
    let model = match backend {
        Some(Backend::ClaudeCode) => settings.get(crate::claude_code::MODEL_ENV),
        Some(Backend::Api) => settings.get("JUNEES_MODEL").or_else(|| {
            matches!(
                Kind::parse(lookup(PROVIDER_ENV).as_deref()),
                Ok(Kind::Anthropic)
            )
            .then(|| crate::config::DEFAULT_MODEL.to_string())
        }),
        None => None,
    };
    let key = |name: &str| {
        let source = settings.origin(name).map(|origin| match origin {
            Origin::File => "config.toml",
            Origin::Env | Origin::Override => "env",
        });
        json!({ "set": source.is_some(), "source": source })
    };
    let (landlock, abi) = match crate::sandbox::Support::current() {
        crate::sandbox::Support::Abi(abi) => (true, Some(abi)),
        crate::sandbox::Support::Unavailable(_) => (false, None),
    };
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "home": home(settings).map(|home| home.display().to_string()),
        "backend": backend.map(|backend| match backend {
            Backend::Api => "api",
            Backend::ClaudeCode => "claude-code",
        }),
        "provider": Kind::parse(lookup(PROVIDER_ENV).as_deref()).ok().map(Kind::as_str),
        "model": model,
        "uptime_secs": context.started.elapsed().as_secs(),
        "keys": {
            "anthropic": key(crate::config_file::ANTHROPIC_KEY_ENV),
            "openai": key(crate::provider::openai::API_KEY_ENV),
        },
        "sandbox": { "landlock": landlock, "abi": abi },
    })
}

fn prices(settings: &Settings) -> Result<crate::usage::Prices, Failure> {
    crate::usage::Prices::from_setting(settings.get(crate::usage::PRICES_ENV).as_deref()).map_err(
        |err| Failure {
            status: 500,
            code: "bad_settings",
            message: format!("{}: {err}", crate::usage::PRICES_ENV),
        },
    )
}

/// The first `chars` characters of what a user wrote, with secrets cut
/// out first so that no piece of one is left at the cut.
fn shown(settings: &Settings, text: &str, chars: usize) -> Value {
    let mut text = text.trim().to_string();
    crate::doctor::hide_secrets_in_user_text(settings, std::iter::once(&mut text));
    if text.is_empty() {
        return Value::Null;
    }
    Value::String(text.chars().take(chars).collect())
}

/// The figures of one journal that take reading it whole.
#[derive(Debug, Clone)]
struct Counted {
    modified: std::time::SystemTime,
    len: u64,
    messages: usize,
    totals: crate::usage::Totals,
}

/// The [`Counted`] figures of the journals listed lately, by path, and the
/// `prices` setting they were priced with: a journal is read again only
/// once it changed, so the page asking every 30 seconds reads the
/// sessions' first lines only.
#[derive(Debug, Default)]
pub struct SessionCache {
    entries: std::collections::HashMap<std::path::PathBuf, Counted>,
    prices: Option<String>,
}

/// Journals kept in [`SessionCache`]: the most a list shows, twice.
const CACHED_SESSIONS: usize = 2 * SESSIONS_LIMIT.1 as usize;

/// The [`Counted`] figures of the journal at `path`, from `cache` when it
/// has not changed since.
fn counted(
    cache: &std::sync::Mutex<SessionCache>,
    path: &Path,
    prices: &crate::usage::Prices,
) -> Counted {
    let stamp = std::fs::metadata(path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
    if let Some((modified, len)) = stamp
        && let Some(found) = cache
            .lock()
            .ok()
            .and_then(|cache| cache.entries.get(path).cloned())
        && found.modified == modified
        && found.len == len
    {
        return found;
    }
    let mut totals = crate::usage::Totals::default();
    if let Ok((records, _)) = crate::usage::read_session(path, prices) {
        for record in &records {
            totals.add(record, prices);
        }
    }
    let (modified, len) = stamp.unwrap_or((std::time::UNIX_EPOCH, 0));
    let counted = Counted {
        modified,
        len,
        messages: crate::session::count_messages(path).unwrap_or(0),
        totals,
    };
    if stamp.is_some()
        && let Ok(mut cache) = cache.lock()
    {
        cache.entries.insert(path.to_path_buf(), counted.clone());
    }
    counted
}

fn sessions(settings: &Settings, limit: u32, context: &Context) -> Result<Value, Failure> {
    let Some(home) = home(settings) else {
        return Ok(json!({ "sessions": [] }));
    };
    let prices_setting = settings.get(crate::usage::PRICES_ENV);
    let prices = prices(settings)?;
    if let Ok(mut cache) = context.sessions.lock()
        && cache.prices != prices_setting
    {
        *cache = SessionCache {
            entries: Default::default(),
            prices: prices_setting,
        };
    }
    let list =
        crate::session::list_all(&home, limit as usize).map_err(|err| Failure::internal(&err))?;
    let sessions: Vec<Value> = list
        .iter()
        .map(|(path, summary)| {
            let counted = counted(&context.sessions, path, &prices);
            let first_line = summary
                .first_input
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            json!({
                "id": summary.id,
                "title": shown(settings, first_line, TITLE_CHARS),
                "cwd": summary.cwd,
                "updated_at": time(i64::try_from(summary.updated).unwrap_or(i64::MAX)),
                "messages": counted.messages,
                "input_tokens": counted.totals.usage.input_tokens,
                "output_tokens": counted.totals.usage.output_tokens,
                "cost_usd": dollars(&counted.totals),
                "preview": shown(settings, &summary.first_input, PREVIEW_CHARS),
            })
        })
        .collect();
    if let Ok(mut cache) = context.sessions.lock()
        && cache.entries.len() > CACHED_SESSIONS
    {
        cache
            .entries
            .retain(|path, _| list.iter().any(|(listed, _)| listed == path));
    }
    Ok(json!({ "sessions": sessions }))
}

fn usage(settings: &Settings, days: u32) -> Result<Value, Failure> {
    let empty = || json!({ "days": [], "total": { "input_tokens": 0, "output_tokens": 0, "cost_usd": null } });
    let Some(home) = home(settings) else {
        return Ok(empty());
    };
    let prices = prices(settings)?;
    let zone = crate::cron::timezone(&|key: &str| settings.get(key)).map_err(|err| Failure {
        status: 500,
        code: "bad_settings",
        message: format!("{err:#}"),
    })?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rows = crate::usage::daily(&home, days, &prices, &zone, now)
        .map_err(|err| Failure::internal(&err))?;
    let mut total = crate::usage::Totals::default();
    let days: Vec<Value> = rows
        .iter()
        .map(|row| {
            total.merge(&row.totals);
            let usage = &row.totals.usage;
            json!({
                "date": row.date,
                "model": row.model,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_read_tokens": usage.cache_read_input_tokens,
                "cache_write_tokens": usage.cache_creation_input_tokens,
                "cost_usd": dollars(&row.totals),
            })
        })
        .collect();
    Ok(json!({
        "days": days,
        "total": {
            "input_tokens": total.usage.input_tokens,
            "output_tokens": total.usage.output_tokens,
            "cost_usd": dollars(&total),
        },
    }))
}

fn last_run(settings: &Settings, last: Option<&LastRun>) -> Value {
    let Some(last) = last else {
        return Value::Null;
    };
    json!({
        "at": time(last.started_at),
        "status": last.status.word(),
        "note": note(settings, &last.note),
    })
}

/// A run's note (an error message, mostly), with secrets cut out as
/// `junees doctor` cuts them; `null` when empty.
fn note(settings: &Settings, text: &str) -> Value {
    let mut text = text.trim().to_string();
    crate::doctor::hide_secrets(settings, std::iter::once(&mut text));
    if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    }
}

fn cron(settings: &Settings) -> Result<Value, Failure> {
    let Some(home) = home(settings) else {
        return Ok(json!({ "jobs": [] }));
    };
    let jobs = read_store(&crate::cron::store::Store::new(&home).path(), |path| {
        crate::cron::store::read(path)
    })?;
    let jobs: Vec<Value> = jobs.iter().map(|job| job_json(settings, job)).collect();
    Ok(json!({ "jobs": jobs }))
}

/// A job as `/api/cron` lists it (and a pause or resume answers it).
pub(super) fn job_json(settings: &Settings, job: &crate::cron::store::Job) -> Value {
    json!({
        "id": job.id,
        "name": job.name,
        "schedule": job.text,
        "dir": job.dir.display().to_string(),
        "paused": job.paused,
        "paused_for_home": job.paused_for_home,
        "allow_home": job.allow_home,
        "next_run": job.next_run.map_or(Value::Null, time),
        "last_run": last_run(settings, job.last.as_ref()),
    })
}

fn webhooks(settings: &Settings) -> Result<Value, Failure> {
    let Some(home) = home(settings) else {
        return Ok(json!({ "routes": [] }));
    };
    let routes = read_store(&crate::webhook::store::Store::new(&home).path(), |path| {
        crate::webhook::store::read(path)
    })?;
    let routes: Vec<Value> = routes
        .iter()
        .map(|route| route_json(settings, route))
        .collect();
    Ok(json!({ "routes": routes }))
}

/// A route as `/api/webhooks` lists it (and a pause or resume answers
/// it).
pub(super) fn route_json(settings: &Settings, route: &crate::webhook::store::Route) -> Value {
    use crate::webhook::store::Kind;
    json!({
        "name": route.name,
        "kind": match route.kind {
            Kind::Github => "github",
            Kind::Generic => "generic",
        },
        "dir": route.dir.display().to_string(),
        "paused": route.paused,
        "paused_for_home": route.paused_for_home,
        "allow_home": route.allow_home,
        "has_secret": route.secret.is_some(),
        "last_run": last_run(settings, route.last.as_ref()),
    })
}

/// A store's entries; none when its file is not there yet.
fn read_store<T>(
    path: &Path,
    read: impl Fn(&Path) -> anyhow::Result<Vec<T>>,
) -> Result<Vec<T>, Failure> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    read(path).map_err(|err| Failure::internal(&err))
}

/// How long `/api/doctor` waits for its turn, and then for doctor.
const DOCTOR_TIME: std::time::Duration = std::time::Duration::from_secs(20);

/// [`doctor`] on a runtime of its own on a blocking thread: some checks
/// hold values that are not `Send` across their awaits, so the future
/// cannot move between the server's threads. One runs at a time, and a
/// request waits [`DOCTOR_TIME`] for its turn and again for the answer;
/// a check that hangs keeps one thread, not one per request.
async fn run_doctor(
    settings: Settings,
    broken: Option<anyhow::Error>,
    context: &Context,
) -> Result<Value, Failure> {
    let busy = |what: &str| Failure {
        status: 503,
        code: "doctor_busy",
        message: format!("doctor {what}; try again shortly"),
    };
    let permit = tokio::time::timeout(DOCTOR_TIME, context.doctor.clone().acquire_owned())
        .await
        .map_err(|_| busy("is still running for an earlier request"))?
        .map_err(|_| busy("is not available"))?;
    let work = move || {
        let _permit = permit;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| Failure::internal(&anyhow::anyhow!("cannot run doctor: {err}")))?;
        Ok(runtime.block_on(doctor(&settings, broken.as_ref())))
    };
    match tokio::time::timeout(DOCTOR_TIME, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => Err(Failure::internal(&anyhow::anyhow!("doctor failed: {err}"))),
        Err(_) => Err(busy("took too long")),
    }
}

/// `junees doctor` offline, by area in the order they ran. Checks that do
/// not apply (`skip`) are left out; secrets are already cut out.
async fn doctor(settings: &Settings, broken: Option<&anyhow::Error>) -> Value {
    use crate::doctor::Status;
    let report = crate::doctor::run(settings, broken, false).await;
    let mut sections: Vec<(&str, Vec<Value>)> = Vec::new();
    for check in &report.checks {
        let status = match check.status {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skip => continue,
        };
        let item = json!({
            "name": check.name,
            "status": status,
            "detail": check.detail,
            "fix": check.fix,
        });
        match sections.iter_mut().find(|(area, _)| *area == check.area) {
            Some((_, checks)) => checks.push(item),
            None => sections.push((check.area, vec![item])),
        }
    }
    let sections: Vec<Value> = sections
        .into_iter()
        .map(|(name, checks)| json!({ "name": name, "checks": checks }))
        .collect();
    json!({ "sections": sections })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_in_the_query_are_checked() {
        let limit = SESSIONS_LIMIT;
        assert_eq!(number("", "limit", limit).unwrap(), 50);
        assert_eq!(number("limit=7", "limit", limit).unwrap(), 7);
        assert_eq!(number("x=1&limit=200", "limit", limit).unwrap(), 200);
        assert_eq!(number("days=3", "limit", limit).unwrap(), 50);
        for query in [
            "limit=0",
            "limit=201",
            "limit=",
            "limit",
            "limit=-1",
            "limit=1.5",
            "limit=ab",
        ] {
            let err = number(query, "limit", limit).unwrap_err();
            assert_eq!((err.status, err.code), (400, "bad_query"), "{query}");
        }
    }

    #[test]
    fn user_text_keeps_its_quotes_and_loses_its_secrets() {
        let settings = Settings::new(
            |_: &str| None,
            crate::config_file::ConfigFile::default(),
            None,
            false,
        );
        assert_eq!(
            shown(&settings, "I got `undefined` from foo", 200),
            json!("I got `undefined` from foo")
        );
        let key = "sk-ant-api03-Abc1Def2Ghi3Jkl4Mno5Pqr6Stu7Vwx8";
        let text = format!("use {key} please");
        let preview = shown(&settings, &text, 200);
        assert!(!preview.as_str().unwrap().contains("sk-ant"), "{preview}");
        // Cut after hiding: no prefix of the key is left at the end.
        let cut = shown(&settings, &text, 10);
        assert!(!cut.as_str().unwrap().contains("sk-"), "{cut}");
        assert_eq!(shown(&settings, "  \n ", 200), Value::Null);
    }

    #[test]
    fn times_are_rfc_3339_in_utc() {
        assert_eq!(time(0), json!("1970-01-01T00:00:00Z"));
        assert_eq!(time(1_763_000_000), json!("2025-11-13T02:13:20Z"));
        assert_eq!(time(-1), Value::Null);
    }
}
