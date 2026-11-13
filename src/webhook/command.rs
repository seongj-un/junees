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
use crate::cron::command::{cancel_on_signals, check_home_folder, engine_in, user_home};
use crate::cron::deliver::{Delivery, Lookup};
use crate::cron::run::Outcome;
use crate::cron::store::{RunStatus, random_hex};
use crate::cron::tz::TimeZone;
use crate::cron::unix_now;
use crate::gateway::config as discord;
use crate::github::Write;
use crate::prompt_text::{escape_hidden, is_hidden};
use crate::tools::secrets_dir;

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
            let made = args.secret_env.is_none() && args.secret_file.is_none() && !args.no_secret;
            let route = new_route(&store, &lookup, args)?;
            println!(
                "added route {}: POST /hooks/{} ({})",
                route.name,
                route.name,
                route.kind.word()
            );
            if let (true, Some(Secret::File(path))) = (made, &route.secret) {
                println!(
                    "secret: the contents of {} (cat it; it is 0600); requests must be signed \
                     with it ({})",
                    path.display(),
                    match route.kind {
                        Kind::Github => "the webhook's Secret on GitHub",
                        Kind::Generic => "X-Junees-Signature",
                    }
                );
            }
            if !super::is_serving(&home) {
                eprintln!(
                    "no `junees webhook serve` is running: start it (see README, \"웹훅\") to \
                     receive requests"
                );
            }
        }
        WebhookCommand::AddGithub(args) => {
            let added = crate::flow::setup::add(&store, &lookup, args, unix_now())?;
            print!(
                "{}",
                crate::flow::setup::instructions(&added, super::is_serving(&home))
            );
        }
        WebhookCommand::List => print!("{}", super::listing(Some(&home), &*lookup, true)),
        WebhookCommand::Remove(name) => {
            let mut locked = store.lock()?;
            let index = locked.find(&name)?;
            let route = locked.routes.remove(index);
            locked.save()?;
            drop(locked);
            if let Some(Secret::File(path)) = &route.secret
                && route.flow.is_some()
            {
                eprintln!(
                    "  (its secret file {} stays; remove it when GitHub no longer sends to the route)",
                    path.display()
                );
            }
            println!("removed route {}", route.name);
        }
        WebhookCommand::Pause(ref name) | WebhookCommand::Resume(ref name) => {
            let pause = matches!(command, WebhookCommand::Pause(_));
            let mut locked = store.lock()?;
            let index = locked.find(name)?;
            set_paused(&mut locked.routes[index], pause);
            locked.save()?;
            let word = if pause { "paused" } else { "resumed" };
            println!("{word} route {}", locked.routes[index].name);
        }
        WebhookCommand::AllowHome(name) => allow_home(&store, &lookup, &name)?,
        WebhookCommand::DisallowHome(name) => disallow_home(&store, &lookup, &name)?,
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
            // The model's tools refuse that folder like the settings file;
            // a secret it could read would let it sign requests itself.
            let folder = secrets_dir(store.home());
            let resolved = std::fs::canonicalize(&folder).unwrap_or_else(|_| folder.clone());
            if !path.starts_with(&resolved) {
                bail!(
                    "--secret-file must be in {} (`$JUNEES_HOME/secrets/`), which Junees's tools \
                     may not read; move it there (`mkdir -m 700 -p {}`)",
                    folder.display(),
                    folder.display()
                );
            }
            let secret = Secret::File(path);
            secret.read(&|_| None)?;
            Some(secret)
        }
        // Made below, once the name is known to be free.
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
    let github_allow = match &args.github_allow {
        None => Vec::new(),
        Some(text) => {
            let writes = Write::parse_list(text, "--github-allow")?;
            if writes.is_empty() {
                bail!("--github-allow is empty; leave it out to allow no GitHub write");
            }
            writes
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
    check_home_folder(lookup, &dir, args.allow_home, None)?;
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
        None if args.no_secret => eprintln!(
            "  !! the route has no secret: anything on this machine that can reach the server \
             (other users, the model's own commands) can start runs. It is only served while \
             `junees webhook serve` listens on a loopback address, to JSON requests sent \
             straight to it (Host 127.0.0.1:<port>, localhost:<port> or [::1]:<port>; no \
             browser or proxy headers)"
        ),
        None => {}
        Some(Secret::Env(name)) if std::env::var_os(name).is_none() => {
            eprintln!("  !! {name} is not set here: set it for `junees webhook serve`")
        }
        _ => {}
    }
    if !github_allow.is_empty() {
        warn_github(lookup, &github_allow);
    }
    let mut locked = store.lock()?;
    if locked.routes.iter().any(|route| route.name == args.name) {
        bail!("a route named `{}` exists already", args.name);
    }
    let secret = match secret {
        None if !args.no_secret => Some(Secret::File(crate::flow::setup::write_secret(
            store.home(),
            &args.name,
        )?)),
        secret => secret,
    };
    let route = Route {
        name: args.name,
        kind,
        prompt: args.prompt,
        secret,
        no_secret: args.no_secret,
        events,
        dir,
        allow_home: args.allow_home,
        deliver,
        paused: false,
        paused_for_home: false,
        created_at: unix_now(),
        last: None,
        last_event: None,
        github_allow,
        flow: None,
    };
    locked.routes.push(route.clone());
    locked.save()?;
    Ok(route)
}

