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
use crate::memory::{self, ProjectMemory};
use crate::provider::types::Role;
use crate::provider::{ChatRequest, ChatResponse, Message, Provider, ToolCall, Usage};
use crate::token::{ContextUsage, estimate_json_tokens, estimate_tokens};
use crate::tools::{ToolContext, ToolOutput, ToolRegistry};
use crate::util::text::truncate_middle;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
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
    ResponseStats {
        duration_ms: u128,
        completion_tokens: u64,
        estimated: bool,
        model: String,
    },
    StepLimit {
        steps: usize,
    },
}

pub type EventSink<'a> = &'a mut (dyn FnMut(AgentEvent) + Send);

/// Receives durable conversation snapshots (implemented by the SQLite layer).
pub trait CheckpointSink: Send + Sync {
    fn checkpoint(&self, messages: &[Message], kind: &str);
}

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
    /// The user-configured prompt, before project memory is appended.
    base_system_prompt: String,
    system_prompt: String,
    permissions: PermissionEngine,
    approver: Arc<dyn Approver>,
    bus: EventBus,
    config: Arc<Config>,
    depth: usize,
    checkpoint_sink: Option<Arc<dyn CheckpointSink>>,
    compact_after_chars: usize,
    compact_keep_messages: usize,
    verify_command: Option<String>,
    verify_retries: usize,
    memory: ProjectMemory,
    loaded_rules: HashSet<PathBuf>,
    pending_reminders: Vec<String>,
    file_state: HashMap<PathBuf, FileStamp>,
    memory_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    fn capture(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
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
        let compact_after_chars = config.general.compact_after_chars;
        let compact_keep_messages = config.general.compact_keep_messages;
        let verify_command = config.general.verify_command.clone();
        let verify_retries = config.general.verify_retries;

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
            checkpoint_sink: None,
            compact_after_chars,
            compact_keep_messages,
            verify_command,
            verify_retries,
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

    pub fn set_checkpoint_sink(&mut self, sink: Arc<dyn CheckpointSink>) {
        self.checkpoint_sink = Some(sink);
        self.checkpoint("attach");
    }

    fn checkpoint(&self, kind: &str) {
        if let Some(sink) = &self.checkpoint_sink {
            sink.checkpoint(&self.messages, kind);
        }
    }

    pub fn verify_command(&self) -> Option<&str> {
        self.verify_command.as_deref()
    }

    pub fn verify_retries(&self) -> usize {
        self.verify_retries
    }

    /// Summarize older history once it grows beyond the configured threshold.
    pub async fn maybe_compact(&mut self) -> Result<bool> {
        let chars: usize = self
            .messages
            .iter()
            .map(|message| {
                message.content.as_ref().map_or(0, String::len)
                    + message
                        .tool_calls
                        .iter()
                        .map(|call| call.name.len() + call.arguments.len())
                        .sum::<usize>()
            })
            .sum();
        if chars < self.compact_after_chars || self.messages.len() <= self.compact_keep_messages + 2
        {
            return Ok(false);
        }

        let keep = self.compact_keep_messages.max(2);
        let mut keep_start = self.messages.len().saturating_sub(keep);
        while keep_start < self.messages.len() && self.messages[keep_start].role != Role::User {
            keep_start += 1;
        }
        if keep_start >= self.messages.len() {
            keep_start = self.messages.len().saturating_sub(keep);
        }
        if keep_start <= 1 {
            return Ok(false);
        }

        let transcript = render_transcript(&self.messages[1..keep_start]);
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![Message::system(COMPACTOR_PROMPT), Message::user(transcript)],
            tools: Vec::new(),
            temperature: Some(0.1),
            max_tokens: Some(1_500),
        };
        let response = self
            .provider
            .chat(request)
            .await
            .context("context compaction request failed")?;
        let summary = response.content.trim();
        if summary.is_empty() {
            bail!("context compactor returned an empty summary");
        }

        let system = self
            .messages
            .first()
            .cloned()
            .unwrap_or_else(|| Message::system(self.system_prompt.clone()));
        let mut compacted = Vec::with_capacity(2 + self.messages.len() - keep_start);
        compacted.push(system);
        compacted.push(Message::system(format!(
            "[context summary of earlier work]
{summary}"
        )));
        compacted.extend_from_slice(&self.messages[keep_start..]);
        self.messages = compacted;
        self.checkpoint("compaction");
        Ok(true)
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
    pub fn import_messages(&mut self, mut messages: Vec<Message>) {
        if messages.is_empty() {
            self.reset();
            return;
        }
        // A crash can leave an assistant tool call without its observation.
        // Close those calls so the resumed conversation is valid.
        let mut pending: HashMap<String, String> = HashMap::new();
        for message in &messages {
            if message.role == Role::Assistant {
                for call in &message.tool_calls {
                    pending.insert(call.id.clone(), call.name.clone());
                }
            } else if message.role == Role::Tool
                && let Some(id) = &message.tool_call_id
            {
                pending.remove(id);
            }
        }
        for (id, name) in pending {
            messages.push(Message::tool_result(
                id,
                format!(
                    "ERROR: tool '{name}' was interrupted before returning a result;                      re-check the workspace state before retrying."
                ),
            ));
        }
        self.messages = messages;
        self.checkpoint("import");
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
        self.checkpoint("reset");
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
        self.checkpoint("turn_start");
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

            let response_started = Instant::now();
            let (response, text_streamed) = match self.stream_response(request, emit).await {
                Ok(value) => value,
                Err(err) => {
                    self.bus.publish(Event::Error(format!("{err:#}")));
                    return Err(err);
                }
            };
            let duration_ms = response_started.elapsed().as_millis();
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
            if text_streamed || !response.content.trim().is_empty() {
                let provider_tokens = response
                    .usage
                    .map(|usage| usage.completion_tokens)
                    .filter(|tokens| *tokens > 0);
                let estimated = provider_tokens.is_none();
                let completion_tokens =
                    provider_tokens.unwrap_or_else(|| estimate_tokens(&response.content));
                dispatch(
                    &self.bus,
                    &mut *emit,
                    AgentEvent::ResponseStats {
                        duration_ms,
                        completion_tokens,
                        estimated,
                        model: format!("{}/{}", self.provider.name(), self.model),
                    },
                );
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
            self.checkpoint("assistant");

            if calls.is_empty() {
                self.checkpoint("turn_end");
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
                self.checkpoint("tool_result");
            }
        }

        let message = format!(
            "step limit reached ({} steps); the task may need a narrower prompt",
            self.max_steps
        );
        self.checkpoint("step_limit");
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

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4).max(1)
}

