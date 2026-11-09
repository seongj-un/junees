use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use junees::agent::{Agent, AgentOptions, Event};
use junees::cli::{self, Command, SessionMode, TaskCommand};
use junees::config::Config;
use junees::notify::Notifier;
use junees::prompt;
use junees::provider::anthropic::AnthropicProvider;
use junees::provider::{Provider, ProviderError};
use junees::session::{self, Journal, Session, SessionWriter, Transcript};
use junees::task::{self, Report, TaskEnv};
use junees::tools::ToolRegistry;
use tokio::io::{AsyncBufReadExt, BufReader};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PREVIEW_CHARS: usize = 120;
const SESSIONS_LISTED: usize = 10;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(&err);
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = match cli::parse_args(std::env::args().skip(1))? {
        Command::Help => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Command::Version => {
            println!("junees {VERSION}");
            return Ok(());
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
    )?;
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

    match args.prompt {
        Some(prompt) => {
            // stdout carries only the answer.
            eprintln!("{}", opened.status);
            agent.run(&prompt, print_event).await?;
            Ok(())
        }
        None => {
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
            };
            repl.run().await
        }
    }
}

/// `junees task status`, for the git repository the current folder is in.
fn task_status(json: bool) -> Result<()> {
    let root = task::git::toplevel(&std::env::current_dir()?)?;
    let status = task::status::load(&root)?;
    if json {
        println!("{}", status.to_json());
    } else {
        print!("{}", status.to_text());
    }
    Ok(())
}

/// `junees task init|run`, in the root of the git repository the current
/// folder is in.
async fn run_task(command: TaskCommand) -> Result<()> {
    let config = Config::from_env()?;
    let root = task::git::toplevel(&std::env::current_dir()?)?;
    // The tools resolve relative paths from the process's folder.
    std::env::set_current_dir(&root)
        .with_context(|| format!("cannot change to {}", root.display()))?;
    let provider = || -> Result<Box<dyn Provider>> {
        Ok(Box::new(AnthropicProvider::new(
            config.api_key.clone(),
            config.base_url.clone(),
            config.model.clone(),
        )?))
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
        notify: Notifier::from_env()?,
    };
    let mut report = |report: Report<'_>| match report {
        Report::Line(line) => println!("{line}"),
        Report::Event(event) => print_event(event),
    };
    match command {
        TaskCommand::Init { goal } => task::init(&env, &goal, &mut report).await,
        TaskCommand::Run { max_sessions } => {
            let stop = task::run(&env, max_sessions, &mut report).await?;
            if stop.reason.is_failure() {
                bail!("task stopped: {stop}");
            }
            println!("task stopped: {stop}");
            Ok(())
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
}

impl Repl<'_> {
    async fn run(&mut self) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        loop {
            print!("> ");
            std::io::stdout().flush()?;
            let Some(line) = lines.next_line().await? else {
                println!();
                return Ok(());
            };
            match line.trim() {
                "" => {}
                "/exit" | "/quit" => return Ok(()),
                "/help" => println!("{}", cli::REPL_HELP),
                "/reset" => self.reset(),
                "/sessions" => self.list_sessions(),
                "/usage" => {
                    let usage = self.agent.usage();
                    println!(
                        "input {} tokens (cache write {}, cache read {}), output {} tokens",
                        usage.input_tokens,
                        usage.cache_creation_input_tokens,
                        usage.cache_read_input_tokens,
                        usage.output_tokens
                    );
                }
                "/compact" => {
                    self.agent.compact(print_event).await;
                }
                "/context" => {
                    let options = self.agent.options();
                    println!(
                        "context {} / {} tokens; old tool output is cleared from {} tokens",
                        self.agent.context_tokens(),
                        options.context_window,
                        options.compact_limit()
                    );
                }
                input => {
                    if let Err(err) = self.agent.run(input, print_event).await {
                        report(&err);
                    }
                }
            }
        }
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

fn print_event(event: Event<'_>) {
    match event {
        Event::Text(text) => println!("{text}"),
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
