use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use junees::agent::{Agent, AgentOptions, Cancelled, Event};
use junees::approval::{self, SharedLines};
use junees::cancel::Cancel;
use junees::claude_code::chat::Chat;
use junees::claude_code::{BACKEND_ENV, Backend, ClaudeCode, MODEL_ENV, UsageLimit};
use junees::cli::{self, Command, SessionMode, TaskCommand, TaskRun};
use junees::config::Config;
use junees::config_file::Settings;
use junees::notify::Notifier;
use junees::prompt;
use junees::provider::anthropic::AnthropicProvider;
use junees::provider::{Provider, ProviderError};
use junees::session::{self, Journal, Session, SessionWriter, Transcript};
use junees::setup::{self, EchoOff, Prompter};
use junees::skills;
use junees::slash::{self, Slash};
use junees::task::budget::Budget;
use junees::task::state::StopReason;
use junees::task::{self, Report, TaskEnv};
use junees::tools::ToolRegistry;
use junees::web_search;
use tokio::signal::unix::{Signal, SignalKind, signal};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PREVIEW_CHARS: usize = 120;
const SESSIONS_LISTED: usize = 10;

fn main() -> ExitCode {
    protect_environment();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(async {
        match run().await {
            Ok(code) => code,
            Err(err) => {
                report(&err);
                ExitCode::FAILURE
            }
        }
    });
    // After Ctrl-C at the prompt a read of stdin is still waiting on a
    // blocking thread; dropping the runtime normally would wait for it.
    runtime.shutdown_background();
    code
}

/// Makes this process non-dumpable. Hiding the API key and the webhook
/// from bash's environment is not enough while the environment of junees
/// itself is readable at `/proc/<pid>/environ` (bash's `$PPID`) by every
/// process of the same user. A non-dumpable process's `/proc` files belong
/// to root, so its `environ` and `mem` cannot be read, nor can it be
/// traced. Children get a dumpable process again when they exec, so the
/// tools work as before; only junees's own core dumps are lost.
#[cfg(target_os = "linux")]
fn protect_environment() {
    // SAFETY: PR_SET_DUMPABLE only changes a flag of this process and
    // touches no memory of ours.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        eprintln!(
            "  !! cannot hide the environment of junees from other processes: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn protect_environment() {}

async fn run() -> Result<ExitCode> {
    let command = cli::parse_args(std::env::args().skip(1))?;
    match command {
        Command::Help => {
            print!("{}", cli::HELP);
            return Ok(ExitCode::SUCCESS);
        }
        Command::Version => {
            println!("junees {VERSION}");
            return Ok(ExitCode::SUCCESS);
        }
        // Needs no API key, and writes nothing.
        Command::Task(TaskCommand::Status { json }) => return task_status(json),
        _ => {}
    }
    let mut settings = if command == Command::Setup {
        // A broken file must not block the command that rewrites it.
        let (settings, broken) = Settings::load_for_setup(|key| std::env::var(key).ok());
        if let (Some(err), Some(path)) = (broken, settings.path()) {
            eprintln!(
                "{} is broken ({}). Setup starts from scratch and replaces it when \
                 you finish; Ctrl-C keeps the old file.",
                path.display(),
                err.root_cause()
            );
        }
        settings
    } else {
        Settings::from_env()?
    };
    // Shared by the setup questions, the prompt and the approval questions.
    let input = SharedLines::stdin();
    if command == Command::Setup {
        if !on_terminal() {
            bail!(
                "`junees setup` asks its questions on a terminal; run it in one, or write \
                 the settings file yourself (see README)"
            );
        }
        setup(&mut settings, &input).await?;
        eprintln!(
            "Ready. Try:\n  junees                          talk with the agent\n  \
             junees \"what is in this folder?\"  ask once\n  \
             junees task \"<goal>\"            a long task, in a git repository"
        );
        return Ok(ExitCode::SUCCESS);
    }
    if wants_first_setup(&command, &settings) {
        setup(&mut settings, &input).await?;
    }
    match command {
        Command::Run(args) => conversation(args, settings, input).await,
        Command::Task(command) => run_task(command, &settings).await,
        Command::Setup | Command::Help | Command::Version => unreachable!("handled above"),
    }
}

/// Whether questions can be asked: both stdin and stderr are terminals.
fn on_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// The first run asks the setup questions: no settings file, no
/// `ANTHROPIC_API_KEY`, a terminal to ask on, and a command that needs a
/// key (nothing on the claude-code backend does).
fn wants_first_setup(command: &Command, settings: &Settings) -> bool {
    if settings.file_exists()
        || settings.path().is_none()
        || settings.env("ANTHROPIC_API_KEY").is_some()
    {
        return false;
    }
    let backend = match command {
        Command::Task(task) => task
            .backend()
            .map_or_else(|| Backend::from_lookup(|key| settings.get(key)), Ok),
        _ => Backend::from_lookup(|key| settings.get(key)),
    };
    if matches!(backend, Ok(Backend::ClaudeCode)) {
        return false;
    }
    on_terminal()
}

/// Asks the setup questions and saves the answers.
async fn setup(settings: &mut Settings, input: &SharedLines) -> Result<()> {
    let path = settings
        .path()
        .context("neither JUNEES_HOME nor HOME is set, so there is nowhere to keep the settings")?
        .to_path_buf();
    let claude = ClaudeCode::from_lookup(|key| settings.get(key));
    let mut terminal = TerminalPrompter {
        input,
        signals: Signals::install()?,
    };
    let file = setup::wizard(&mut terminal, &claude, settings.file().clone(), &path).await?;
    file.save(&path)?;
    let has_key = file.get("api_key").is_some();
    settings.replace_file(file);
    eprintln!("Saved {}.", path.display());
    for (key, name) in [("api_key", "ANTHROPIC_API_KEY"), ("backend", BACKEND_ENV)] {
        if settings.env(name).is_some() && (key != "api_key" || has_key) {
            eprintln!("Note: {name} in your environment wins over `{key}` in the file.");
        }
    }
    Ok(())
}

/// The setup questions on the terminal: questions on stderr, answers from
/// the lines the conversation reads later. Ctrl-C cancels the setup.
struct TerminalPrompter<'a> {
    input: &'a SharedLines,
    signals: Signals,
}

