//! Agent turn loop: plan -> think -> act -> observe -> reflect.
//!
//! Every event is rendered through the caller's sink and published on the
//! [`EventBus`] for future front-ends. Tool calls pass through the permission
//! engine and approval gate before execution.

pub mod executor;
pub mod plan;
pub mod subagent;

use crate::approval::{
    ApprovalRequest, ApprovalResponse, Approver, Decision, NonInteractiveApprover,
    PermissionEngine, subjects_for,
};
use crate::config::{ApprovalMode, Config};
use crate::event::{Event, EventBus};
use crate::provider::{ChatRequest, ChatResponse, Message, Provider, ToolCall, Usage};
use crate::tools::{ToolContext, ToolOutput, ToolRegistry};
use crate::util::text::truncate_middle;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tracing::debug;

use self::subagent::SessionSubagentRunner;

/// Events emitted while a turn runs.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    AssistantMessage,
    ToolCall {
        /// Correlates with the tool result; used by the event bus/TUI.
        #[allow(dead_code)]
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        /// Correlates with the tool call; used by the event bus/TUI.
        #[allow(dead_code)]
        id: String,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: u128,
    },
    PermissionDenied {
        tool: String,
        reason: String,
    },
    Status(String),
    Diff {
        /// Kept for the Phase 3 diff pane; the REPL shows the tool summary.
        #[allow(dead_code)]
        tool: String,
        #[allow(dead_code)]
        diff: String,
    },
    Usage(Usage),
    StepLimit {
        steps: usize,
    },
}

pub type EventSink<'a> = &'a mut (dyn FnMut(AgentEvent) + Send);

/// Per-turn options.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnOptions {
    /// 1-based plan step currently executing. Its approval gates mutations.
    pub plan_step: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct TurnResult {
    pub text: String,
    pub steps: usize,
    pub tool_calls: usize,
}

pub struct AgentSession {
    provider: Arc<dyn Provider>,
    model: String,
    registry: ToolRegistry,
    ctx: ToolContext,
    messages: Vec<Message>,
    max_steps: usize,
    system_prompt: String,
    permissions: PermissionEngine,
    approver: Arc<dyn Approver>,
    bus: EventBus,
    config: Arc<Config>,
    depth: usize,
}

