//! Agent worker: owns the session and communicates with the TUI via channels.

use crate::agent::executor::{self, PlanOutcome};
use crate::agent::plan::Plan;
use crate::agent::{AgentEvent, AgentSession};
use crate::approval::{ApprovalRequest, ApprovalResponse, Approver};
use crate::config::{ApprovalMode, Config};
use crate::event::Event;
use crate::provider::{self, Message};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot};

#[derive(Debug)]
pub enum AgentCommand {
    Submit(String),
    GeneratePlan(String),
    ExecutePlan { plan: Plan, selected: Vec<usize> },
    SetModel(String),
    SetProvider(String),
    ListModels,
    SetMode(ApprovalMode),
    ClearHistory,
    NewSession(String),
    SwitchSession(String),
    DeleteSession(String),
}

pub enum UiMessage {
    Agent(AgentEvent),
    Bus(Event),
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalResponse>,
    },
    Models {
        provider: String,
        current: String,
        models: Vec<String>,
    },
    ModelChanged {
        provider: String,
        model: String,
    },
    ProviderChanged {
        provider: String,
        model: String,
    },
    ModeChanged(ApprovalMode),
    Status(String),
    Error(String),
    TurnFinished,
    PlanFinished(PlanOutcome),
}

struct TuiApprover {
    tx: mpsc::UnboundedSender<UiMessage>,
}

#[async_trait]
impl Approver for TuiApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalResponse> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(UiMessage::Approval {
                request: request.clone(),
                reply,
            })
            .map_err(|_| anyhow!("UI closed"))?;
        response
            .await
            .map_err(|_| anyhow!("UI closed while waiting for approval"))
    }
}

pub fn spawn(
    mut session: AgentSession,
    config: Config,
    commands: mpsc::UnboundedReceiver<AgentCommand>,
    ui: mpsc::UnboundedSender<UiMessage>,
) -> tokio::task::JoinHandle<()> {
    session.set_approver(Arc::new(TuiApprover { tx: ui.clone() }));
    tokio::spawn(async move {
        run(session, config, commands, ui).await;
    })
}