impl TerminalPrompter<'_> {
    async fn read(&mut self) -> Result<Option<String>> {
        tokio::select! {
            line = self.input.next_line() => Ok(line?),
            _ = self.signals.next() => {
                eprintln!();
                bail!("setup cancelled; nothing was saved")
            }
        }
    }
}

#[async_trait]
impl Prompter for TerminalPrompter<'_> {
    fn say(&mut self, text: &str) {
        eprintln!("{text}");
    }

    async fn line(&mut self, question: &str) -> Result<Option<String>> {
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        self.read().await
    }

    async fn secret(&mut self, question: &str) -> Result<Option<String>> {
        let quiet = EchoOff::new(libc::STDIN_FILENO).context("cannot turn off the echo")?;
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        let answer = self.read().await;
        drop(quiet);
        // The Enter that ended the answer did not show either.
        eprintln!();
        answer
    }
}

/// What runs the conversation: the `backend` setting.
enum Engine {
    /// Junees's own agent on the Anthropic API.
    Api(Box<Agent>),
    /// Claude Code, signed in with the user's subscription.
    Claude(Box<Chat>),
}

impl Engine {
    fn backend(&self) -> Backend {
        match self {
            Engine::Api(_) => Backend::Api,
            Engine::Claude(_) => Backend::ClaudeCode,
        }
    }

    /// The `backend` of this engine's session journals.
    fn session_backend(&self) -> Option<&'static str> {
        session_backend(self.backend())
    }

    /// The model a new session's meta line names.
    fn model(&self, config: &Config) -> String {
        match self {
            Engine::Api(_) => config.model.clone(),
            Engine::Claude(chat) => chat.claude().model.clone().unwrap_or_default(),
        }
    }

    /// Runs one input.
    async fn turn(&mut self, input: &str, on_event: impl FnMut(Event<'_>)) -> Result<()> {
        match self {
            Engine::Api(agent) => agent.run(input, on_event).await.map(drop),
            Engine::Claude(chat) => chat.turn(input, on_event).await,
        }
    }

    fn resume(&mut self, transcript: Transcript, journal: Option<Box<dyn Journal>>) {
        match self {
            Engine::Api(agent) => agent.resume(transcript, journal),
            Engine::Claude(chat) => chat.resume(transcript, journal),
        }
    }

    fn reset(&mut self, journal: Option<Box<dyn Journal>>) {
        match self {
            Engine::Api(agent) => agent.reset(journal),
            Engine::Claude(chat) => chat.reset(journal),
        }
    }
}

/// The `backend` of the session journals a backend writes.
fn session_backend(backend: Backend) -> Option<&'static str> {
    match backend {
        Backend::Api => None,
        Backend::ClaudeCode => Some(session::CLAUDE_CODE),
    }
}

fn make_provider(config: &Config) -> Result<Box<dyn Provider>> {
    Ok(Box::new(
        AnthropicProvider::new(
            config.api_key.clone(),
            config.base_url.clone(),
            config.model.clone(),
        )?
        .with_thinking(config.thinking_budget)
        .with_streaming(config.stream)
        .with_web_search(config.web_search.clone()),
    ))
}

/// Junees's own agent for the conversation. It asks before risky tool
/// calls when `input` is given (the prompt) and the settings want it.
fn api_agent(
    config: &Config,
    cwd: &Path,
    cancel: &Cancel,
    input: Option<&SharedLines>,
) -> Result<Agent> {
    let mut tools = ToolRegistry::with_defaults(&config.tools);
    skills::register(&mut tools, config.home.as_deref(), true);
    let mut agent = Agent::new(
        make_provider(config)?,
        tools,
        AgentOptions {
            system: conversation_system(config, cwd),
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
            context_window: config.context_window,
            compact_threshold: config.compact_threshold,
        },
    )
    .with_cancel(cancel.clone());
    if let Some(gate) = input.and_then(|input| approval::for_conversation(config, cwd, input)) {
        agent = agent.with_gate(Box::new(gate));
    }
    Ok(agent)
}

/// The system prompt of a conversation on the API backend, with the
/// skills as they are now.
fn conversation_system(config: &Config, cwd: &Path) -> String {
    prompt::system_prompt(cwd)
        + &skills::system_section(config.home.as_deref(), skills::Mode::Manage)
}

