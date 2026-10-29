//! REPL and one-shot (`run`) front-ends.

use crate::agent::AgentSession;
use crate::approval::NonInteractiveApprover;
use crate::cli::input::{PromptApprover, SharedInput};
use crate::cli::plan;
use crate::cli::print::{Mode, Printer};
use crate::config::{ApprovalMode, Config};
use crate::provider;
use crate::tools::{ToolContext, ToolRegistry};
use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};
use std::sync::Arc;

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
    let input = SharedInput::new();
    session.set_approver(Arc::new(PromptApprover::new(input.clone())));
    let mut printer = Printer::new(Mode::Repl);
    print_banner(&session, &ctx);

    let mut line = String::new();
    loop {
        if std::io::stdin().is_terminal() {
            if std::io::stdout().is_terminal() {
                print!("\x1b[1m❯\x1b[0m ");
            } else {
                print!("> ");
            }
            let _ = std::io::stdout().flush();
        }

        let read = input
            .read_line(&mut line)
            .await
            .context("cannot read stdin")?;
        if read == 0 {
            if std::io::stdin().is_terminal() {
                println!();
            }
            break;
        }

        let command_line = line.trim();
        if command_line.is_empty() {
            continue;
        }

        if command_line.starts_with('/') {
            let keep_going =
                handle_command(&mut session, &config, &input, command_line, &mut printer).await;
            if !keep_going {
                break;
            }
            continue;
        }

        if let Err(err) = session
            .run_turn(command_line, &mut |event| printer.event(event))
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
    plan_requested: bool,
) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("prompt must not be empty");
    }
    let mut session =
        build_session(&config, &ctx, provider_name.as_deref(), model.as_deref()).await?;
    session.set_approver(Arc::new(NonInteractiveApprover::default()));
    let mut printer = Printer::new(Mode::Run);

    if plan_requested {
        let mut generated = plan::generate_plan(&session, &prompt, &mut printer).await?;
        printer.info(&generated.render(None));

        if session.approval_mode() != ApprovalMode::FullAuto {
            bail!("plan approval is required to execute; re-run with --yes or --mode full-auto");
        }

        let selected: Vec<usize> = (0..generated.steps.len()).collect();
        let outcome = crate::agent::executor::execute_plan(
            &mut session,
            &mut generated,
            &selected,
            &mut |event| printer.event(event),
        )
        .await?;
        printer.finish();
        if outcome.failed > 0 {
            bail!("plan finished with {} failed step(s)", outcome.failed);
        }
        return Ok(());
    }

    let result = session
        .run_turn(&prompt, &mut |event| printer.event(event))
        .await;
    printer.finish();
    result?;
    Ok(())
}

/// Generate and print a plan without executing it.
pub async fn plan_only(
    config: Config,
    ctx: ToolContext,
    provider_name: Option<String>,
    model: Option<String>,
    task: String,
    json: bool,
) -> Result<()> {
    if task.trim().is_empty() {
        bail!("task must not be empty");
    }
    let session = build_session(&config, &ctx, provider_name.as_deref(), model.as_deref()).await?;
    let plan = session
        .planner()
        .create(&task, &session.context().cwd, &session.registry().names())
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print!("{}", plan.render(None));
    }
    Ok(())
}

fn print_banner(session: &AgentSession, ctx: &ToolContext) {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        "palicode {version}  provider={}  model={}  mode={}",
        session.provider().name(),
        session.model(),
        session.approval_mode()
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
    println!("/help for commands  ·  /plan <task> to plan  ·  /quit to exit");
    println!();
}

/// Returns `false` when the REPL should exit.
async fn handle_command(
    session: &mut AgentSession,
    config: &Config,
    input: &SharedInput,
    command_line: &str,
    printer: &mut Printer,
) -> bool {
    let mut parts = command_line.splitn(2, char::is_whitespace);
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
        "plan" => {
            if argument.is_empty() {
                printer.info("usage: /plan <task>");
            } else if let Err(err) = plan::interactive(session, input, argument, printer).await {
                printer.error(&format!("{err:#}"));
            }
        }
        "mode" => {
            if argument.is_empty() {
                printer.info(&format!(
                    "approval mode: {} (suggest | auto-edit | full-auto)",
                    session.approval_mode()
                ));
            } else if let Some(mode) = ApprovalMode::parse(argument) {
                session.set_approval_mode(mode);
                printer.info(&format!("approval mode set to {mode}"));
            } else {
                printer.error(&format!(
                    "invalid approval mode '{argument}' (suggest | auto-edit | full-auto)"
                ));
            }
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
                    let kind = if tool.read_only() { "ro" } else { "rw" };
                    println!("  {name:<14} [{kind}] {description}");
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
  /plan <task>       Generate a plan, review/edit/approve it, then execute
  /mode [mode]       Show or set approval mode (suggest | auto-edit | full-auto)
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

Plan review: a=all, n=none, 1 2 3=toggle, e N text, r N M, d N, y=execute, q=cancel.
Everything else is sent to the model. Mutating tools follow the approval mode."
    );
}