async fn run(
    mut session: AgentSession,
    config: Config,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
    ui: mpsc::UnboundedSender<UiMessage>,
) {
    let mut sessions: HashMap<String, Vec<Message>> = HashMap::new();
    let mut current_session = "default".to_string();
    let mut bus_rx = session.bus().subscribe();
    let bus_tx = ui.clone();
    let bus_task = tokio::spawn(async move {
        loop {
            match bus_rx.recv().await {
                Ok(event) => {
                    if matches!(event, Event::Agent(_)) {
                        continue;
                    }
                    if bus_tx.send(UiMessage::Bus(event)).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(command) = commands.recv().await {
        match command {
            AgentCommand::Submit(prompt) => {
                if let Err(err) = run_turn(&mut session, &prompt, &ui).await {
                    let _ = ui.send(UiMessage::Error(format!("{err:#}")));
                }
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::GeneratePlan(task) => {
                let _ = ui.send(UiMessage::Status("planning…".to_string()));
                let tools = session.registry().names();
                match session
                    .planner()
                    .create(&task, &session.context().cwd, &tools)
                    .await
                {
                    Ok(plan) => session.bus().publish(Event::Plan(plan)),
                    Err(err) => {
                        let message = format!("planning failed: {err:#}");
                        session.bus().publish(Event::Error(message.clone()));
                        let _ = ui.send(UiMessage::Error(message));
                    }
                }
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::ExecutePlan { mut plan, selected } => {
                let tx = ui.clone();
                let mut sink = move |event: AgentEvent| {
                    let _ = tx.send(UiMessage::Agent(event));
                };
                match executor::execute_plan(&mut session, &mut plan, &selected, &mut sink).await {
                    Ok(outcome) => {
                        let _ = ui.send(UiMessage::PlanFinished(outcome));
                    }
                    Err(err) => {
                        let _ = ui.send(UiMessage::Error(format!("{err:#}")));
                    }
                }
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::SetModel(model) => {
                session.set_model(model.clone());
                let _ = ui.send(UiMessage::ModelChanged {
                    provider: session.provider().name().to_string(),
                    model,
                });
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::SetProvider(name) => {
                match provider::resolve(&config, Some(&name), None).await {
                    Ok(resolved) => {
                        let model = resolved.model.clone();
                        let provider = resolved.provider.name().to_string();
                        session.set_provider(resolved.provider, model.clone());
                        let _ = ui.send(UiMessage::ProviderChanged {
                            provider: provider.clone(),
                            model,
                        });
                        let _ = ui.send(UiMessage::Status(format!("switched to {provider}")));
                        let _ = ui.send(UiMessage::TurnFinished);
                    }
                    Err(err) => {
                        let _ = ui.send(UiMessage::Error(format!("{err:#}")));
                    }
                }
            }
            AgentCommand::ListModels => match session.provider().list_models().await {
                Ok(models) => {
                    let _ = ui.send(UiMessage::Models {
                        provider: session.provider().name().to_string(),
                        current: session.model().to_string(),
                        models,
                    });
                }
                Err(err) => {
                    let _ = ui.send(UiMessage::Error(format!("{err:#}")));
                }
            },
            AgentCommand::SetMode(mode) => {
                session.set_approval_mode(mode);
                let _ = ui.send(UiMessage::ModeChanged(mode));
            }
            AgentCommand::ClearHistory => {
                session.reset();
                let _ = ui.send(UiMessage::Status("history cleared".to_string()));
            }
            AgentCommand::NewSession(id) => {
                sessions.insert(current_session.clone(), session.export_messages());
                current_session = id;
                session.reset();
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::SwitchSession(id) => {
                if id != current_session {
                    sessions.insert(current_session.clone(), session.export_messages());
                    match sessions.get(&id).cloned() {
                        Some(messages) => session.import_messages(messages),
                        None => session.reset(),
                    }
                    current_session = id;
                }
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::DeleteSession(id) => {
                if id != current_session {
                    sessions.remove(&id);
                }
            }
        }
    }

    bus_task.abort();
}

async fn run_turn(
    session: &mut AgentSession,
    prompt: &str,
    ui: &mpsc::UnboundedSender<UiMessage>,
) -> Result<()> {
    let tx = ui.clone();
    let mut sink = move |event: AgentEvent| {
        let _ = tx.send(UiMessage::Agent(event));
    };
    session.run_turn(prompt, &mut sink).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentEvent, AgentSession};
    use crate::config::{ApprovalMode, Config, ToolsConfig};
    use crate::provider::types::{ProviderStream, Role};
    use crate::provider::{ChatRequest, ChatResponse, StreamEvent};
    use crate::tools::{ToolContext, ToolRegistry};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::time::timeout;

    struct EchoProvider;

    #[async_trait]
    impl provider::Provider for EchoProvider {
        fn name(&self) -> &str {
            "echo"
        }

        fn base_url(&self) -> &str {
            "http://localhost"
        }

        async fn chat_stream(&self, request: ChatRequest) -> anyhow::Result<ProviderStream> {
            let users: Vec<&str> = request
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .filter_map(|message| message.content.as_deref())
                .collect();
            let response = ChatResponse {
                content: users.join("|"),
                ..ChatResponse::default()
            };
            let (tx, stream) = ProviderStream::channel(4);
            tx.send(Ok(StreamEvent::Finished(response))).await.unwrap();
            drop(tx);
            Ok(stream)
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(vec!["echo".to_string()])
        }
    }

    fn session(dir: &std::path::Path) -> AgentSession {
        let mut config = Config::default();
        config.general.approval_mode = ApprovalMode::FullAuto;
        let ctx = ToolContext::from_config(dir, &ToolsConfig::default()).unwrap();
        AgentSession::new(
            Arc::new(EchoProvider),
            "echo".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        )
    }

    async fn next_turn(rx: &mut mpsc::UnboundedReceiver<UiMessage>) -> String {
        let mut text = String::new();
        loop {
            let message = timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("worker timed out")
                .expect("worker channel closed");
            match message {
                UiMessage::Agent(AgentEvent::TextDelta(delta)) => text.push_str(&delta),
                UiMessage::TurnFinished => return text,
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn switches_session_histories() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(run(session(dir.path()), config, command_rx, ui_tx));

        command_tx
            .send(AgentCommand::Submit("first".to_string()))
            .unwrap();
        assert_eq!(next_turn(&mut ui_rx).await, "first");

        command_tx
            .send(AgentCommand::NewSession("s2".to_string()))
            .unwrap();
        let _ = next_turn(&mut ui_rx).await;

        command_tx
            .send(AgentCommand::Submit("second".to_string()))
            .unwrap();
        assert_eq!(next_turn(&mut ui_rx).await, "second");

        command_tx
            .send(AgentCommand::SwitchSession("default".to_string()))
            .unwrap();
        let _ = next_turn(&mut ui_rx).await;

        command_tx
            .send(AgentCommand::Submit("third".to_string()))
            .unwrap();
        assert_eq!(next_turn(&mut ui_rx).await, "first|third");

        worker.abort();
    }
}