/// What a conversation on the claude-code backend appends to Claude
/// Code's system prompt: the skill list as it is now.
fn claude_system(config: &Config) -> String {
    skills::system_section(config.home.as_deref(), skills::Mode::ClaudeCode)
}

/// Claude Code for the conversation, once `claude --version` and its
/// sign-in are checked; also returns the version it printed.
async fn claude_chat(
    settings: &Settings,
    config: &Config,
    cwd: &Path,
    cancel: &Cancel,
) -> Result<(Chat, String)> {
    let claude = ClaudeCode::from_lookup(|key| settings.get(key));
    let version = claude.check(cwd, &config.tools).await?;
    for notice in claude.notices() {
        eprintln!("  !! {notice}");
    }
    // `-p` has nobody to ask (`--permission-prompts none`): the allowed
    // tools run without the questions the API backend asks.
    eprintln!(
        "(Claude Code runs commands without Junees approval questions; \
         only the command policy deny rules apply)"
    );
    let mut chat = Chat::new(
        claude,
        cwd.to_path_buf(),
        config.tools.clone(),
        config.max_turns,
        cancel.clone(),
    );
    chat.set_system(claude_system(config));
    Ok((chat, version))
}

/// The settings of the conversation: the API backend needs a key, the
/// claude-code backend none.
fn conversation_config(settings: &Settings, backend: Backend) -> Result<Config> {
    let lookup = |key: &str| settings.get(key);
    match backend {
        Backend::Api => Config::from_lookup(lookup),
        Backend::ClaudeCode => Config::from_lookup_without_key(lookup),
    }
}

/// `junees`, `junees "<question>"` and their options, on the backend the
/// settings name.
async fn conversation(
    args: cli::RunArgs,
    settings: Settings,
    input: SharedLines,
) -> Result<ExitCode> {
    let backend = Backend::from_lookup(|key| settings.get(key))?;
    let config = conversation_config(&settings, backend)?;
    let cwd = std::env::current_dir()?;
    let cancel = Cancel::new();
    let interactive = args.prompt.is_none();
    let (mut engine, banner) = match backend {
        Backend::Api => {
            let agent = api_agent(&config, &cwd, &cancel, interactive.then_some(&input))?;
            (Engine::Api(Box::new(agent)), config.model.clone())
        }
        Backend::ClaudeCode => {
            let (chat, version) = claude_chat(&settings, &config, &cwd, &cancel).await?;
            let model = chat.claude().model.clone();
            let banner = format!(
                "Claude Code {version}, model {}",
                model.as_deref().unwrap_or("of Claude Code's choice")
            );
            (Engine::Claude(Box::new(chat)), banner)
        }
    };
    let opened = open_session(
        &args.session,
        &config,
        &cwd,
        &engine.model(&config),
        engine.session_backend(),
    )?;
    engine.resume(opened.transcript, opened.journal);
    let signals = Signals::install()?;

    match args.prompt {
        Some(prompt) => {
            let received = cancel_on_signals(signals, cancel);
            // stdout carries only the answer.
            eprintln!("{}", opened.status);
            let mut printer = Printer::new();
            let result = engine.turn(&prompt, |event| printer.event(event)).await;
            printer.end_line();
            match result {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                    let signal = received.load(Ordering::SeqCst);
                    eprintln!("interrupted by {}", signal_name(signal));
                    Ok(signal_exit(signal))
                }
                Err(err) => Err(err),
            }
        }
        None => {
            println!("junees {VERSION} ({banner}). /help for commands, /exit to quit.");
            println!("{}", opened.status);
            let mut repl = Repl {
                engine,
                config,
                settings,
                cwd,
                saving: args.session != SessionMode::Off,
                session_id: opened.id,
                cancel,
                signals,
                input,
            };
            repl.run().await
        }
    }
}

/// `junees status`, for the git repository the current folder is in.
fn task_status(json: bool) -> Result<ExitCode> {
    let root = task::git::toplevel(&std::env::current_dir()?)?;
    let status = task::status::load(&root)?;
    if json {
        println!("{}", status.to_json());
    } else {
        print!("{}", status.to_text());
    }
    Ok(ExitCode::SUCCESS)
}

/// SIGINT (Ctrl-C) and SIGTERM, taken over from their default action of
/// ending Junees on the spot.
struct Signals {
    interrupt: Signal,
    terminate: Signal,
}

impl Signals {
    fn install() -> Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("cannot handle SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("cannot handle SIGTERM")?,
        })
    }

    /// Waits for the next one and returns its number.
    async fn next(&mut self) -> i32 {
        tokio::select! {
            _ = self.interrupt.recv() => libc::SIGINT,
            _ = self.terminate.recv() => libc::SIGTERM,
        }
    }
}

/// For runs without a prompt to return to (`-p`, task mode): the first
/// signal sets `cancel`, so the work stops cleanly; a second one exits at
/// once. Returns where the first signal's number is kept (0 until then).
fn cancel_on_signals(mut signals: Signals, cancel: Cancel) -> Arc<AtomicI32> {
    let received = Arc::new(AtomicI32::new(0));
    let first = Arc::clone(&received);
    tokio::spawn(async move {
        loop {
            let signal = signals.next().await;
            if first
                .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                eprintln!("  !! {} again: quitting at once", signal_name(signal));
                std::process::exit(128 + signal);
            }
            eprintln!(
                "  !! {}: stopping the current step (send it again to quit at once)",
                signal_name(signal)
            );
            cancel.cancel();
        }
    });
    received
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    }
}

