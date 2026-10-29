//! ratatui application: panes, overlays, and the agent worker.

pub mod agents;
pub mod app;
pub mod chat;
pub mod diff;
pub mod plan;
pub mod theme;
pub mod util;
pub mod worker;

use crate::agent::AgentSession;
use crate::config::Config;
use crate::provider;
use crate::tools::{ToolContext, ToolRegistry};
use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{Stdout, stdout};
use tokio::sync::mpsc;

pub async fn run(
    config: Config,
    config_path: std::path::PathBuf,
    ctx: ToolContext,
    provider_name: Option<String>,
    model: Option<String>,
) -> Result<()> {
    let resolved = provider::resolve(&config, provider_name.as_deref(), model.as_deref()).await?;
    let provider = resolved.provider.name().to_string();
    let model = resolved.model;
    let registry = ToolRegistry::with_defaults();
    let session = AgentSession::new(
        resolved.provider,
        model.clone(),
        registry,
        ctx.clone(),
        &config,
    );

    let mouse = config.tui.mouse;
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let worker = worker::spawn(session, config.clone(), command_rx, ui_tx);
    let mut app = app::App::new(config, config_path, ctx, provider, model, command_tx, ui_rx);

    let mut guard = TerminalGuard::enter(mouse)?;
    let result = app.run(&mut guard.terminal).await;
    drop(guard);
    worker.abort();
    result
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode().context("cannot enable raw mode")?;
        let mut out = stdout();
        let entered = if mouse {
            execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture
            )
        } else {
            execute!(out, EnterAlternateScreen, EnableBracketedPaste)
        };
        if let Err(err) = entered {
            let _ = disable_raw_mode();
            return Err(err).context("cannot enter alternate screen");
        }

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(
                std::io::stdout(),
                LeaveAlternateScreen,
                crossterm::cursor::Show
            );
            original_hook(info);
        }));

        let backend = CrosstermBackend::new(out);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = disable_raw_mode();
                let _ = execute!(
                    std::io::stdout(),
                    DisableMouseCapture,
                    DisableBracketedPaste,
                    LeaveAlternateScreen,
                    crossterm::cursor::Show
                );
                return Err(err).context("cannot create terminal");
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = self.terminal.show_cursor();
    }
}
