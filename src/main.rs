use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};
use junees::agent::{Agent, AgentOptions, Cancelled, Event};
use junees::approval::{self, SharedLines};
use junees::cancel::Cancel;
use junees::cli::{self, Command, SessionMode, TaskCommand};
use junees::config::Config;
use junees::notify::Notifier;
use junees::prompt;
use junees::provider::anthropic::AnthropicProvider;
use junees::provider::{Provider, ProviderError};
use junees::session::{self, Journal, Session, SessionWriter, Transcript};
use junees::task::budget::Budget;
use junees::task::claude_code::{Backend, ClaudeCode};
use junees::task::state::StopReason;
use junees::task::{self, Report, TaskEnv};
use junees::tools::ToolRegistry;
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
    let args = match cli::parse_args(std::env::args().skip(1))? {
        Command::Help => {
            print!("{}", cli::HELP);
            return Ok(ExitCode::SUCCESS);
        }
        Command::Version => {
            println!("junees {VERSION}");
            return Ok(ExitCode::SUCCESS);
        }
        Command::Run(args) => args,
        // Needs no API key, and writes nothing.
        Command::Task(TaskCommand::Status { json }) => return task_status(json),
        Command::Task(command) => return run_task(command).await,
    };

    let config = Config::from_env()?;
    let provider = AnthropicProvider::new(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
    )?
    .with_thinking(config.thinking_budget)
    .with_streaming(config.stream);
    let cwd = std::env::current_dir()?;
    let opened = open_session(&args.session, &config, &cwd)?;
    let mut agent = Agent::restore(
        Box::new(provider),
        ToolRegistry::with_defaults(&config.tools),
        AgentOptions {
            system: prompt::system_prompt(&cwd),
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
            context_window: config.context_window,
            compact_threshold: config.compact_threshold,
        },
        opened.transcript,
    );
    if let Some(journal) = opened.journal {
        agent = agent.with_journal(journal);
    }
    let cancel = Cancel::new();
    agent = agent.with_cancel(cancel.clone());
    let signals = Signals::install()?;

    match args.prompt {
        Some(prompt) => {
            let received = cancel_on_signals(signals, cancel);
            // stdout carries only the answer.
            eprintln!("{}", opened.status);
            let mut printer = Printer::new();
            let result = agent.run(&prompt, |event| printer.event(event)).await;
            printer.end_line();
            match result {
                Ok(_) => Ok(ExitCode::SUCCESS),
                Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                    let signal = received.load(Ordering::SeqCst);
                    eprintln!("interrupted by {}", signal_name(signal));
                    Ok(signal_exit(signal))
                }
                Err(err) => Err(err),
            }
        }
        None => {
            // Shared by the prompt and the approval questions.
            let input = SharedLines::stdin();
            if let Some(gate) = approval::for_conversation(&config, &cwd, &input) {
                agent = agent.with_gate(Box::new(gate));
            }
            println!(
                "junees {VERSION} ({}). /help for commands, /exit to quit.",
                config.model
            );
            println!("{}", opened.status);
            let mut repl = Repl {
                agent,
                config: &config,
                cwd: &cwd,
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

/// `junees task status`, for the git repository the current folder is in.
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

/// `junees task init|run`, in the root of the git repository the current
/// folder is in. SIGINT/SIGTERM stop the session that runs; `task run`
/// then checkpoints it, records the stop and exits with 130/143.
async fn run_task(command: TaskCommand) -> Result<ExitCode> {
    let backend = match command.backend() {
        Some(backend) => backend,
        None => Backend::from_lookup(|key| std::env::var(key).ok())?,
    };
    let (config, claude) = match backend {
        Backend::Api => (Config::from_env()?, None),
        Backend::ClaudeCode => (
            Config::from_env_without_key()?,
            Some(ClaudeCode::from_lookup(|key| std::env::var(key).ok())),
        ),
    };
    let root = task::git::toplevel(&std::env::current_dir()?)?;
    // The tools resolve relative paths from the process's folder.
    std::env::set_current_dir(&root)
        .with_context(|| format!("cannot change to {}", root.display()))?;
    let provider = || -> Result<Box<dyn Provider>> {
        Ok(Box::new(
            AnthropicProvider::new(
                config.api_key.clone(),
                config.base_url.clone(),
                config.model.clone(),
            )?
            .with_thinking(config.thinking_budget)
            .with_streaming(config.stream),
        ))
    };
    let budget = match &command {
        TaskCommand::Run {
            max_tokens,
            max_time,
            ..
        } => Budget {
            max_tokens: max_tokens.or(config.task_max_tokens),
            max_time: max_time.or(config.task_max_time),
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
        cancel: Cancel::new(),
        notify: Notifier::from_env()?,
        claude,
    };
    let received = cancel_on_signals(Signals::install()?, env.cancel.clone());
    let interrupted = || signal_exit(received.load(Ordering::SeqCst));
    let mut printer = Printer::new();
    let mut reporter = |report: Report<'_>| match report {
        Report::Line(line) => {
            printer.end_line();
            println!("{line}");
        }
        Report::Event(event) => printer.event(event),
    };
    match command {
        TaskCommand::Init { goal, .. } => match task::init(&env, &goal, &mut reporter).await {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                report(&err);
                Ok(interrupted())
            }
            Err(err) => Err(err),
        },
        TaskCommand::Run { max_sessions, .. } => {
            let stop = task::run(&env, max_sessions, &mut reporter).await?;
            if stop.reason.is_failure() {
                bail!("task stopped: {stop}");
            }
            println!("task stopped: {stop}");
            if stop.reason == StopReason::Interrupted {
                return Ok(interrupted());
            }
            Ok(ExitCode::SUCCESS)
        }
        TaskCommand::Status { json } => task_status(json),
    }
}

/// The conversation to start from and where to record it.
struct Opened {
    transcript: Transcript,
    journal: Option<Box<dyn Journal>>,
    id: Option<String>,
    /// One line for the user about which session this is.
    status: String,
}

fn open_session(mode: &SessionMode, config: &Config, cwd: &Path) -> Result<Opened> {
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
            let latest = session::find_latest(home, cwd)?;
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
        return Ok(new_session(config, cwd));
    };
    let (session, writer) = Session::resume(&path)?;
    if let Some(warning) = &session.warning {
        eprintln!("  !! {warning}");
    }
    // Relative paths in the history mean the folder it started in.
    let elsewhere = if session.meta.cwd == cwd.to_string_lossy() {
        String::new()
    } else {
        format!(", started in {}", session.meta.cwd)
    };
    Ok(Opened {
        status: format!(
            "resumed {} ({} messages{elsewhere})",
            session.meta.id,
            session.transcript.messages.len()
        ),
        id: Some(session.meta.id),
        transcript: session.transcript,
        journal: Some(Box::new(writer)),
    })
}