/// The shell's exit status for a process ended by `signal`: 130 for
/// SIGINT, 143 for SIGTERM.
fn signal_exit(signal: i32) -> ExitCode {
    let signal = if signal == 0 { libc::SIGINT } else { signal };
    ExitCode::from(u8::try_from(128 + signal).unwrap_or(1))
}

/// `junees task ...` from the command line. SIGINT/SIGTERM stop the
/// session that runs; `task run` then checkpoints it, records the stop and
/// exits with 130/143.
async fn run_task(command: TaskCommand, settings: &Settings) -> Result<ExitCode> {
    let cancel = Cancel::new();
    let received = cancel_on_signals(Signals::install()?, cancel.clone());
    match task_work(&command, settings, cancel).await? {
        TaskEnd::Done => {
            if matches!(command, TaskCommand::Init { .. }) {
                println!("work on it with `junees task`");
            }
            Ok(ExitCode::SUCCESS)
        }
        TaskEnd::Interrupted => Ok(signal_exit(received.load(Ordering::SeqCst))),
    }
}

/// How a task command that did not fail ended.
enum TaskEnd {
    Done,
    /// Stopped by `cancel`.
    Interrupted,
}

/// `task init`, `task run`, or both (`junees task "<goal>"`), in the root
/// of the git repository the current folder is in (the process moves
/// there). `cancel` stops the session that runs.
async fn task_work(command: &TaskCommand, settings: &Settings, cancel: Cancel) -> Result<TaskEnd> {
    let lookup = |key: &str| settings.get(key);
    let backend = match command.backend() {
        Some(backend) => backend,
        None => Backend::from_lookup(lookup)?,
    };
    let (config, claude) = match backend {
        Backend::Api => (Config::from_lookup(lookup)?, None),
        Backend::ClaudeCode => (
            Config::from_lookup_without_key(lookup)?,
            Some(ClaudeCode::from_lookup(lookup)),
        ),
    };
    let root = task::git::toplevel(&std::env::current_dir()?)?;
    // The tools resolve relative paths from the process's folder.
    std::env::set_current_dir(&root)
        .with_context(|| format!("cannot change to {}", root.display()))?;
    let provider = || make_provider(&config);
    let budget = match command {
        TaskCommand::Run(run) | TaskCommand::Start { run, .. } => Budget {
            max_tokens: run.max_tokens.or(config.task_max_tokens),
            max_time: run.max_time.or(config.task_max_time),
            ..Budget::default()
        },
        TaskCommand::Init { .. } | TaskCommand::Status { .. } => Budget::default(),
    };
    let env = TaskEnv {
        root,
        home: config.home.clone(),
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        max_turns: config.max_turns,
        tool_settings: config.tools.clone(),
        context_window: config.context_window,
        compact_threshold: config.compact_threshold,
        provider: &provider,
        budget,
        cancel,
        notify: Notifier::from_lookup(lookup)?,
        claude,
        skill_manage: config.task_skill_manage,
    };
    let mut printer = Printer::new();
    let mut reporter = |report: Report<'_>| match report {
        Report::Line(line) => {
            printer.end_line();
            println!("{line}");
        }
        Report::Event(event) => printer.event(event),
    };
    let (goal, run) = match command {
        TaskCommand::Init { goal, .. } => (Some(goal), None),
        TaskCommand::Run(run) => (None, Some(run)),
        TaskCommand::Start { goal, run } => (Some(goal), Some(run)),
        TaskCommand::Status { json } => {
            task_status(*json)?;
            return Ok(TaskEnd::Done);
        }
    };
    if let Some(goal) = goal {
        match task::init(&env, goal, &mut reporter).await {
            Ok(()) => {}
            Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                report(&err);
                return Ok(TaskEnd::Interrupted);
            }
            Err(err) => return Err(err),
        }
    }
    let Some(TaskRun { max_sessions, .. }) = run else {
        return Ok(TaskEnd::Done);
    };
    let stop = task::run(&env, *max_sessions, &mut reporter).await?;
    if stop.reason.is_failure() {
        bail!("task stopped: {stop}");
    }
    println!("task stopped: {stop}");
    if stop.reason == StopReason::Interrupted {
        return Ok(TaskEnd::Interrupted);
    }
    Ok(TaskEnd::Done)
}

/// The conversation to start from and where to record it.
struct Opened {
    transcript: Transcript,
    journal: Option<Box<dyn Journal>>,
    id: Option<String>,
    /// One line for the user about which session this is.
    status: String,
}

