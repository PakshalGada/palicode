//! Agent worker: owns the session and communicates with the TUI via channels.

use crate::agent::executor::{self, PlanOutcome};
use crate::agent::plan::Plan;
use crate::agent::{AgentEvent, AgentSession, CheckpointSink};
use crate::approval::{ApprovalRequest, ApprovalResponse, Approver};
use crate::config::{ApprovalMode, Config};
use crate::event::Event;
use crate::provider::{self, Message};
use crate::state::StateStore;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

#[derive(Debug)]
pub enum AgentCommand {
    Submit(String),
    GeneratePlan(String),
    ExecutePlan { plan: Plan, selected: Vec<usize> },
    SetModel(String),
    SetProvider(String),
    SetProviderModel { provider: String, model: String },
    ListAllModels,
    SetMode(ApprovalMode),
    ClearHistory,
    NewSession(String),
    SwitchSession(String),
    DeleteSession(String),
}

#[derive(Debug, Clone)]
pub struct ModelGroup {
    pub provider: String,
    pub models: Vec<String>,
    pub error: Option<String>,
    pub has_credentials: bool,
}

pub enum UiMessage {
    Agent(AgentEvent),
    Bus(Event),
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalResponse>,
    },
    ModelGroups {
        current_provider: String,
        current_model: String,
        groups: Vec<ModelGroup>,
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

struct StateCheckpointer {
    store: StateStore,
    session_id: Arc<Mutex<String>>,
}

impl CheckpointSink for StateCheckpointer {
    fn checkpoint(&self, messages: &[Message], kind: &str) {
        let session_id = self
            .session_id
            .lock()
            .map(|id| id.clone())
            .unwrap_or_else(|_| "default".to_string());
        if let Err(err) = self.store.save_checkpoint(&session_id, messages, kind) {
            warn!(error = %err, session = %session_id, "checkpoint failed");
        }
    }
}

fn set_session_id(shared: &Arc<Mutex<String>>, id: &str) {
    if let Ok(mut current) = shared.lock() {
        *current = id.to_string();
    }
}

pub fn spawn(
    mut session: AgentSession,
    config: Config,
    store: Option<StateStore>,
    initial_session: String,
    commands: mpsc::UnboundedReceiver<AgentCommand>,
    ui: mpsc::UnboundedSender<UiMessage>,
) -> tokio::task::JoinHandle<()> {
    let session_id = Arc::new(Mutex::new(initial_session.clone()));
    if let Some(store) = &store {
        session.set_checkpoint_sink(Arc::new(StateCheckpointer {
            store: store.clone(),
            session_id: Arc::clone(&session_id),
        }));
    }
    session.set_approver(Arc::new(TuiApprover { tx: ui.clone() }));
    tokio::spawn(async move {
        run(
            session,
            config,
            store,
            initial_session,
            session_id,
            commands,
            ui,
        )
        .await;
    })
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

async fn run(
    mut session: AgentSession,
    config: Config,
    store: Option<StateStore>,
    initial_session: String,
    session_id: Arc<Mutex<String>>,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
    ui: mpsc::UnboundedSender<UiMessage>,
) {
    let mut sessions: HashMap<String, Vec<Message>> = HashMap::new();
    let mut current_session = initial_session;
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
                } else {
                    match session.maybe_compact().await {
                        Ok(true) => {
                            session
                                .bus()
                                .publish(Event::Status("context compacted".to_string()));
                        }
                        Ok(false) => {}
                        Err(err) => {
                            warn!(error = %err, "context compaction failed");
                        }
                    }
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
                    Ok(plan) => {
                        if let Some(store) = &store
                            && let Err(err) = store.save_plan(&current_session, &plan)
                        {
                            warn!(error = %err, "cannot persist plan");
                        }
                        session.bus().publish(Event::Plan(plan));
                    }
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
                        if let Some(store) = &store
                            && let Err(err) = store.save_plan(&current_session, &plan)
                        {
                            warn!(error = %err, "cannot persist plan status");
                        }
                        match session.maybe_compact().await {
                            Ok(true) => session
                                .bus()
                                .publish(Event::Status("context compacted".to_string())),
                            Ok(false) => {}
                            Err(err) => {
                                warn!(error = %err, "context compaction failed");
                            }
                        }
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
            AgentCommand::SetProviderModel { provider, model } => {
                match provider::resolve(&config, Some(&provider), Some(&model)).await {
                    Ok(resolved) => {
                        let provider = resolved.provider.name().to_string();
                        let model = resolved.model;
                        session.set_provider(resolved.provider, model.clone());
                        let _ = ui.send(UiMessage::ProviderChanged { provider, model });
                        let _ = ui.send(UiMessage::TurnFinished);
                    }
                    Err(err) => {
                        let _ = ui.send(UiMessage::Error(format!("{err:#}")));
                    }
                }
            }
            AgentCommand::ListAllModels => {
                let probes = provider::probe_all(&config).await;
                let groups = probes
                    .into_iter()
                    .map(|probe| {
                        let (models, error) = match probe.result {
                            Ok(models) => (models, None),
                            Err(error) => (Vec::new(), Some(error)),
                        };
                        ModelGroup {
                            provider: probe.name,
                            models,
                            error,
                            has_credentials: probe.has_credentials,
                        }
                    })
                    .collect();
                let _ = ui.send(UiMessage::ModelGroups {
                    current_provider: session.provider().name().to_string(),
                    current_model: session.model().to_string(),
                    groups,
                });
            }
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
                set_session_id(&session_id, &id);
                current_session = id;
                session.reset();
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::SwitchSession(id) => {
                if id != current_session {
                    sessions.insert(current_session.clone(), session.export_messages());
                    let messages = match sessions.get(&id).cloned() {
                        Some(messages) => Some(messages),
                        None => store
                            .as_ref()
                            .and_then(|store| store.load_messages(&id).ok())
                            .filter(|messages| !messages.is_empty()),
                    };
                    set_session_id(&session_id, &id);
                    current_session = id;
                    match messages {
                        Some(messages) => session.import_messages(messages),
                        None => session.reset(),
                    }
                }
                let _ = ui.send(UiMessage::TurnFinished);
            }
            AgentCommand::DeleteSession(id) => {
                if id != current_session {
                    sessions.remove(&id);
                    if let Some(store) = &store
                        && let Err(err) = store.delete_session(&id)
                    {
                        warn!(error = %err, "cannot delete session from state");
                    }
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
    use crate::state::{SESSION_ACTIVE, SessionRecord, now_epoch};
    use crate::tools::{ToolContext, ToolRegistry};
    use async_trait::async_trait;
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
    async fn checkpoints_sessions_to_sqlite() {
        let dir = tempdir().unwrap();
        let store = StateStore::open_at(&dir.path().join("state.db")).unwrap();
        let now = now_epoch();
        store
            .insert_session(&SessionRecord {
                id: "default".to_string(),
                title: "Test".to_string(),
                provider: "echo".to_string(),
                model: "echo".to_string(),
                cwd: dir.path().display().to_string(),
                status: SESSION_ACTIVE.to_string(),
                turns: 0,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let config = Config::default();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let shared_id = Arc::new(Mutex::new("default".to_string()));
        let mut session = session(dir.path());
        session.set_checkpoint_sink(Arc::new(StateCheckpointer {
            store: store.clone(),
            session_id: shared_id.clone(),
        }));
        let worker = tokio::spawn(run(
            session,
            config,
            Some(store.clone()),
            "default".to_string(),
            shared_id,
            command_rx,
            ui_tx,
        ));

        command_tx
            .send(AgentCommand::Submit("first".to_string()))
            .unwrap();
        assert_eq!(next_turn(&mut ui_rx).await, "first");
        let persisted = store.load_messages("default").unwrap();
        assert!(
            persisted
                .iter()
                .any(|message| message.content.as_deref() == Some("first"))
        );

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

        let default_messages = store.load_messages("default").unwrap();
        let users: Vec<&str> = default_messages
            .iter()
            .filter(|message| message.role == Role::User)
            .filter_map(|message| message.content.as_deref())
            .collect();
        assert_eq!(users, vec!["first", "third"]);
        let second_messages = store.load_messages("s2").unwrap();
        let second_users: Vec<&str> = second_messages
            .iter()
            .filter(|message| message.role == Role::User)
            .filter_map(|message| message.content.as_deref())
            .collect();
        assert_eq!(second_users, vec!["second"]);
        worker.abort();
    }

    #[tokio::test]
    async fn switches_session_histories() {
        let dir = tempdir().unwrap();
        let config = Config::default();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(run(
            session(dir.path()),
            config,
            None,
            "default".to_string(),
            Arc::new(Mutex::new("default".to_string())),
            command_rx,
            ui_tx,
        ));

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