/// A fresh saved session. Without a state folder, or if its file cannot be
/// created, Junees still runs, just without saving.
fn new_session(config: &Config, cwd: &Path) -> Opened {
    let created = config
        .require_home()
        .and_then(|home| SessionWriter::create(home, cwd, &config.model));
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

struct Repl<'a> {
    agent: Agent,
    config: &'a Config,
    cwd: &'a Path,
    /// False with --no-session, so /reset starts another unsaved session.
    saving: bool,
    session_id: Option<String>,
    /// Set to cancel the turn that is running.
    cancel: Cancel,
    signals: Signals,
    input: SharedLines,
}

impl Repl<'_> {
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
            let terminated = match line.trim() {
                "" => false,
                "/exit" | "/quit" => return Ok(ExitCode::SUCCESS),
                "/help" => {
                    println!("{}", cli::REPL_HELP);
                    false
                }
                "/reset" => {
                    self.reset();
                    false
                }
                "/sessions" => {
                    self.list_sessions();
                    false
                }
                "/usage" => {
                    let usage = self.agent.usage();
                    println!(
                        "input {} tokens (cache write {}, cache read {}), output {} tokens",
                        usage.input_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_read_input_tokens,
                        usage.output_tokens
                    );
                    false
                }
                // Nothing is recorded until the summary is back, so the
                // compaction can simply be dropped.
                "/compact" => tokio::select! {
                    _ = self.agent.compact(print_event) => false,
                    signal = self.signals.next() => {
                        eprintln!("(compaction cancelled)");
                        signal == libc::SIGTERM
                    }
                },
                "/context" => {
                    let options = self.agent.options();
                    println!(
                        "context {} / {} tokens; old tool output is cleared from {} tokens",
                        self.agent.context_tokens(),
                        options.context_window,
                        options.compact_limit()
                    );
                    false
                }
                input => self.turn(input).await,
            };
            if terminated {
                return Ok(signal_exit(libc::SIGTERM));
            }
        }
    }

    /// Runs one input. A signal cancels the turn; returns whether it was
    /// SIGTERM, which also quits.
    async fn turn(&mut self, input: &str) -> bool {
        self.cancel.reset();
        let mut printer = Printer::new();
        let mut terminated = false;
        let result = {
            let run = self.agent.run(input, |event| printer.event(event));
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
            Ok(_) => {}
            Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                eprintln!("(turn cancelled)");
            }
            Err(err) => report(&err),
        }
        terminated
    }

    fn reset(&mut self) {
        if !self.saving {
            self.agent.reset(None);
            println!("(new conversation, not saved)");
            return;
        }
        let opened = new_session(self.config, self.cwd);
        self.agent.reset(opened.journal);
        self.session_id = opened.id;
        println!("(new conversation) {}", opened.status);
    }

    fn list_sessions(&self) {
        let listed = self
            .config
            .require_home()
            .and_then(|home| session::list_recent(home, SESSIONS_LISTED));
        match listed {
            Ok(list) if list.is_empty() => println!("(no saved sessions)"),
            Ok(list) => {
                for summary in &list {
                    let current = self.session_id.as_deref() == Some(summary.id.as_str());
                    println!("{}", cli::session_line(summary, current));
                }
                println!("continue one with: junees --resume <id>");
            }
            Err(err) => report(&err),
        }
    }
}

fn report(err: &anyhow::Error) {
    eprintln!("error: {err:#}");
    if let Some(provider_error) = err.downcast_ref::<ProviderError>() {
        if provider_error.is_context_overflow() {
            eprintln!(
                "hint: the conversation no longer fits the model's context window, \
                 even after clearing old tool output and compacting. /reset starts a new one."
            );
        } else if provider_error.is_auth() {
            eprintln!("hint: check ANTHROPIC_API_KEY.");
        }
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
            Event::Thinking => print_event(event),
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