/// The session `mode` asks for, of the conversations `backend` runs
/// ([`session::Meta::backend`]); `model` goes into a new one's meta line.
fn open_session(
    mode: &SessionMode,
    config: &Config,
    cwd: &Path,
    model: &str,
    backend: Option<&str>,
) -> Result<Opened> {
    let path = match mode {
        SessionMode::Off => {
            return Ok(Opened {
                transcript: Transcript::default(),
                journal: None,
                id: None,
                status: "session not saved (--no-session)".into(),
            });
        }
        SessionMode::New => None,
        SessionMode::Continue => {
            let home = config
                .require_home()
                .context("cannot continue a saved session")?;
            let latest = session::find_latest(home, cwd, backend)?;
            if latest.is_none() {
                eprintln!(
                    "no earlier session in {}; starting a new one",
                    cwd.display()
                );
            }
            latest
        }
        SessionMode::Resume(prefix) => {
            let home = config
                .require_home()
                .context("cannot resume a saved session")?;
            Some(session::find_by_prefix(home, prefix)?)
        }
    };

    let Some(path) = path else {
        return Ok(new_session(config, cwd, model, backend));
    };
    resume_session(&path, cwd, backend)
}

/// The saved session at `path`, locked for this process. It has to be a
/// conversation of `backend`: Claude Code cannot see a conversation held by
/// Junees, nor Junees's agent one held by Claude Code.
fn resume_session(path: &Path, cwd: &Path, backend: Option<&str>) -> Result<Opened> {
    if let Some(meta) = session::read_meta(path)
        && meta.backend.as_deref() != backend
    {
        let (ran, switch) = match meta.backend.as_deref() {
            Some(other) => (format!("by {other}"), other.to_string()),
            None => ("on the API".to_string(), "api".to_string()),
        };
        bail!(
            "session {} was run {ran}, and this conversation runs on {}; continue it with \
             `/backend {switch}` or JUNEES_BACKEND={switch}",
            meta.id,
            backend.unwrap_or("the API")
        );
    }
    let (session, writer) = Session::resume(path)?;
    if let Some(warning) = &session.warning {
        eprintln!("  !! {warning}");
    }
    // Relative paths in the history mean the folder it started in.
    let elsewhere = if session.meta.cwd == cwd.to_string_lossy() {
        String::new()
    } else {
        format!(", started in {}", session.meta.cwd)
    };
    let size = if session.meta.backend.is_some() {
        format!("{} Claude Code turns", session.transcript.claude_turns)
    } else {
        format!("{} messages", session.transcript.messages.len())
    };
    Ok(Opened {
        status: format!("resumed {} ({size}{elsewhere})", session.meta.id),
        id: Some(session.meta.id),
        transcript: session.transcript,
        journal: Some(Box::new(writer)),
    })
}

/// A fresh saved session. Without a state folder, or if its file cannot be
/// created, Junees still runs, just without saving.
fn new_session(config: &Config, cwd: &Path, model: &str, backend: Option<&str>) -> Opened {
    let created = config
        .require_home()
        .and_then(|home| SessionWriter::create_for(home, cwd, model, backend));
    match created {
        Ok((meta, writer)) => Opened {
            transcript: Transcript::default(),
            journal: Some(Box::new(writer)),
            status: format!("session {}", meta.id),
            id: Some(meta.id),
        },
        Err(err) => Opened {
            transcript: Transcript::default(),
            journal: None,
            id: None,
            status: format!("session not saved: {err:#}"),
        },
    }
}

struct Repl {
    engine: Engine,
    config: Config,
    /// Where `/task` gets its settings; `/model` and `/backend` change
    /// them for this process only.
    settings: Settings,
    cwd: PathBuf,
    /// False with --no-session, so /new starts another unsaved session.
    saving: bool,
    session_id: Option<String>,
    /// Set to cancel the turn that is running.
    cancel: Cancel,
    signals: Signals,
    input: SharedLines,
}

impl Repl {
    /// Reads and handles input until /exit, Ctrl-D or two Ctrl-C in a row
    /// at the prompt. Ctrl-C during a turn cancels just that turn; SIGTERM
    /// cancels it and quits.
    async fn run(&mut self) -> Result<ExitCode> {
        let mut interrupted = false;
        loop {
            print!("> ");
            std::io::stdout().flush()?;
            let line = tokio::select! {
                line = self.input.next_line() => line?,
                signal = self.signals.next() => {
                    println!();
                    if signal == libc::SIGTERM {
                        return Ok(signal_exit(signal));
                    }
                    if interrupted {
                        return Ok(ExitCode::SUCCESS);
                    }
                    interrupted = true;
                    println!("(press Ctrl-C again or Ctrl-D to quit)");
                    continue;
                }
            };
            interrupted = false;
            let Some(line) = line else {
                println!();
                return Ok(ExitCode::SUCCESS);
            };
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            let terminated = match slash::parse(input) {
                None => self.turn(input).await,
                Some(Slash::Exit) => return Ok(ExitCode::SUCCESS),
                Some(command) => self.command(command).await,
            };
            if terminated {
                return Ok(signal_exit(libc::SIGTERM));
            }
        }
    }