impl AgentSession {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        registry: ToolRegistry,
        ctx: ToolContext,
        config: &Config,
    ) -> Self {
        Self::new_internal(
            provider,
            model,
            registry,
            ctx,
            Arc::new(config.clone()),
            EventBus::default(),
            0,
        )
    }

    pub(crate) fn new_internal(
        provider: Arc<dyn Provider>,
        model: String,
        registry: ToolRegistry,
        ctx: ToolContext,
        config: Arc<Config>,
        bus: EventBus,
        depth: usize,
    ) -> Self {
        let system_prompt = config
            .general
            .system_prompt
            .clone()
            .unwrap_or_else(|| default_system_prompt(&ctx.cwd));
        let messages = vec![Message::system(system_prompt.clone())];
        let permissions = PermissionEngine::new(config.general.approval_mode, &config.permissions);
        // Subagents and headless sessions cannot prompt; anything that still
        // needs approval is denied rather than silently allowed.
        let approver: Arc<dyn Approver> = Arc::new(NonInteractiveApprover::default());

        let mut session = Self {
            provider,
            model,
            registry,
            ctx,
            messages,
            max_steps: config.general.max_steps.max(1),
            system_prompt,
            permissions,
            approver,
            bus,
            config,
            depth,
        };
        session.install_subagent();
        session
    }

    fn install_subagent(&mut self) {
        // Never let the runner hold a context that already owns the runner.
        self.ctx.subagent = None;
        if self.depth < 2 && self.registry.get("task").is_some() {
            let runner = SessionSubagentRunner::new(
                Arc::clone(&self.provider),
                self.model.clone(),
                self.registry.clone(),
                self.ctx.clone(),
                Arc::clone(&self.config),
                self.bus.clone(),
                self.depth + 1,
            );
            self.ctx.subagent = Some(Arc::new(runner));
        }
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        self.install_subagent();
    }

    pub fn set_provider(&mut self, provider: Arc<dyn Provider>, model: impl Into<String>) {
        self.provider = provider;
        self.model = model.into();
        self.install_subagent();
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    pub fn context(&self) -> &ToolContext {
        &self.ctx
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Snapshot the conversation for session switching.
    pub fn export_messages(&self) -> Vec<Message> {
        self.messages.clone()
    }

    /// Replace the conversation (must include a system message).
    pub fn import_messages(&mut self, messages: Vec<Message>) {
        if messages.is_empty() {
            self.reset();
        } else {
            self.messages = messages;
        }
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
        self.reset();
    }

    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    pub fn planner(&self) -> plan::Planner {
        plan::Planner::new(
            Arc::clone(&self.provider),
            self.model.clone(),
            self.max_steps,
        )
    }

    pub fn approval_mode(&self) -> ApprovalMode {
        self.permissions.mode()
    }

    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.permissions.set_mode(mode);
    }

    pub fn set_approver(&mut self, approver: Arc<dyn Approver>) {
        self.approver = approver;
    }

    /// Clear conversation history but keep the current system prompt.
    pub fn reset(&mut self) {
        self.messages = vec![Message::system(self.system_prompt.clone())];
    }

    /// Run one user turn with default options.
    pub async fn run_turn(&mut self, input: &str, emit: EventSink<'_>) -> Result<TurnResult> {
        self.run_turn_with(input, TurnOptions::default(), emit)
            .await
    }

    /// Run one user turn, executing any tool calls the model requests.
    pub async fn run_turn_with(
        &mut self,
        input: &str,
        options: TurnOptions,
        emit: EventSink<'_>,
    ) -> Result<TurnResult> {
        self.messages.push(Message::user(input));
        self.bus.publish(Event::Status("thinking".to_string()));

        let mut result = TurnResult::default();
        for step in 0..self.max_steps {
            result.steps = step + 1;
            let request = ChatRequest {
                model: self.model.clone(),
                messages: self.messages.clone(),
                tools: self.registry.specs(),
                temperature: None,
                max_tokens: None,
            };

            let (response, text_streamed) = match self.stream_response(request, emit).await {
                Ok(value) => value,
                Err(err) => {
                    self.bus.publish(Event::Error(format!("{err:#}")));
                    return Err(err);
                }
            };
            if let Some(usage) = response.usage {
                dispatch(&self.bus, &mut *emit, AgentEvent::Usage(usage));
            }

            if !response.content.is_empty() {
                if !text_streamed {
                    // E.g. a thinking model that returned the answer only in
                    // the reasoning channel; surface the recovered text.
                    dispatch(
                        &self.bus,
                        &mut *emit,
                        AgentEvent::TextDelta(response.content.clone()),
                    );
                }
                result.text = response.content.clone();
            }
            debug!(
                finish_reason = ?response.finish_reason,
                tool_calls = response.tool_calls.len(),
                "model response complete"
            );
            let calls = response.tool_calls.clone();
            dispatch(&self.bus, &mut *emit, AgentEvent::AssistantMessage);

            let content = if response.content.is_empty() {
                None
            } else {
                Some(response.content)
            };
            self.messages
                .push(Message::assistant(content, calls.clone()));

            if calls.is_empty() {
                self.bus.publish(Event::Status("idle".to_string()));
                return Ok(result);
            }

            for call in calls {
                result.tool_calls += 1;
                let observation = self
                    .execute_tool_call(call, options.plan_step, &mut *emit)
                    .await;
                self.messages
                    .push(Message::tool_result(&observation.id, observation.output));
            }
        }

        let message = format!(
            "step limit reached ({} steps); the task may need a narrower prompt",
            self.max_steps
        );
        self.bus.publish(Event::Error(message.clone()));
        dispatch(
            &self.bus,
            &mut *emit,
            AgentEvent::StepLimit {
                steps: self.max_steps,
            },
        );
        bail!(message)
    }

    async fn stream_response(
        &self,
        request: ChatRequest,
        emit: EventSink<'_>,
    ) -> Result<(ChatResponse, bool)> {
        let mut stream = self
            .provider
            .chat_stream(request)
            .await
            .with_context(|| format!("provider '{}' request failed", self.provider.name()))?;

        let mut response: Option<ChatResponse> = None;
        let mut text_streamed = false;
        while let Some(event) = stream.next().await {
            match event? {
                crate::provider::StreamEvent::TextDelta(delta) => {
                    text_streamed = true;
                    dispatch(&self.bus, &mut *emit, AgentEvent::TextDelta(delta));
                }
                crate::provider::StreamEvent::ReasoningDelta(delta) => {
                    dispatch(&self.bus, &mut *emit, AgentEvent::ReasoningDelta(delta));
                }
                crate::provider::StreamEvent::Finished(finished) => {
                    response = Some(finished);
                }
            }
        }
        let response = response.context("provider stream ended without a completion")?;
        Ok((response, text_streamed))
    }

    /// Check permissions and ask the approver when required.
    async fn authorize(
        &mut self,
        name: &str,
        args: &Value,
        read_only: bool,
        plan_step: Option<usize>,
        emit: EventSink<'_>,
    ) -> std::result::Result<(), String> {
        let decision = self
            .permissions
            .check(name, args, read_only, plan_step.is_some());
        match decision {
            Decision::Allow => Ok(()),
            Decision::Deny(reason) => {
                dispatch(
                    &self.bus,
                    &mut *emit,
                    AgentEvent::PermissionDenied {
                        tool: name.to_string(),
                        reason: reason.clone(),
                    },
                );
                Err(format!("permission denied: {reason}"))
            }
            Decision::Ask(reason) => {
                let request = ApprovalRequest {
                    tool: name.to_string(),
                    subject: subjects_for(name, args).join(", "),
                    arguments: args.clone(),
                    reason,
                    plan_step,
                };
                match self.approver.approve(&request).await {
                    Ok(ApprovalResponse::Allow) => Ok(()),
                    Ok(ApprovalResponse::AllowAlways) => {
                        self.permissions.allow_tool_for_session(name);
                        Ok(())
                    }
                    Ok(ApprovalResponse::Deny { reason }) => {
                        dispatch(
                            &self.bus,
                            &mut *emit,
                            AgentEvent::PermissionDenied {
                                tool: name.to_string(),
                                reason: reason.clone(),
                            },
                        );
                        Err(format!("permission denied: {reason}"))
                    }
                    Err(err) => Err(format!("approval failed: {err:#}")),
                }
            }
        }
    }

    async fn execute_tool_call(
        &mut self,
        call: ToolCall,
        plan_step: Option<usize>,
        emit: EventSink<'_>,
    ) -> Observation {
        dispatch(
            &self.bus,
            &mut *emit,
            AgentEvent::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        );

        let started = Instant::now();
        let parsed = if call.arguments.trim().is_empty() {
            Ok(serde_json::json!({}))
        } else {
            serde_json::from_str::<Value>(&call.arguments)
                .with_context(|| "tool arguments are not valid JSON")
        };

        let output: std::result::Result<ToolOutput, String> = match parsed {
            Ok(args) if args.is_object() => {
                let Some(tool) = self.registry.get(&call.name) else {
                    let error = format!("unknown tool '{}'", call.name);
                    let duration_ms = started.elapsed().as_millis();
                    return self.finish_observation(call, Err(error), duration_ms, &mut *emit);
                };

                match self
                    .authorize(&call.name, &args, tool.read_only(), plan_step, &mut *emit)
                    .await
                {
                    Ok(()) => match self.registry.execute(&call.name, args, &self.ctx).await {
                        Ok(output) => {
                            if let Some(diff) = &output.diff {
                                dispatch(
                                    &self.bus,
                                    &mut *emit,
                                    AgentEvent::Diff {
                                        tool: call.name.clone(),
                                        diff: truncate_middle(diff, 200_000),
                                    },
                                );
                            }
                            Ok(output)
                        }
                        Err(err) => Err(format!("{err:#}")),
                    },
                    Err(error) => Err(error),
                }
            }
            Ok(_) => Err("tool arguments must be a JSON object".to_string()),
            Err(err) => Err(format!("{err:#}")),
        };

        let duration_ms = started.elapsed().as_millis();
        self.finish_observation(call, output, duration_ms, &mut *emit)
    }

    fn finish_observation(
        &self,
        call: ToolCall,
        output: std::result::Result<ToolOutput, String>,
        duration_ms: u128,
        emit: EventSink<'_>,
    ) -> Observation {
        let (content, is_error) = match output {
            Ok(ToolOutput { content, .. }) => (content, false),
            Err(error) => (
                format!(
                    "ERROR: {error}\n\
                     [reflection] The tool call failed. Diagnose the cause before proceeding; \
                     do not repeat the same call unchanged."
                ),
                true,
            ),
        };
        let content = truncate_middle(&content, self.ctx.max_output_bytes);
        debug!(
            tool = %call.name,
            ms = duration_ms,
            is_error,
            "tool observation"
        );
        dispatch(
            &self.bus,
            &mut *emit,
            AgentEvent::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                output: content.clone(),
                is_error,
                duration_ms,
            },
        );
        self.bus.publish(Event::Status("thinking".to_string()));

        Observation {
            id: call.id,
            output: content,
        }
    }
}

