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
use crate::state::{SESSION_ACTIVE, SessionRecord, StateStore, now_epoch};
use crate::tools::{ToolContext, ToolRegistry};
use crate::ui::app::Bootstrap;
use crate::ui::chat::ChatState;
use anyhow::{Context, Result, bail};
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
    resume_id: Option<String>,
) -> Result<()> {
    let resolved = provider::resolve(&config, provider_name.as_deref(), model.as_deref()).await?;
    let provider = resolved.provider.name().to_string();
    let model = resolved.model;

    let store = match StateStore::open() {
        Ok(store) => Some(store),
        Err(err) => {
            eprintln!("warning: durable state disabled: {err:#}");
            None
        }
    };
    let bootstrap = build_bootstrap(
        store.as_ref(),
        resume_id.as_deref(),
        &ctx,
        &provider,
        &model,
    )?;

    let registry = ToolRegistry::with_defaults();
    let mut session = AgentSession::new(
        resolved.provider,
        model.clone(),
        registry,
        ctx.clone(),
        &config,
    );
    if !bootstrap.messages.is_empty() {
        session.import_messages(bootstrap.messages.clone());
    }

    let mouse = config.tui.mouse;
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    let initial_session = bootstrap.session_id.clone();
    let worker = worker::spawn(
        session,
        config.clone(),
        store.clone(),
        initial_session,
        command_rx,
        ui_tx,
    );
    let mut app = app::App::new(
        config,
        config_path,
        ctx,
        provider,
        model,
        store,
        bootstrap,
        command_tx,
        ui_rx,
    );

    let mut guard = TerminalGuard::enter(mouse)?;
    let result = app.run(&mut guard.terminal).await;
    app.shutdown();
    drop(guard);
    worker.abort();
    result
}

fn build_bootstrap(
    store: Option<&StateStore>,
    resume_id: Option<&str>,
    ctx: &ToolContext,
    provider: &str,
    model: &str,
) -> Result<Bootstrap> {
    let Some(store) = store else {
        if let Some(id) = resume_id {
            bail!("durable state is unavailable; cannot resume session '{id}'");
        }
        return Ok(Bootstrap::default());
    };

    let cwd = ctx.cwd.display().to_string();
    let interrupted = store
        .mark_active_interrupted_for_cwd(&cwd)
        .unwrap_or_default();
    let mut sessions = store.list_sessions_for_cwd(&cwd)?;
    let chosen = if let Some(id) = resume_id {
        let session = store
            .get_session(id)?
            .ok_or_else(|| anyhow::anyhow!("unknown session id '{id}'"))?;
        if session.cwd != cwd {
            bail!(
                "session '{id}' belongs to project '{}', not the current project '{}'",
                session.cwd,
                cwd
            );
        }
        session
    } else if let Some(session) = interrupted.first().cloned() {
        session
    } else if let Some(session) = sessions.first().cloned() {
        session
    } else {
        let now = now_epoch();
        let session = SessionRecord {
            id: uuid::Uuid::new_v4().to_string(),
            title: "New session".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            cwd: ctx.cwd.display().to_string(),
            status: SESSION_ACTIVE.to_string(),
            turns: 0,
            created_at: now,
            updated_at: now,
        };
        store.insert_session(&session)?;
        sessions.push(session.clone());
        session
    };
    store.set_active_session(&chosen.id)?;

    let messages = store.load_messages(&chosen.id)?;
    let plan = store.load_plan(&chosen.id)?;
    let (chat, tools) = ChatState::restore(&messages);
    let recovered = interrupted
        .iter()
        .any(|session| session.id == chosen.id)
        .then(|| chosen.title.clone());

    Ok(Bootstrap {
        session_id: chosen.id,
        sessions,
        messages,
        chat,
        tools,
        plan,
        recovered,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::{SessionRecord, now_epoch};
    use crate::tools::ToolContext;
    use tempfile::tempdir;

    #[test]
    fn bootstrap_recovers_an_interrupted_session() {
        let dir = tempdir().unwrap();
        let store = StateStore::open_at(&dir.path().join("state.db")).unwrap();
        let now = now_epoch();
        let id = "interrupted-1".to_string();
        store
            .insert_session(&SessionRecord {
                id: id.clone(),
                title: "Recover me".to_string(),
                provider: "ollama".to_string(),
                model: "qwen3.5:9b".to_string(),
                cwd: dir.path().display().to_string(),
                status: "active".to_string(),
                turns: 1,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        store
            .save_checkpoint(
                &id,
                &[
                    crate::provider::Message::system("sys"),
                    crate::provider::Message::user("remember BANANA"),
                    crate::provider::Message::assistant(Some("OK".to_string()), Vec::new()),
                ],
                "turn_end",
            )
            .unwrap();

        let config = Config::default();
        let ctx = ToolContext::from_config(dir.path(), &config.tools).unwrap();
        let bootstrap = build_bootstrap(Some(&store), None, &ctx, "ollama", "qwen3.5:9b").unwrap();
        assert_eq!(bootstrap.session_id, id);
        assert_eq!(bootstrap.recovered.as_deref(), Some("Recover me"));
        assert_eq!(bootstrap.messages.len(), 3);
        assert!(bootstrap.chat.items.iter().any(|item| matches!(
            item,
            crate::ui::chat::ChatItem::User(text) if text.contains("BANANA")
        )));
        assert_eq!(
            store.get_session(&id).unwrap().unwrap().status,
            crate::state::SESSION_ACTIVE
        );
    }

    #[test]
    fn sessions_do_not_leak_between_projects() {
        let one = tempdir().unwrap();
        let two = tempdir().unwrap();
        let store = StateStore::open_at(&one.path().join("state.db")).unwrap();
        let now = now_epoch();
        let mut first = SessionRecord {
            id: "one-session".to_string(),
            title: "One".to_string(),
            provider: "ollama".to_string(),
            model: "qwen3.5:9b".to_string(),
            cwd: one.path().display().to_string(),
            status: "active".to_string(),
            turns: 0,
            created_at: now,
            updated_at: now,
        };
        store.insert_session(&first).unwrap();
        let mut second = first.clone();
        second.id = "two-session".to_string();
        second.cwd = two.path().display().to_string();
        store.insert_session(&second).unwrap();
        first.status = "interrupted".to_string();
        store.insert_session(&first).unwrap();

        let config = Config::default();
        let ctx = ToolContext::from_config(one.path(), &config.tools).unwrap();
        let bootstrap = build_bootstrap(Some(&store), None, &ctx, "ollama", "qwen3.5:9b").unwrap();
        assert_eq!(bootstrap.session_id, "one-session");
        assert!(
            bootstrap
                .sessions
                .iter()
                .all(|session| session.id != "two-session")
        );

        let err = build_bootstrap(
            Some(&store),
            Some("two-session"),
            &ctx,
            "ollama",
            "qwen3.5:9b",
        )
        .unwrap_err();
        assert!(err.to_string().contains("belongs to project"));
    }

    #[test]
    fn unknown_resume_id_is_rejected() {
        let dir = tempdir().unwrap();
        let store = StateStore::open_at(&dir.path().join("state.db")).unwrap();
        let config = Config::default();
        let ctx = ToolContext::from_config(dir.path(), &config.tools).unwrap();
        let err = build_bootstrap(
            Some(&store),
            Some("does-not-exist"),
            &ctx,
            "ollama",
            "qwen3.5:9b",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown session"));
    }
}