    /// Runs a slash command; returns whether SIGTERM came meanwhile.
    async fn command(&mut self, command: Slash) -> bool {
        match command {
            Slash::Help => println!("{}", cli::REPL_HELP),
            Slash::New => self.reset(),
            Slash::Sessions => return self.sessions().await,
            Slash::Status => {
                if let Err(err) = task_status(false) {
                    report(&err);
                }
            }
            Slash::Task(goal) => return self.task(goal).await,
            Slash::Model(model) => self.model(model),
            Slash::Backend(backend) => self.backend(backend).await,
            Slash::Usage => match &self.engine {
                Engine::Api(agent) => {
                    let usage = agent.usage();
                    println!(
                        "input {} tokens (cache write {}, cache read {}), output {} tokens, \
                         web searches {}",
                        usage.input_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_read_input_tokens,
                        usage.output_tokens,
                        usage.web_searches()
                    );
                }
                Engine::Claude(chat) => {
                    let usage = chat.usage();
                    println!(
                        "input {} tokens (cache write {}, cache read {}), output {} tokens, \
                         {} Claude Code turns{}",
                        usage.input_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_read_input_tokens,
                        usage.output_tokens,
                        chat.turns(),
                        chat.claude_session()
                            .map_or_else(String::new, |id| format!(", Claude Code session {id}"))
                    );
                }
            },
            // Nothing is recorded until the summary is back, so the
            // compaction can simply be dropped.
            Slash::Compact => match &mut self.engine {
                Engine::Api(agent) => {
                    return tokio::select! {
                        _ = agent.compact(print_event) => false,
                        signal = self.signals.next() => {
                            eprintln!("(compaction cancelled)");
                            signal == libc::SIGTERM
                        }
                    };
                }
                Engine::Claude(_) => eprintln!(
                    "/compact is not available on the claude-code backend: Claude Code \
                     manages the conversation's context and compacts it by itself"
                ),
            },
            Slash::Context => match &self.engine {
                Engine::Api(agent) => {
                    let options = agent.options();
                    println!(
                        "context {} / {} tokens; old tool output is cleared from {} tokens",
                        agent.context_tokens(),
                        options.context_window,
                        options.compact_limit()
                    );
                }
                Engine::Claude(_) => eprintln!(
                    "/context is not available on the claude-code backend: Claude Code \
                     keeps the context (and compacts it by itself); /usage shows the tokens \
                     used"
                ),
            },
            Slash::Skills => self.skills(),
            Slash::Skill(name) => self.skill(&name),
            Slash::Bad(message) => eprintln!("{message}"),
            Slash::Exit => {}
        }
        false
    }