/// Says what a route's `--github-allow` means and what keeps its writes
/// from running yet.
fn warn_github(lookup: &Lookup, allow: &[Write]) {
    eprintln!(
        "  !! runs of this route may write to GitHub without a question ({}): the pull \
         request bodies and comments that start them are anyone's text, and data tags do \
         not stop prompt injection",
        Write::join(allow)
    );
    match crate::github::Settings::read(|key| {
        lookup(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }) {
        Ok(settings) => {
            let missing: Vec<Write> = allow
                .iter()
                .copied()
                .filter(|write| !settings.unattended.contains(write))
                .collect();
            if !missing.is_empty() {
                eprintln!(
                    "  !! github_allow_unattended does not allow {}: those writes stay refused \
                     until it does (a write needs both)",
                    Write::join(&missing)
                );
            }
            if settings.repos.is_empty() {
                eprintln!(
                    "  !! github_repos is not set: no GitHub write runs without a question \
                     until it is"
                );
            }
        }
        Err(err) => eprintln!("  !! the GitHub settings are not readable: {err:#}"),
    }
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
    // gh in the run's commands finds no login there (removed after the run).
    let gh_config = crate::tools::GhConfigDir::new()
        .context("cannot make the run's GitHub CLI configuration folder");
    let refused = check_home_folder(
        lookup,
        &route.dir,
        route.allow_home,
        Some(("route", &route.name)),
    );
    let (outcome, prompt, quiet) = match (&refused, &route.flow, &gh_config) {
        (Err(err), _, _) => {
            let (note, first) = pause_refused(home, &route.name);
            // A request queued before the pause is recorded, not delivered.
            (Outcome::failed(format!("{err:#}; {note}")), None, !first)
        }
        (_, _, Err(err)) => (Outcome::failed(format!("{err:#}")), None, false),
        (Ok(()), Some(flow), Ok(gh)) => {
            let report = flow_run(home, lookup, &route, flow, incoming, gh.path(), &cancel).await;
            let outcome = Outcome {
                status: report.status,
                answer: report.text(),
                note: report.note(),
                session: report.session.clone(),
                tokens: report.tokens,
            };
            (outcome, report.prompt, report.quiet)
        }
        (Ok(()), None, Ok(gh)) => {
            let prompt = run::prompt(&route, incoming);
            let outcome = match &prompt {
                Err(err) => Outcome::failed(format!("cannot make the prompt: {err:#}")),
                Ok(prompt) => match prepare(&hide_secrets(home, lookup), &route).await {
                    Ok((engine, limits)) => {
                        run::execute(&*engine, &route, prompt, gh.path(), &cancel, limits).await
                    }
                    Err(err) => Outcome::failed(format!("{err:#}")),
                },
            };
            (outcome, prompt.ok(), false)
        }
    };
    drop(gh_config);
    let ended = Ended {
        run_id,
        started_at,
        prompt: prompt.as_deref(),
        outcome: &outcome,
        quiet,
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

/// `junees webhook allow-home <name>`: marks a route in the home folder
/// (or a folder holding it) like `--allow-home` would have, keeping its
/// secret, and resumes it when a run refused for that folder paused it.
/// A pause made with `webhook pause` stays. A route elsewhere is refused:
/// the mark would change nothing.
fn allow_home(store: &Store, lookup: &Lookup, name: &str) -> Result<()> {
    let mut locked = store.lock()?;
    let index = locked.find(name)?;
    let route = &mut locked.routes[index];
    let dir = escape_hidden(&route.dir.display().to_string());
    if !crate::tools::holds_home(&route.dir, user_home(lookup).as_deref()) {
        bail!(
            "route {} works in {dir}, which is not the home folder or a folder holding it: it \
             needs no allow-home",
            route.name
        );
    }
    route.allow_home = true;
    let resumed = std::mem::take(&mut route.paused_for_home);
    if resumed {
        route.paused = false;
    }
    let (name, paused) = (route.name.clone(), route.paused);
    locked.save()?;
    println!(
        "route {name} may work in the home folder now: the tools of its runs may change \
         everything in {dir} but the protected paths"
    );
    if resumed {
        println!("resumed route {name}");
    } else if paused {
        println!(
            "route {name} stays paused (paused with `junees webhook pause`; `junees webhook \
             resume {name}` resumes it)"
        );
    }
    Ok(())
}

/// `junees webhook disallow-home <name>`: takes the `allow-home` mark
/// back. A route whose folder is the home folder (or holds it) is paused
/// at once (requests get 503), as its next run would be refused;
/// `allow-home` resumes it.
fn disallow_home(store: &Store, lookup: &Lookup, name: &str) -> Result<()> {
    let mut locked = store.lock()?;
    let index = locked.find(name)?;
    let route = &mut locked.routes[index];
    let name = route.name.clone();
    if !route.allow_home {
        println!("route {name} has no allow-home mark");
        return Ok(());
    }
    route.allow_home = false;
    let pause = !route.paused && crate::tools::holds_home(&route.dir, user_home(lookup).as_deref());
    if pause {
        route.paused = true;
        route.paused_for_home = true;
    }
    locked.save()?;
    println!("route {name} may no longer work in the home folder");
    if pause {
        println!(
            "paused route {name}: it works in the home folder (or a folder holding it); \
             `junees webhook allow-home {name}` resumes it"
        );
    }
    Ok(())
}

/// Pauses or resumes `route` as `junees webhook pause|resume` (and the
/// dashboard) do. A pause by hand is the user's; a resume ends either
/// kind.
pub fn set_paused(route: &mut super::store::Route, pause: bool) {
    route.paused = pause;
    route.paused_for_home = false;
}

/// Pauses route `name` after [`check_home_folder`] refused its run, so
/// the server answers its requests 503 instead of starting runs: a route
/// saved in the home folder before `--allow-home` existed fails and
/// delivers that once, not at every request. What to add to the run's
/// note, and whether this run paused it (it was not paused already).
fn pause_refused(home: &Path, name: &str) -> (String, bool) {
    let paused = Store::new(home).lock().and_then(|mut locked| {
        let Some(route) = locked.routes.iter_mut().find(|route| route.name == name) else {
            return Ok(false);
        };
        let first = !route.paused;
        // A pause the user made stays theirs: `allow-home` keeps it.
        route.paused_for_home |= first;
        route.paused = true;
        locked.save()?;
        Ok(first)
    });
    match paused {
        Ok(first) => (
            "the route is paused so it does not fail again at every request".into(),
            first,
        ),
        Err(err) => (format!("cannot pause the route: {err:#}"), true),
    }
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

/// Makes the engines of a flow's model runs: like a webhook run's, in the
/// folder the step works in.
struct FlowEngines {
    lookup: Lookup,
}

#[async_trait::async_trait(?Send)]
impl crate::flow::run::Engines for FlowEngines {
    async fn engine(&self, dir: &Path) -> Result<Box<dyn crate::cron::run::Engine>> {
        if crate::claude_code::Backend::from_lookup(|key| (self.lookup)(key))?
            == crate::claude_code::Backend::ClaudeCode
        {
            bail!(
                "GitHub flows need the api backend: the claude-code backend has no `github` tool \
                 whose writes the route limits"
            );
        }
        engine_in(&self.lookup, dir).await
    }
}

/// The variables a flow's test command (PR code) does not get: the
/// `hidden_env` of [`crate::config::Config`], which adds the header
/// variables of mcp.json to the list in `lookup`.
fn flow_tools(lookup: &Lookup) -> Result<crate::tools::ToolSettings> {
    Ok(crate::config::Config::from_lookup_without_key(|key| lookup(key))?.tools)
}

/// Runs the GitHub flow `flow` of `route` for `incoming`.
async fn flow_run(
    home: &Path,
    lookup: &Lookup,
    route: &Route,
    flow: &crate::flow::FlowConfig,
    incoming: &Incoming,
    gh_config: &Path,
    cancel: &Cancel,
) -> crate::flow::run::Report {
    let lookup = hide_secrets(home, lookup);
    let prepared = (|| -> Result<_> {
        let settings = crate::github::Settings::read(|key| {
            lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })?;
        let token = settings.token.clone().with_context(|| {
            format!(
                "GitHub flows need github_token ({})",
                crate::github::TOKEN_ENV
            )
        })?;
        let repo = crate::github::Repo::parse(&flow.repo).context("the route's repository")?;
        if !settings.repos.contains(&repo) {
            bail!(
                "github_repos ({}) must list {repo} for the flow to use it",
                crate::github::REPOS_ENV
            );
        }
        let client = crate::github::Client::new(&settings.api_url, token)?;
        let merge = crate::flow::merge_enabled(&*lookup)?;
        let limits = super::limits(&*lookup)?;
        let tools = flow_tools(&lookup)?;
        Ok((settings.unattended, client, merge, limits, tools))
    })();
    let (unattended, client, merge_setting, limits, tools) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => return crate::flow::run::Report::failed(format!("{err:#}")),
    };
    let engines = FlowEngines {
        lookup: Arc::clone(&lookup),
    };
    let sandbox = crate::flow::address::test_sandbox(&tools, gh_config);
    let cx = crate::flow::run::Context {
        home,
        route,
        flow,
        client: &client,
        engines: &engines,
        cancel,
        limits,
        merge_setting,
        unattended: &unattended,
        hidden: &tools.hidden_env,
        sandbox: sandbox.as_ref(),
        gh_config,
        now: unix_now(),
    };
    crate::flow::run::handle(&cx, incoming).await
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
            None if !route.no_secret => eprintln!(
                "  !! route {} has no secret and was not added with --no-secret: its requests \
                 are refused (403); remove it and add it again (`junees webhook add` makes a \
                 secret)",
                route.name
            ),
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
    Arc::new(Server::new(home, local, Arc::new(runner), env, log))
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
        // Secret files: in `$JUNEES_HOME/secrets/` and private.
        let secrets = home.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        let private = |path: &Path, mode: u32| {
            std::fs::write(path, "x").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let secret_file = secrets.join("open");
        private(&secret_file, 0o640);
        let outside = home.path().join("outside");
        private(&outside, 0o600);
        let linked = secrets.join("link");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        let good = secrets.join("good");
        private(&good, 0o600);
        let mut filed = args("filed");
        filed.secret_env = None;
        filed.secret_file = Some(good.clone());
        let route = new_route(&store, &settings, filed).unwrap();
        assert_eq!(
            route.secret,
            Some(Secret::File(std::fs::canonicalize(&good).unwrap()))
        );
        // Without a secret option: a random private secret file, like
        // add-github's; unsigned only when asked for.
        let mut made = args("made");
        made.secret_env = None;
        let route = new_route(&store, &settings, made).unwrap();
        let path = secrets.join("webhook-made");
        assert_eq!(route.secret, Some(Secret::File(path.clone())));
        assert!(!route.no_secret);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim().len(), 64);
        let mut open = args("open-route");
        open.secret_env = None;
        open.no_secret = true;
        let route = new_route(&store, &settings, open).unwrap();
        assert_eq!(route.secret, None);
        assert!(route.no_secret);
        assert!(!secrets.join("webhook-open-route").exists());
        // A taken name does not overwrite that route's secret file.
        std::fs::write(&path, "kept").unwrap();
        let mut again = args("made");
        again.secret_env = None;
        assert!(new_route(&store, &settings, again).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
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
                Box::new(move |a: &mut AddArgs| {
                    a.secret_env = None;
                    a.secret_file = Some(outside.clone());
                }),
                "must be in",
            ),
            (
                Box::new(move |a: &mut AddArgs| {
                    a.secret_env = None;
                    a.secret_file = Some(linked.clone());
                }),
                "must be in",
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
            (
                Box::new(|a: &mut AddArgs| a.github_allow = Some("comment,delete".into())),
                "--github-allow: unknown write `delete`",
            ),
            (
                Box::new(|a: &mut AddArgs| a.github_allow = Some(" , ".into())),
                "--github-allow is empty",
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
        assert_eq!(store.routes().unwrap().len(), 4);
    }

    #[test]
    fn routes_allow_no_github_write_unless_added_with_github_allow() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let settings = lookup(&[]);
        let args = |name: &str| AddArgs {
            name: name.into(),
            prompt: "review {{payload.number}}".into(),
            secret_env: Some("PR_HOOK_SECRET".into()),
            dir: Some(home.path().to_path_buf()),
            ..AddArgs::default()
        };
        let plain = new_route(&store, &settings, args("plain")).unwrap();
        assert!(plain.github_allow.is_empty());
        let mut allowing = args("review");
        allowing.github_allow = Some("Review, comment".into());
        new_route(&store, &settings, allowing).unwrap();
        let routes = store.routes().unwrap();
        assert_eq!(routes[1].github_allow, [Write::Review, Write::Comment]);
        let text = std::fs::read_to_string(home.path().join("webhooks/routes.json")).unwrap();
        assert!(
            text.contains("\"github_allow\": [\n        \"review\",\n        \"comment\"\n      ]"),
            "{text}"
        );
        assert_eq!(text.matches("github_allow").count(), 1, "{text}");
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
    fn a_flow_test_command_gets_no_mcp_header_variable() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::new(home.path());
        let mut locked = store.lock().unwrap();
        let mut route = crate::webhook::store::tests::route("a");
        route.secret = Some(Secret::Env("A_SECRET".into()));
        locked.routes = vec![route];
        locked.save().unwrap();
        drop(locked);
        let mcp = home.path().join(crate::mcp::config::FILE_NAME);
        std::fs::write(
            &mcp,
            r#"{"servers": {"docs": {"url": "https://docs.example.com/mcp",
                "headers": {"Authorization": "Bearer env:DOCS_TOKEN"}}}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&mcp, std::fs::Permissions::from_mode(0o600)).unwrap();
        let home_text = home.path().to_str().unwrap();
        let lookup = hide_secrets(
            home.path(),
            &lookup(&[("JUNEES_HOME", home_text), (HIDDEN_ENV, "DB_PASSWORD")]),
        );
        assert_eq!(
            flow_tools(&lookup).unwrap().hidden_env,
            ["DB_PASSWORD", "A_SECRET", "DOCS_TOKEN"]
        );
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