fn dispatch(bus: &EventBus, emit: &mut (dyn FnMut(AgentEvent) + Send), event: AgentEvent) {
    bus.publish(Event::Agent(event.clone()));
    emit(event);
}

struct Observation {
    id: String,
    output: String,
}

pub fn default_system_prompt(cwd: &Path) -> String {
    format!(
        "You are palicode, a terminal coding agent working in {cwd}.\n\
         You complete tasks by calling tools, observing results, and iterating.\n\n\
         Rules:\n\
         - Read a file before editing it. Use grep/glob to locate code instead of guessing paths.\n\
         - Use edit_file for one localized change; use apply_patch for multi-file or multi-hunk changes.\n\
         - Use the task tool for read-only research that would otherwise flood the conversation.\n\
         - All paths are relative to the working directory and confined to the sandbox.\n\
         - Verify your work with run_shell (tests, build, git diff) before claiming success.\n\
         - Never invent file contents or command output. If something fails, report it and fix it.\n\
         - Keep answers concise and lead with the outcome.",
        cwd = cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApprovalMode;
    use crate::provider::types::ProviderStream;
    use crate::provider::{ChatResponse, StreamEvent, ToolCall};
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct ScriptedProvider {
        responses: Mutex<VecDeque<ChatResponse>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
            })
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn base_url(&self) -> &str {
            "http://localhost"
        }

        async fn chat_stream(&self, _request: ChatRequest) -> Result<ProviderStream> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ChatResponse {
                    content: "done".to_string(),
                    ..ChatResponse::default()
                });
            let (tx, stream) = ProviderStream::channel(4);
            tx.send(Ok(StreamEvent::Finished(response))).await.unwrap();
            drop(tx);
            Ok(stream)
        }

        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec!["scripted".to_string()])
        }
    }

    fn write_then_finish() -> Vec<ChatResponse> {
        vec![
            ChatResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "write_file".to_string(),
                    arguments: json!({"path": "a.txt", "content": "x"}).to_string(),
                }],
                usage: None,
                finish_reason: Some("tool_calls".to_string()),
            },
            ChatResponse {
                content: "finished".to_string(),
                ..ChatResponse::default()
            },
        ]
    }

    fn test_config(mode: ApprovalMode) -> Config {
        let mut config = Config::default();
        config.general.approval_mode = mode;
        config
    }

    fn context(root: &Path) -> ToolContext {
        ToolContext::from_config(root, &Config::default().tools).unwrap()
    }

    #[tokio::test]
    async fn exports_and_imports_session_history() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::FullAuto);
        let provider = ScriptedProvider::new(vec![ChatResponse {
            content: "hello back".to_string(),
            ..ChatResponse::default()
        }]);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        session.run_turn("hello", &mut |_| {}).await.unwrap();
        let messages = session.export_messages();
        assert!(
            messages
                .iter()
                .any(|message| message.content.as_deref() == Some("hello"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.content.as_deref() == Some("hello back"))
        );

        session.reset();
        assert_eq!(session.messages().len(), 1);
        session.import_messages(messages);
        assert!(
            session
                .messages()
                .iter()
                .any(|message| message.content.as_deref() == Some("hello back"))
        );
    }

    #[tokio::test]
    async fn suggest_mode_denies_mutations_without_an_approver() {
        let provider = ScriptedProvider::new(write_then_finish());
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::Suggest);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        let mut events = Vec::new();
        let result = session
            .run_turn("create a.txt", &mut |event| events.push(event))
            .await;

        result.unwrap();
        assert!(!dir.path().join("a.txt").exists());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::PermissionDenied { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolResult { is_error: true, .. }))
        );
    }

    #[tokio::test]
    async fn full_auto_mode_executes_mutations() {
        let provider = ScriptedProvider::new(write_then_finish());
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::FullAuto);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        let mut events = Vec::new();
        let result = session
            .run_turn("create a.txt", &mut |event| events.push(event))
            .await;

        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::PermissionDenied { .. }))
        );
    }

    #[tokio::test]
    async fn plan_step_approval_gates_mutations() {
        let provider = ScriptedProvider::new(write_then_finish());
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::Suggest);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        let mut events = Vec::new();
        let result = session
            .run_turn_with(
                "execute plan step",
                TurnOptions { plan_step: Some(1) },
                &mut |event| events.push(event),
            )
            .await;

        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::PermissionDenied { .. }))
        );
    }
}