    /// Runs one input. A signal cancels the turn; returns whether it was
    /// SIGTERM, which also quits.
    async fn turn(&mut self, input: &str) -> bool {
        self.cancel.reset();
        let mut printer = Printer::new();
        let mut terminated = false;
        let result = {
            let run = self.engine.turn(input, |event| printer.event(event));
            tokio::pin!(run);
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    signal = self.signals.next() => {
                        terminated |= signal == libc::SIGTERM;
                        self.cancel.cancel();
                    }
                }
            }
        };
        printer.end_line();
        match result {
            Ok(()) => {}
            Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                eprintln!("(turn cancelled)");
            }
            Err(err) => report(&err),
        }
        terminated
    }

    /// `/task [<goal>]`: like `junees task ["<goal>"]` in this folder, on
    /// the backend `/backend` picked. A signal stops the task the way it
    /// stops `junees task`; returns whether it was SIGTERM.
    async fn task(&mut self, goal: Option<String>) -> bool {
        let command = match goal {
            Some(goal) => TaskCommand::Start {
                goal,
                run: TaskRun::default(),
            },
            None => TaskCommand::Run(TaskRun::default()),
        };
        eprintln!("(task mode asks no approval questions; only the command policy applies)");
        self.cancel.reset();
        let mut terminated = false;
        let result = {
            let work = task_work(&command, &self.settings, self.cancel.clone());
            tokio::pin!(work);
            loop {
                tokio::select! {
                    result = &mut work => break result,
                    signal = self.signals.next() => {
                        terminated |= signal == libc::SIGTERM;
                        eprintln!(
                            "  !! {}: stopping the task (it checkpoints the session first)",
                            signal_name(signal)
                        );
                        self.cancel.cancel();
                    }
                }
            }
        };
        // The task moved to the repository root.
        if let Err(err) = std::env::set_current_dir(&self.cwd) {
            eprintln!("  !! cannot go back to {}: {err}", self.cwd.display());
        }
        match result {
            Ok(TaskEnd::Done) => {}
            Ok(TaskEnd::Interrupted) => eprintln!("(task interrupted)"),
            Err(err) => report(&err),
        }
        self.cancel.reset();
        terminated
    }

    /// `/model [<name>]`: shows the model, or uses another for the rest of
    /// this process (the conversation and `/task` on the same backend).
    fn model(&mut self, model: Option<String>) {
        let agent = match &mut self.engine {
            Engine::Claude(chat) => {
                match model {
                    None => println!(
                        "model: {} (Claude Code runs the conversation)",
                        chat.claude()
                            .model
                            .as_deref()
                            .unwrap_or("of Claude Code's choice")
                    ),
                    Some(model) => {
                        chat.set_model(Some(model.clone()));
                        self.settings.set_override(MODEL_ENV, model.as_str());
                        println!(
                            "model: {model} (this session, passed to Claude Code with --model; \
                             `claude_model` in the settings file changes the default)"
                        );
                    }
                }
                return;
            }
            Engine::Api(agent) => agent,
        };
        let Some(model) = model else {
            println!("model: {}", self.config.model);
            return;
        };
        if !model.starts_with("claude-") {
            eprintln!(
                "  !! `{model}` does not look like a Claude model name; the next question may fail"
            );
        }
        let mut config = self.config.clone();
        config.model = model.clone();
        match make_provider(&config) {
            Ok(provider) => {
                agent.set_provider(provider);
                self.config = config;
                self.settings.set_override("JUNEES_MODEL", model.as_str());
                println!("model: {model} (this session; `junees setup` changes the default)");
            }
            Err(err) => report(&err),
        }
    }

    /// `/backend [api|claude-code]`: shows what runs the conversation and
    /// `/task`, or switches both for the rest of this process. Switching
    /// the conversation starts a new session: neither side can see the
    /// other's conversation.
    async fn backend(&mut self, backend: Option<Backend>) {
        let current = self.engine.backend();
        let Some(backend) = backend else {
            match Backend::from_lookup(|key| self.settings.get(key)) {
                Ok(task) if task == current => println!(
                    "the conversation and /task run on {}",
                    slash::backend_name(current)
                ),
                Ok(task) => println!(
                    "the conversation runs on {}, /task on {}",
                    slash::backend_name(current),
                    slash::backend_name(task)
                ),
                Err(err) => report(&err),
            }
            return;
        };
        let name = slash::backend_name(backend);
        let previous = self.settings.get(BACKEND_ENV);
        self.settings.set_override(BACKEND_ENV, name);
        if backend != current
            && let Err(err) = self.switch(backend).await
        {
            report(&err);
            self.settings.set_override(
                BACKEND_ENV,
                previous.unwrap_or_else(|| slash::backend_name(current).to_string()),
            );
            println!("the conversation stays on {}", slash::backend_name(current));
            return;
        }
        println!(
            "the conversation and /task now run on {name} (this session; `junees setup` \
             changes the default)"
        );
    }

    /// Moves the conversation to `backend`, in a new session.
    async fn switch(&mut self, backend: Backend) -> Result<()> {
        let config = conversation_config(&self.settings, backend)?;
        self.engine = match backend {
            Backend::Api => Engine::Api(Box::new(api_agent(
                &config,
                &self.cwd,
                &self.cancel,
                Some(&self.input),
            )?)),
            Backend::ClaudeCode => {
                let (chat, version) =
                    claude_chat(&self.settings, &config, &self.cwd, &self.cancel).await?;
                println!("Claude Code {version}");
                Engine::Claude(Box::new(chat))
            }
        };
        self.config = config;
        self.reset();
        Ok(())
    }

    fn reset(&mut self) {
        // A new session reads the skills again.
        match &mut self.engine {
            Engine::Api(agent) => agent.set_system(conversation_system(&self.config, &self.cwd)),
            Engine::Claude(chat) => chat.set_system(claude_system(&self.config)),
        }
        if !self.saving {
            self.engine.reset(None);
            println!("(new conversation, not saved)");
            return;
        }
        let opened = new_session(
            &self.config,
            &self.cwd,
            &self.engine.model(&self.config),
            self.engine.session_backend(),
        );
        self.engine.reset(opened.journal);
        self.session_id = opened.id;
        println!("(new conversation) {}", opened.status);
    }

    /// The skills folder, or why there is none.
    fn skills_store(&self) -> Option<skills::Store> {
        match self.config.require_home() {
            Ok(home) => Some(skills::Store::new(skills::dir(home))),
            Err(err) => {
                eprintln!("no skills: {err}");
                None
            }
        }
    }

    /// `/skills`: the skills in the folder now, and the folders that are
    /// not skills.
    fn skills(&self) {
        let Some(store) = self.skills_store() else {
            return;
        };
        let index = skills::Index::load(store.dir());
        if index.skills.is_empty() {
            println!("(no skills in {})", store.dir().display());
        } else {
            let count = index.skills.len();
            println!(
                "{count} {} in {} (prompt list {} / {} characters):",
                if count == 1 { "skill" } else { "skills" },
                store.dir().display(),
                index.listing_chars(),
                skills::INDEX_MAX_CHARS
            );
            for entry in &index.skills {
                println!("  {}: {}", entry.name, entry.description);
            }
        }
        for problem in &index.problems {
            eprintln!("  !! {problem}");
        }
    }

    /// `/skill <name>`: a skill's SKILL.md and the list of its files.
    fn skill(&self, name: &str) {
        let Some(store) = self.skills_store() else {
            return;
        };
        match store.view(name, None) {
            Ok(text) => println!("{}", text.trim_end()),
            Err(err) => eprintln!("{err}"),
        }
    }

    /// `/sessions`: lists recent sessions and continues the one picked by
    /// number. Returns whether SIGTERM came while it asked.
    async fn sessions(&mut self) -> bool {
        let listed = self
            .config
            .require_home()
            .and_then(|home| session::list_recent(home, SESSIONS_LISTED));
        let list = match listed {
            Ok(list) if list.is_empty() => {
                println!("(no saved sessions)");
                return false;
            }
            Ok(list) => list,
            Err(err) => {
                report(&err);
                return false;
            }
        };
        for (n, summary) in list.iter().enumerate() {
            let current = self.session_id.as_deref() == Some(summary.id.as_str());
            println!("{:>3} {}", n + 1, cli::session_line(summary, current));
        }
        let index = loop {
            print!("number to continue (Enter to stay here) > ");
            let _ = std::io::stdout().flush();
            let answer = tokio::select! {
                line = self.input.next_line() => line.ok().flatten(),
                signal = self.signals.next() => {
                    println!();
                    return signal == libc::SIGTERM;
                }
            };
            let Some(answer) = answer else {
                println!();
                return false;
            };
            match slash::pick(&answer, list.len()) {
                Ok(index) => break index,
                Err(message) => println!("{message}"),
            }
        };
        let Some(index) = index else {
            return false;
        };
        let summary = &list[index];
        if self.session_id.as_deref() == Some(summary.id.as_str()) {
            println!("(already in this session)");
            return false;
        }
        if !self.saving {
            println!("(this conversation is not saved: --no-session)");
            return false;
        }
        let backend = self.engine.session_backend();
        let opened = self
            .config
            .require_home()
            .and_then(|home| session::find_by_prefix(home, &summary.id))
            .and_then(|path| resume_session(&path, &self.cwd, backend));
        match opened {
            Ok(opened) => {
                self.engine.resume(opened.transcript, opened.journal);
                self.session_id = opened.id;
                println!("{}", opened.status);
            }
            Err(err) => report(&err),
        }
        false
    }
}

