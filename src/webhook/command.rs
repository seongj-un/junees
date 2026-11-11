//! The `junees webhook` commands.

use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::signal::unix::{SignalKind, signal};

use super::cli::{AddArgs, USAGE, WebhookCommand};
use super::run::{self, Ended, Incoming};
use super::server::{self, Env, ProcessRunner, Server};
use super::store::{Kind, Route, Secret, Store, check_event, check_name};
use super::template;
use crate::cancel::Cancel;
use crate::config_file::{self, Settings};
use crate::cron::command::{cancel_on_signals, engine_in};
use crate::cron::deliver::{Delivery, Lookup};
use crate::cron::run::Outcome;
use crate::cron::store::{RunStatus, random_hex};
use crate::cron::tz::TimeZone;
use crate::cron::unix_now;
use crate::gateway::config as discord;
use crate::prompt_text::{escape_hidden, is_hidden};

/// The setting of variables the tools do not see.
const HIDDEN_ENV: &str = "JUNEES_HIDDEN_ENV";

/// Longest prompt template.
const PROMPT_CHARS: usize = 50_000;

/// Largest request `junees webhook run` reads from its standard input
/// (a 1 MiB body, as JSON inside JSON).
const RUN_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Runs `command` with the settings of `settings`.
pub async fn main(command: WebhookCommand, settings: Settings) -> Result<ExitCode> {
    let settings = Arc::new(settings);
    let lookup: Lookup = {
        let settings = Arc::clone(&settings);
        Arc::new(move |key: &str| settings.get(key))
    };
    if command == WebhookCommand::Help {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let home = config_file::home_dir(|key| lookup(key))
        .context("webhook routes live in the state folder: set JUNEES_HOME or HOME")?;
    let store = Store::new(&home);
    match command {
        WebhookCommand::Add(args) => {
            let route = new_route(&store, &lookup, args)?;
            println!(
                "added route {}: POST /hooks/{} ({})",
                route.name,
                route.name,
                route.kind.word()
            );
            if !super::is_serving(&home) {
                eprintln!(
                    "no `junees webhook serve` is running: start it (see README, \"웹훅\") to \
                     receive requests"
                );
            }
        }
        WebhookCommand::List => print!("{}", super::listing(Some(&home), &*lookup, true)),
        WebhookCommand::Remove(name) => {
            let mut locked = store.lock()?;
            let index = locked.find(&name)?;
            let route = locked.routes.remove(index);
            locked.save()?;
            println!("removed route {}", route.name);
        }
        WebhookCommand::Pause(ref name) | WebhookCommand::Resume(ref name) => {
            let pause = matches!(command, WebhookCommand::Pause(_));
            let mut locked = store.lock()?;
            let index = locked.find(name)?;
            locked.routes[index].paused = pause;
            locked.save()?;
            let word = if pause { "paused" } else { "resumed" };
            println!("{word} route {}", locked.routes[index].name);
        }
        WebhookCommand::Serve { listen } => return serve(&home, lookup, listen).await,
        WebhookCommand::Test {
            name,
            payload,
            event,
            delivery,
        } => {
            let incoming = test_request(&store, &name, &payload, event, delivery)?;
            return run_now(&home, &lookup, &name, &incoming, &random_hex(8)).await;
        }
        WebhookCommand::Run { name, run_id } => {
            let mut input = Vec::new();
            std::io::stdin()
                .take(RUN_INPUT_BYTES)
                .read_to_end(&mut input)
                .context("cannot read the request")?;
            let incoming: Incoming =
                serde_json::from_slice(&input).context("the request is not readable")?;
            return run_now(&home, &lookup, &name, &incoming, &run_id).await;
        }
        WebhookCommand::Help => unreachable!("handled above"),
    }
    Ok(ExitCode::SUCCESS)
}

/// Checks and saves a new route.
fn new_route(store: &Store, lookup: &Lookup, args: AddArgs) -> Result<Route> {
    check_name(&args.name)?;
    check_prompt(&args.prompt)?;
    let kind = match &args.kind {
        Some(kind) => Kind::parse(kind)?,
        None => Kind::Github,
    };
    let secret = match (args.secret_env, args.secret_file) {
        (Some(name), _) => {
            let ok = !name.is_empty()
                && name.len() <= 128
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !name.as_bytes()[0].is_ascii_digit();
            if !ok {
                bail!("--secret-env takes the name of an environment variable");
            }
            Some(Secret::Env(name))
        }
        (None, Some(path)) => {
            let path = std::fs::canonicalize(&path)
                .with_context(|| format!("cannot find {}", path.display()))?;
            let secret = Secret::File(path);
            secret.read(&|_| None)?;
            Some(secret)
        }
        (None, None) => None,
    };
    let events = match &args.events {
        None => Vec::new(),
        Some(text) => {
            let events: Vec<String> = text
                .split(',')
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect();
            for event in &events {
                check_event(event)?;
            }
            if events.is_empty() {
                bail!("--events is empty; leave it out to run on every event");
            }
            events
        }
    };
    let dir = match args.dir {
        Some(dir) => dir,
        None => std::env::current_dir().context("cannot read the current folder")?,
    };
    let dir = std::fs::canonicalize(&dir)
        .ok()
        .filter(|dir| dir.is_dir())
        .with_context(|| format!("{} is not a folder", dir.display()))?;
    let deliver = match &args.deliver {
        Some(text) => Delivery::parse(text)?,
        None => Delivery::Local,
    };
    match &deliver {
        Delivery::Discord(_)
            if lookup(discord::TOKEN_ENV).is_none_or(|token| token.trim().is_empty()) =>
        {
            eprintln!(
                "  !! no discord_token is set: delivery to the channel will fail until it is"
            );
        }
        Delivery::Webhook
            if lookup(crate::notify::WEBHOOK_ENV).is_none_or(|url| url.trim().is_empty()) =>
        {
            eprintln!("  !! no notify_webhook is set: delivery will fail until it is");
        }
        _ => {}
    }
    match &secret {
        None => eprintln!(
            "  !! the route has no secret: anyone who can reach the server can start runs. \
             It is only served while `junees webhook serve` listens on a loopback address"
        ),
        Some(Secret::Env(name)) if std::env::var_os(name).is_none() => {
            eprintln!("  !! {name} is not set here: set it for `junees webhook serve`")
        }
        _ => {}
    }
    let mut locked = store.lock()?;
    if locked.routes.iter().any(|route| route.name == args.name) {
        bail!("a route named `{}` exists already", args.name);
    }
    let route = Route {
        name: args.name,
        kind,
        prompt: args.prompt,
        secret,
        events,
        dir,
        deliver,
        paused: false,
        created_at: unix_now(),
        last: None,
        last_event: None,
    };
    locked.routes.push(route.clone());
    locked.save()?;
    Ok(route)
}

fn check_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    if prompt.chars().count() > PROMPT_CHARS {
        bail!("the prompt is longer than {PROMPT_CHARS} characters");
    }
    if let Some(c) = prompt
        .chars()
        .find(|&c| is_hidden(c) && c != '\n' && c != '\t')
    {
        bail!(
            "the prompt contains U+{:04X}, a control or bidirectional formatting character",
            c as u32
        );
    }
    template::check(prompt)
}