const COMPACTOR_PROMPT: &str = "You are compacting a coding-agent conversation for long-horizon continuation. \n\
     Preserve: the user's goals, decisions, files changed, commands/tests run and their outcomes, \n\
     unresolved failures, plan status, and concrete next steps. Drop chit-chat and redundant tool output. \n\
     Return a concise but complete summary as plain text.";

fn render_transcript(messages: &[Message]) -> String {
    let mut output = String::new();
    for message in messages {
        let label = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        output.push_str(label);
        if let Some(id) = &message.tool_call_id {
            output.push_str(&format!(" ({id})"));
        }
        output.push_str(": ");
        if let Some(content) = &message.content {
            output.push_str(&truncate_middle(content, 4_000));
        }
        for call in &message.tool_calls {
            output.push_str(&format!(
                "\n  tool_call {} {}",
                call.name,
                truncate_middle(&call.arguments, 500)
            ));
        }
        output.push_str("\n\n");
    }
    truncate_middle(&output, 120_000)
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

    struct RecordingSink {
        snapshots: Mutex<Vec<(String, usize)>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                snapshots: Mutex::new(Vec::new()),
            })
        }
    }

    impl CheckpointSink for RecordingSink {
        fn checkpoint(&self, messages: &[Message], kind: &str) {
            self.snapshots
                .lock()
                .unwrap()
                .push((kind.to_string(), messages.len()));
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
    async fn emits_response_timing_stats() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::FullAuto);
        let provider = ScriptedProvider::new(vec![ChatResponse {
            content: "hello there".to_string(),
            ..ChatResponse::default()
        }]);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        let mut events = Vec::new();
        session
            .run_turn("hi", &mut |event| events.push(event))
            .await
            .unwrap();
        let stats = events.iter().find_map(|event| match event {
            AgentEvent::ResponseStats {
                duration_ms,
                completion_tokens,
                estimated,
                model,
            } => Some((*duration_ms, *completion_tokens, *estimated, model.clone())),
            _ => None,
        });
        let (_, tokens, estimated, model) = stats.expect("missing response stats");
        assert!(tokens > 0);
        assert!(estimated);
        assert_eq!(model, "scripted/scripted");
    }

    #[tokio::test]
    async fn checkpoint_sink_receives_turn_snapshots() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::FullAuto);
        let provider = ScriptedProvider::new(vec![ChatResponse {
            content: "ok".to_string(),
            ..ChatResponse::default()
        }]);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        let sink = RecordingSink::new();
        session.set_checkpoint_sink(sink.clone());
        session.run_turn("hello", &mut |_| {}).await.unwrap();
        let snapshots = sink.snapshots.lock().unwrap();
        let kinds: Vec<&str> = snapshots.iter().map(|(kind, _)| kind.as_str()).collect();
        assert!(kinds.contains(&"attach"));
        assert!(kinds.contains(&"turn_start"));
        assert!(kinds.contains(&"turn_end"));
        assert!(snapshots.iter().any(|(_, len)| *len >= 3));
    }

    #[tokio::test]
    async fn resumes_after_an_interrupted_tool_call() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let config = test_config(ApprovalMode::FullAuto);
        let provider = ScriptedProvider::new(vec![]);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        session.import_messages(vec![
            Message::system("sys"),
            Message::assistant(
                Some(String::new()),
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "write_file".to_string(),
                    arguments: "{\"path\":\"a\",\"content\":\"x\"}".to_string(),
                }],
            ),
        ]);
        let messages = session.messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(
            messages[2]
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("ERROR:")
        );
    }

    #[tokio::test]
    async fn compacts_old_history_into_a_summary() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let mut config = test_config(ApprovalMode::FullAuto);
        config.general.compact_after_chars = 1_000;
        config.general.compact_keep_messages = 2;
        let provider = ScriptedProvider::new(vec![ChatResponse {
            content: "SUMMARY".to_string(),
            ..ChatResponse::default()
        }]);
        let mut session = AgentSession::new(
            provider,
            "scripted".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        session.import_messages(vec![
            Message::system("system"),
            Message::user("x".repeat(2_000)),
            Message::assistant(Some("old answer".to_string()), Vec::new()),
            Message::assistant(Some("another old answer".to_string()), Vec::new()),
            Message::user("recent question"),
        ]);
        assert!(session.maybe_compact().await.unwrap());
        let messages = session.messages();
        assert!(messages.len() < 4);
        assert!(
            messages.iter().any(|message| message
                .content
                .as_deref()
                .unwrap_or("")
                .contains("SUMMARY"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.content.as_deref() == Some("recent question"))
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