fn report(err: &anyhow::Error) {
    eprintln!("error: {err:#}");
    if let Some(provider_error) = err.downcast_ref::<ProviderError>() {
        if provider_error.is_context_overflow() {
            eprintln!(
                "hint: the conversation no longer fits the model's context window, \
                 even after clearing old tool output and compacting. /new starts a new one."
            );
        } else if provider_error.is_auth() {
            eprintln!("hint: check the API key (`junees setup`, or ANTHROPIC_API_KEY).");
        }
    }
    if err.downcast_ref::<UsageLimit>().is_some() {
        eprintln!(
            "hint: the subscription's usage limit stopped the turn; once it resets,              the next input continues the same conversation."
        );
    }
}

/// Shows the agent's progress: the model's text on stdout, everything else
/// on stderr.
///
/// On a terminal the text is shown as it arrives. Otherwise (`-p` into a
/// pipe or a file) stdout is the result, so the text of a model call is
/// held until the call is over and the part of an attempt that broke off
/// and was retried is dropped: stdout gets each reply once, whole.
struct Printer {
    /// Show text as it arrives.
    live: bool,
    /// Text was printed since the last line break (live).
    in_line: bool,
    /// The text of the model call in progress (not live).
    pending: String,
}

impl Printer {
    fn new() -> Self {
        Self {
            live: std::io::stdout().is_terminal(),
            in_line: false,
            pending: String::new(),
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::TextDelta(text) if self.live => {
                print!("{text}");
                // Nothing to do about a closed stdout but stop showing.
                let _ = std::io::stdout().flush();
                self.in_line = true;
            }
            Event::TextDelta(text) => self.pending.push_str(text),
            Event::TextEnd if self.live => {
                println!();
                self.in_line = false;
            }
            Event::TextEnd => self.pending.push('\n'),
            Event::Retrying => {
                self.pending.clear();
                self.end_line();
                eprintln!("  (retrying)");
            }
            // Reported during the call; its text is not done yet.
            Event::Thinking | Event::ServerToolUse { .. } => print_event(event),
            // Anything else comes after a model call.
            event => {
                self.end_line();
                print_event(event);
            }
        }
    }

    /// Prints what is held of the model call, or ends a line of text that
    /// was cut off (by a cancel or an error). A reply cut off that way
    /// still shows as far as it got.
    fn end_line(&mut self) {
        if std::mem::take(&mut self.in_line) {
            println!();
        }
        if !self.pending.is_empty() {
            if !self.pending.ends_with('\n') {
                self.pending.push('\n');
            }
            print!("{}", std::mem::take(&mut self.pending));
            let _ = std::io::stdout().flush();
        }
    }
}

fn print_event(event: Event<'_>) {
    match event {
        // Shown by the printer.
        Event::TextDelta(_) | Event::TextEnd | Event::Retrying => {}
        Event::Thinking => eprintln!("  (thinking...)"),
        Event::Warning(warning) => eprintln!("  !! {warning}"),
        Event::ToolCall { name, input } => {
            eprintln!(
                "  -> {name} {}",
                cli::preview(&input.to_string(), PREVIEW_CHARS)
            );
        }
        Event::ServerToolUse { name, input } => {
            eprintln!(
                "  -> {name} {} (run by the API)",
                cli::preview(&input.to_string(), PREVIEW_CHARS)
            );
        }
        // Part of the answer, after its text.
        Event::Sources(urls) => print!("{}", web_search::sources_text(urls)),
        Event::Compacted {
            before_messages,
            after_messages,
        } => eprintln!(
            "  (compacted the conversation: {before_messages} messages -> {after_messages})"
        ),
        Event::ToolResult { name, output } => {
            let status = if output.is_error { "error" } else { "ok" };
            eprintln!(
                "  <- {name} {status}: {}",
                cli::preview(&output.content, PREVIEW_CHARS)
            );
        }
    }
}
