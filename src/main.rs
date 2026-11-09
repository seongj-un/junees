use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use junees::agent::{Agent, AgentOptions, Event};
use junees::cli::{self, Command};
use junees::config::Config;
use junees::prompt;
use junees::provider::ProviderError;
use junees::provider::anthropic::AnthropicProvider;
use junees::tools::ToolRegistry;
use tokio::io::{AsyncBufReadExt, BufReader};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PREVIEW_CHARS: usize = 120;

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
    let command = cli::parse_args(std::env::args().skip(1))?;
    match command {
        Command::Help => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Command::Version => {
            println!("junees {VERSION}");
            return Ok(());
        }
        Command::Repl | Command::Prompt(_) => {}
    }

    let config = Config::from_env()?;
    let provider = AnthropicProvider::new(
        config.api_key.clone(),
        config.base_url.clone(),
        config.model.clone(),
    )?;
    let cwd = std::env::current_dir()?;
    let mut agent = Agent::new(
        Box::new(provider),
        ToolRegistry::with_defaults(),
        AgentOptions {
            system: prompt::system_prompt(&cwd),
            max_tokens: config.max_tokens,
            max_turns: config.max_turns,
        },
    );

    match command {
        Command::Prompt(prompt) => {
            agent.run(&prompt, print_event).await?;
            Ok(())
        }
        _ => repl(&mut agent, &config.model).await,
    }
}

async fn repl(agent: &mut Agent, model: &str) -> Result<()> {
    println!("junees {VERSION} ({model}). /help for commands, /exit to quit.");
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
            "/reset" => {
                agent.reset();
                println!("(conversation cleared)");
            }
            "/usage" => {
                let usage = agent.usage();
                println!(
                    "input {} tokens, output {} tokens",
                    usage.input_tokens, usage.output_tokens
                );
            }
            input => {
                if let Err(err) = agent.run(input, print_event).await {
                    report(&err);
                }
            }
        }
    }
}

fn report(err: &anyhow::Error) {
    eprintln!("error: {err:#}");
    if let Some(provider_error) = err.downcast_ref::<ProviderError>() {
        if provider_error.is_context_overflow() {
            eprintln!(
                "hint: the conversation no longer fits the model's context window. \
                 /reset starts a new one."
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
        Event::ToolResult { name, output } => {
            let status = if output.is_error { "error" } else { "ok" };
            eprintln!(
                "  <- {name} {status}: {}",
                cli::preview(&output.content, PREVIEW_CHARS)
            );
        }
    }
}