/// The request `junees webhook test` runs: the payload file, the event
/// (`--event`, else the first of the route's `--events`, else `webhook`).
fn test_request(
    store: &Store,
    name: &str,
    payload: &Path,
    event: Option<String>,
    delivery: Option<String>,
) -> Result<Incoming> {
    let route = {
        let locked = store.lock()?;
        locked.routes[locked.find(name)?].clone()
    };
    let text =
        std::fs::read(payload).with_context(|| format!("cannot read {}", payload.display()))?;
    if text.len() > super::http::MAX_BODY {
        bail!(
            "{} is larger than a request may be (1 MiB)",
            payload.display()
        );
    }
    let value: Value = serde_json::from_slice(&text)
        .with_context(|| format!("{} is not JSON", payload.display()))?;
    let event = event.unwrap_or_else(|| {
        route
            .events
            .first()
            .map_or("webhook", |first| first.split('.').next().unwrap_or(first))
            .to_string()
    });
    let incoming = Incoming::new(&event, delivery.as_deref(), value, unix_now());
    if !route.wants(&incoming.event, incoming.action.as_deref()) {
        eprintln!(
            "  !! the server would ignore this request: the route does not run for {}",
            escape_hidden(&incoming.label())
        );
    }
    Ok(incoming)
}

/// Runs `incoming` on route `name` in this process, records and delivers
/// the result, and prints the answer.
async fn run_now(
    home: &Path,
    lookup: &Lookup,
    name: &str,
    incoming: &Incoming,
    run_id: &str,
) -> Result<ExitCode> {
    let started_at = unix_now();
    let route = run::start(home, name, run_id, started_at, incoming.received_at)?;
    let tz = crate::cron::timezone(&**lookup).unwrap_or_else(|_| TimeZone::utc());
    let cancel = Cancel::new();
    cancel_on_signals(cancel.clone())?;
    eprintln!(
        "running route {} ({}) in {}",
        route.name,
        escape_hidden(&incoming.label()),
        escape_hidden(&route.dir.display().to_string())
    );
    let prompt = run::prompt(&route, incoming);
    let outcome = match &prompt {
        Err(err) => Outcome::failed(format!("cannot make the prompt: {err:#}")),
        Ok(prompt) => match prepare(&hide_secrets(home, lookup), &route).await {
            Ok((engine, limits)) => run::execute(&*engine, &route, prompt, &cancel, limits).await,
            Err(err) => Outcome::failed(format!("{err:#}")),
        },
    };
    let ended = Ended {
        run_id,
        started_at,
        prompt: prompt.as_deref().ok(),
        outcome: &outcome,
    };
    let last = run::finish(home, &tz, lookup, &route, incoming, &ended).await;
    if !outcome.answer.is_empty() {
        println!("{}", outcome.answer);
    }
    let mut status = format!("route {}: {}", route.name, outcome.status.word());
    if !outcome.note.is_empty() {
        status.push_str(&format!(" ({})", outcome.note));
    }
    if let Some(session) = &outcome.session {
        status.push_str(&format!("; session {session}"));
    }
    status.push_str(&format!("; {} tokens", outcome.tokens));
    if let Some(output) = &last.output {
        status.push_str(&format!("; output {}", output.display()));
    }
    eprintln!("{status}");
    Ok(if outcome.status == RunStatus::Ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `lookup` with every route's secret variable added to `hidden_env`, so
/// the run's tools (bash, MCP servers, plugins) never see one. The server
/// starts runs without them already; `junees webhook test` runs in the
/// user's shell, which may hold them.
fn hide_secrets(home: &Path, lookup: &Lookup) -> Lookup {
    let names: Vec<String> = Store::new(home)
        .routes()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|route| match route.secret {
            Some(Secret::Env(name)) => Some(name),
            _ => None,
        })
        .collect();
    let inner = Arc::clone(lookup);
    Arc::new(move |key: &str| {
        let value = inner(key);
        if key != HIDDEN_ENV || names.is_empty() {
            return value;
        }
        let mut list: Vec<String> = value.into_iter().filter(|v| !v.trim().is_empty()).collect();
        list.extend(names.iter().cloned());
        Some(list.join(","))
    })
}

async fn prepare(
    lookup: &Lookup,
    route: &Route,
) -> Result<(Box<dyn crate::cron::run::Engine>, crate::cron::run::Limits)> {
    let limits = super::limits(&**lookup)?;
    Ok((engine_in(lookup, &route.dir).await?, limits))
}

/// `junees webhook serve`.
async fn serve(home: &Path, lookup: Lookup, listen: Option<String>) -> Result<ExitCode> {
    let listen = listen.unwrap_or_else(|| super::DEFAULT_LISTEN.to_string());
    let addr: SocketAddr = listen.parse().with_context(|| {
        format!("--listen takes an address and port such as 127.0.0.1:8650 (got `{listen}`)")
    })?;
    let _lock = server::lock(home)?;
    let limits = super::limits(&*lookup)?;
    let program = std::env::current_exe().context("cannot find the junees program")?;
    let loopback = addr.ip().is_loopback();
    let env: Env = Arc::new(|key: &str| std::env::var(key).ok());
    for route in Store::new(home).routes()? {
        match &route.secret {
            None if !loopback => eprintln!(
                "  !! route {} has no secret: refused on a non-loopback address",
                route.name
            ),
            Some(secret) => {
                if let Err(err) = secret.read(&*env) {
                    eprintln!("  !! route {}: {err:#}; its requests get 500", route.name);
                }
            }
            None => {}
        }
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    let local = listener.local_addr()?;
    if !loopback {
        eprintln!(
            "  !! listening on {local}, beyond this machine: only routes with a secret are served"
        );
    }
    let (stop, force) = (Cancel::new(), Cancel::new());
    stop_on_signals(stop.clone(), force.clone())?;
    let log = crate::gateway::stderr_log();
    let runner = ProcessRunner::new(program, home, lookup, limits, Arc::clone(&log));
    eprintln!(
        "junees webhook server for {}: POST http://{local}/hooks/<name>; SIGINT or SIGTERM stops it",
        home.display()
    );
    Arc::new(Server::new(home, loopback, Arc::new(runner), env, log))
        .serve(listener, stop, force)
        .await;
    eprintln!("webhook server stopped");
    Ok(ExitCode::SUCCESS)
}

/// Sets `stop` on the first SIGINT or SIGTERM and `force` on the second.
fn stop_on_signals(stop: Cancel, force: Cancel) -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt()).context("cannot catch SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("cannot catch SIGTERM")?;
    tokio::spawn(async move {
        for flag in [stop, force] {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
            flag.cancel();
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::cron::deliver::tests::lookup;

    #[test]
    fn adding_checks_the_route_and_names_are_unique() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let settings = lookup(&[]);
        let args = |name: &str| AddArgs {
            name: name.into(),
            prompt: "review {{payload.number}}".into(),
            secret_env: Some("PR_HOOK_SECRET".into()),
            events: Some("pull_request.opened, issue_comment".into()),
            dir: Some(home.path().to_path_buf()),
            ..AddArgs::default()
        };
        let route = new_route(&store, &settings, args("pr")).unwrap();
        assert_eq!(route.kind, Kind::Github);
        assert_eq!(route.events, ["pull_request.opened", "issue_comment"]);
        assert_eq!(route.dir, std::fs::canonicalize(home.path()).unwrap());
        assert_eq!(route.secret, Some(Secret::Env("PR_HOOK_SECRET".into())));
        assert_eq!(store.routes().unwrap(), [route]);
        let secret_file = home.path().join("secret");
        std::fs::write(&secret_file, "x").unwrap();
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o640)).unwrap();
        for (change, wanted) in [
            (
                Box::new(|_: &mut AddArgs| {}) as Box<dyn Fn(&mut AddArgs)>,
                "exists already",
            ),
            (
                Box::new(|a: &mut AddArgs| a.name = "Bad/".into()),
                "route name",
            ),
            (
                Box::new(|a: &mut AddArgs| a.prompt = " ".into()),
                "prompt is empty",
            ),
            (
                Box::new(|a: &mut AddArgs| a.prompt = "a\u{202e}b".into()),
                "U+202E",
            ),
            (
                Box::new(|a: &mut AddArgs| a.prompt = "{{secret}}".into()),
                "unknown field",
            ),
            (
                Box::new(|a: &mut AddArgs| a.kind = Some("gitlab".into())),
                "--kind",
            ),
            (
                Box::new(|a: &mut AddArgs| a.secret_env = Some("A-B".into())),
                "--secret-env",
            ),
            (
                Box::new(move |a: &mut AddArgs| {
                    a.secret_env = None;
                    a.secret_file = Some(secret_file.clone());
                }),
                "chmod 600",
            ),
            (
                Box::new(|a: &mut AddArgs| a.events = Some("pull request".into())),
                "not an event",
            ),
            (
                Box::new(|a: &mut AddArgs| a.events = Some(",".into())),
                "empty",
            ),
            (
                Box::new(|a: &mut AddArgs| a.deliver = Some("slack".into())),
                "--deliver",
            ),
            (
                Box::new(|a: &mut AddArgs| a.dir = Some("/nonexistent/x".into())),
                "not a folder",
            ),
        ] {
            let mut args = args("pr");
            if wanted != "exists already" {
                args.name = "other".into();
            }
            change(&mut args);
            let err = new_route(&store, &settings, args).unwrap_err().to_string();
            assert!(err.contains(wanted), "{wanted}: {err}");
        }
        assert_eq!(store.routes().unwrap().len(), 1);
    }

    #[test]
    fn runs_hide_every_route_secret_from_the_tools() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let mut locked = store.lock().unwrap();
        let mut first = crate::webhook::store::tests::route("a");
        first.secret = Some(Secret::Env("A_SECRET".into()));
        let mut second = crate::webhook::store::tests::route("b");
        second.secret = Some(Secret::File("/x".into()));
        let mut third = crate::webhook::store::tests::route("c");
        third.secret = Some(Secret::Env("C_SECRET".into()));
        locked.routes = vec![first, second, third];
        locked.save().unwrap();
        drop(locked);
        let hidden = hide_secrets(home.path(), &lookup(&[(HIDDEN_ENV, "DB_PASSWORD")]));
        assert_eq!(
            hidden(HIDDEN_ENV).as_deref(),
            Some("DB_PASSWORD,A_SECRET,C_SECRET")
        );
        assert_eq!(hidden("JUNEES_MODEL"), None);
        let hidden = hide_secrets(home.path(), &lookup(&[]));
        let config = crate::config::Config::from_lookup(|key| match key {
            "ANTHROPIC_API_KEY" => Some("k".into()),
            _ => hidden(key),
        })
        .unwrap();
        assert_eq!(config.tools.hidden_env, ["A_SECRET", "C_SECRET"]);
    }

    #[test]
    fn a_test_request_takes_the_event_of_the_filter() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let mut locked = store.lock().unwrap();
        let mut route = crate::webhook::store::tests::route("pr");
        route.events = vec!["pull_request.opened".into()];
        locked.routes.push(route);
        locked.save().unwrap();
        drop(locked);
        let payload = home.path().join("p.json");
        std::fs::write(&payload, r#"{"action": "opened", "number": 3}"#).unwrap();
        let incoming = test_request(&store, "pr", &payload, None, None).unwrap();
        assert_eq!(incoming.label(), "pull_request.opened");
        let incoming = test_request(
            &store,
            "pr",
            &payload,
            Some("push".into()),
            Some("d".into()),
        )
        .unwrap();
        assert_eq!(incoming.event, "push");
        assert_eq!(incoming.delivery.as_deref(), Some("d"));
        std::fs::write(&payload, "nope").unwrap();
        let err = test_request(&store, "pr", &payload, None, None).unwrap_err();
        assert!(err.to_string().contains("is not JSON"), "{err}");
        assert!(test_request(&store, "zz", &payload, None, None).is_err());
    }
}
