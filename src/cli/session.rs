//! REPL and one-shot (`run`) front-ends.

use crate::agent::AgentSession;
use crate::cli::print::{Mode, Printer};
use crate::config::Config;
use crate::provider;
use crate::tools::{ToolContext, ToolRegistry};
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;

/// Build a session by resolving a provider and model.
pub async fn build_session(
    config: &Config,
    ctx: &ToolContext,
    provider_name: Option<&str>,
    model: Option<&str>,
) -> Result<AgentSession> {
    let resolved = provider::resolve(config, provider_name, model).await?;
    let registry = ToolRegistry::with_defaults();
    Ok(AgentSession::new(
        resolved.provider,
        resolved.model,
        registry,
        ctx.clone(),
        config,
    ))
}

pub async fn repl(
    config: Config,
    ctx: ToolContext,
    provider_name: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let mut session =
        build_session(&config, &ctx, provider_name.as_deref(), model.as_deref()).await?;
    let mut printer = Printer::new(Mode::Repl);
    print_banner(&session, &ctx);
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        if std::io::stdin().is_terminal() {
            if std::io::stdout().is_terminal() {
                print!("\x1b[36m❯\x1b[0m ");
            } else {
                print!("> ");
            }
            let _ = std::io::stdout().flush();
        }

        line.clear();
        let read = reader
            .read_line(&mut line)
            .await
            .context("cannot read stdin")?;
        if read == 0 {
            if std::io::stdin().is_terminal() {
                println!();
            }
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if input.starts_with('/') {
            let keep_going = handle_command(&mut session, &config, input, &mut printer).await;
            if !keep_going {
                break;
            }
            continue;
        }

        if let Err(err) = session
            .run_turn(input, &mut |event| printer.event(event))
            .await
        {
            printer.error(&format!("{err:#}"));
        }
        printer.finish();
    }

    Ok(())
}

pub async fn run_once(
    config: Config,
    ctx: ToolContext,
    provider_name: Option<String>,
    model: Option<String>,
    prompt: String,
) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("prompt must not be empty");
    }
    let mut session =
        build_session(&config, &ctx, provider_name.as_deref(), model.as_deref()).await?;
    let mut printer = Printer::new(Mode::Run);
    let result = session
        .run_turn(&prompt, &mut |event| printer.event(event))
        .await;
    printer.finish();
    result?;
    Ok(())
}

fn print_banner(session: &AgentSession, ctx: &ToolContext) {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "palicode {version}  provider={}  model={}",
        session.provider().name(),
        session.model()
    );
    println!("endpoint: {}", session.provider().base_url());
    let sandbox = ctx
        .sandbox_root
        .as_ref()
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| "(disabled)".to_string());
    println!(
        "cwd: {}  sandbox: {}  tools: {}",
        ctx.cwd.display(),
        sandbox,
        session.registry().names().len()
    );
    println!("/help for commands  ·  /quit to exit  ·  Ctrl-D to exit");
    println!();
}

/// Returns `false` when the REPL should exit.
async fn handle_command(
    session: &mut AgentSession,
    config: &Config,
    input: &str,
    printer: &mut Printer,
) -> bool {
    let mut parts = input.splitn(2, char::is_whitespace);
    let command = parts
        .next()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_ascii_lowercase();
    let argument = parts.next().unwrap_or("").trim();

    match command.as_str() {
        "q" | "quit" | "exit" => return false,
        "help" | "h" | "?" => print_help(),
        "clear" => {
            session.reset();
            printer.info("history cleared");
        }
        "models" => match session.provider().list_models().await {
            Ok(models) if models.is_empty() => printer.info("provider advertises no models"),
            Ok(models) => {
                for model in models {
                    let marker = if model == session.model() { "*" } else { " " };
                    println!(" {marker} {model}");
                }
            }
            Err(err) => printer.error(&format!("{err:#}")),
        },
        "model" => {
            if argument.is_empty() {
                printer.info(&format!("current model: {}", session.model()));
                printer.info("use /model <id> or /models");
            } else {
                session.set_model(argument);
                printer.info(&format!("model set to {argument}"));
            }
        }
        "provider" => {
            if argument.is_empty() {
                for provider in &config.providers.list {
                    let marker = if provider.name == session.provider().name() {
                        "*"
                    } else {
                        " "
                    };
                    println!(" {marker} {} ({})", provider.name, provider.base_url);
                }
                printer.info("use /provider <name> to switch");
            } else {
                match provider::resolve(config, Some(argument), None).await {
                    Ok(resolved) => {
                        let model = resolved.model.clone();
                        let name = resolved.provider.name().to_string();
                        session.set_provider(resolved.provider, model.clone());
                        printer.info(&format!("provider set to {name}, model {model}"));
                    }
                    Err(err) => printer.error(&format!("{err:#}")),
                }
            }
        }
        "tools" => {
            for name in session.registry().names() {
                if let Some(tool) = session.registry().get(name) {
                    let description = tool.description().lines().next().unwrap_or("");
                    println!("  {name:<14} {description}");
                }
            }
        }
        "system" => {
            if argument.is_empty() {
                println!("{}", session.system_prompt());
            } else {
                session.set_system_prompt(argument);
                printer.info("system prompt updated; history cleared");
            }
        }
        "cwd" => println!("{}", session.context().cwd.display()),
        "history" => {
            let count = session.messages().len().saturating_sub(1);
            printer.info(&format!("{count} messages in history"));
        }
        other => printer.error(&format!("unknown command /{other} (try /help)")),
    }
    true
}

fn print_help() {
    println!(
        "\
Commands:
  /help              Show this help
  /models            List models advertised by the current provider
  /model <id>        Switch model
  /provider [name]   List or switch provider
  /tools             List available tools
  /system [prompt]   Show or replace the system prompt (clears history)
  /clear             Clear conversation history
  /cwd               Show the workspace directory
  /history           Show message count
  /quit              Exit (also Ctrl-D)

Everything else is sent to the model. Tool calls are executed automatically."
    );
}
