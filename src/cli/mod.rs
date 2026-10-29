//! CLI entry point and command dispatch.

mod input;
mod plan;
mod print;
mod session;

use crate::config::{ApprovalMode, Config};
use crate::provider;
use crate::tools::{ToolContext, ToolRegistry};
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tracing_subscriber::EnvFilter;

/// Minimal, shortcut-driven coding agent.
#[derive(Debug, Parser)]
#[command(name = "palicode", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Config file path (default: ~/.config/palicode/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Working directory / sandbox root
    #[arg(long, global = true, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Provider name (opencode, ollama, ...)
    #[arg(long, global = true, value_name = "NAME")]
    provider: Option<String>,

    /// Model override
    #[arg(long, global = true, value_name = "MODEL")]
    model: Option<String>,

    /// Approval mode: suggest | auto-edit | full-auto
    #[arg(long, global = true, value_name = "MODE", value_parser = parse_approval_mode)]
    mode: Option<ApprovalMode>,

    /// Auto-approve plans and mutating tools (equivalent to --mode full-auto)
    #[arg(long, global = true)]
    yes: bool,

    /// Increase log verbosity (-v = debug, -vv = trace)
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the TUI (implemented in Phase 3)
    Tui,
    /// Interactive REPL with tools
    Repl,
    /// Run one prompt headlessly and exit
    Run {
        /// Generate a plan first (requires --yes or --mode full-auto to execute)
        #[arg(long)]
        plan: bool,
        /// Prompt text. If omitted, stdin is read (when piped).
        #[arg(
            value_name = "PROMPT",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        prompt: Vec<String>,
    },
    /// Generate and print a plan without executing it
    Plan {
        /// Print the plan as JSON
        #[arg(long)]
        json: bool,
        /// Task text. If omitted, stdin is read (when piped).
        #[arg(
            value_name = "TASK",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        task: Vec<String>,
    },
    /// List configured providers and their models
    Models,
    /// List built-in tools
    Tools,
    /// Print the effective config, or write an example config
    Config {
        /// Write the commented example config to the config path
        #[arg(long)]
        init: bool,
        /// Overwrite an existing config when used with --init
        #[arg(long)]
        force: bool,
        /// Print the effective config after initializing
        #[arg(long)]
        show: bool,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = resolve_cwd(cli.cwd.as_deref())?;
    let config_path = match &cli.config {
        Some(path) => path.clone(),
        None => Config::path()?,
    };
    let command = cli.command.unwrap_or(Command::Tui);
    // `config --init` must work before the file exists.
    let config_required = cli.config.is_some() && !matches!(&command, Command::Config { .. });
    let mut config = Config::load_from_optional(&config_path, config_required)?;
    if cli.yes {
        config.general.approval_mode = ApprovalMode::FullAuto;
    } else if let Some(mode) = cli.mode {
        config.general.approval_mode = mode;
    }
    init_logging(&config, cli.verbose);
    let ctx = ToolContext::from_config(&cwd, &config.tools)?;

    match command {
        Command::Tui => {
            crate::ui::run(config, config_path, ctx, cli.provider, cli.model).await?;
        }
        Command::Repl => {
            session::repl(config, ctx, cli.provider, cli.model).await?;
        }
        Command::Run { plan, prompt } => {
            let prompt = collect_prompt(prompt).await?;
            session::run_once(config, ctx, cli.provider, cli.model, prompt, plan).await?;
        }
        Command::Plan { json, task } => {
            let task = collect_prompt(task).await?;
            session::plan_only(config, ctx, cli.provider, cli.model, task, json).await?;
        }
        Command::Models => {
            list_models(&config).await;
        }
        Command::Tools => list_tools(),
        Command::Config { init, force, show } => {
            handle_config(&config, &config_path, init, force, show)?;
        }
    }
    Ok(())
}

fn parse_approval_mode(value: &str) -> std::result::Result<ApprovalMode, String> {
    ApprovalMode::parse(value).ok_or_else(|| {
        format!("invalid approval mode '{value}' (expected suggest, auto-edit, or full-auto)")
    })
}

fn resolve_cwd(given: Option<&Path>) -> Result<PathBuf> {
    let path = match given {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve working directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

fn init_logging(config: &Config, verbose: u8) {
    let level = match verbose {
        0 => config.general.log_level.clone(),
        1 => "debug".to_string(),
        _ => "trace".to_string(),
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(format!("palicode={level},warn"))
            .unwrap_or_else(|_| EnvFilter::new("warn"))
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .compact()
        .try_init();
}

async fn collect_prompt(parts: Vec<String>) -> Result<String> {
    let joined = parts.join(" ");
    if !joined.trim().is_empty() {
        return Ok(joined);
    }
    if std::io::stdin().is_terminal() {
        bail!("no prompt provided; pass one as an argument or pipe it via stdin");
    }

    let mut input = String::new();
    tokio::io::stdin()
        .read_to_string(&mut input)
        .await
        .context("cannot read prompt from stdin")?;
    if input.trim().is_empty() {
        bail!("no prompt provided on stdin");
    }
    Ok(input)
}

async fn list_models(config: &Config) {
    let probes = provider::probe_all(config).await;
    for probe in probes {
        let disabled = if probe.enabled { "" } else { " [disabled]" };
        match probe.model_count() {
            Some(count) => println!("{} ({count} models){disabled}", probe.name),
            None => println!("{}{disabled}", probe.name),
        }
        println!("  {}", probe.base_url);
        match probe.result {
            Ok(models) if models.is_empty() => println!("  (no models advertised)"),
            Ok(models) => {
                for model in models {
                    println!("  - {model}");
                }
            }
            Err(reason) => println!("  ! {reason}"),
        }
        println!();
    }
}

fn list_tools() {
    let registry = ToolRegistry::with_defaults();
    for name in registry.names() {
        if let Some(tool) = registry.get(name) {
            let kind = if tool.read_only() {
                "read-only"
            } else {
                "mutating"
            };
            println!("{name}  [{kind}]");
            println!("  {}", tool.description());
        }
    }
}

fn handle_config(config: &Config, path: &Path, init: bool, force: bool, show: bool) -> Result<()> {
    if init {
        let written = Config::init(path, force)?;
        println!("wrote example config to {}", written.display());
        if !show {
            return Ok(());
        }
    }

    let exists = path.is_file();
    if exists {
        println!("# config: {}", path.display());
    } else {
        println!("# config: {} (not found; using defaults)", path.display());
    }
    print!("{}", config.to_toml()?);
    Ok(())
}
