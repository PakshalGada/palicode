//! A single-agent turn loop: stream -> tool calls -> observations.
//!
//! This is deliberately small; Phase 2 grows it into the planner/approval
//! pipeline while keeping the same provider and tool interfaces.

use crate::config::Config;
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

/// Events emitted while a turn runs. The Phase 3 TUI will forward these over
/// the event bus; the REPL prints them directly.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    AssistantMessage,
    ToolCall {
        /// Correlates with the tool result; used by the Phase 3 event bus.
        #[allow(dead_code)]
        id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        /// Correlates with the tool call; used by the Phase 3 event bus.
        #[allow(dead_code)]
        id: String,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: u128,
    },
    Usage(Usage),
    StepLimit {
        steps: usize,
    },
}

pub type EventSink<'a> = &'a mut (dyn FnMut(AgentEvent) + Send);

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
}

impl AgentSession {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        registry: ToolRegistry,
        ctx: ToolContext,
        config: &Config,
    ) -> Self {
        let system_prompt = config
            .general
            .system_prompt
            .clone()
            .unwrap_or_else(|| default_system_prompt(&ctx.cwd));
        let messages = vec![Message::system(system_prompt.clone())];
        Self {
            provider,
            model,
            registry,
            ctx,
            messages,
            max_steps: config.general.max_steps.max(1),
            system_prompt,
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
    }

    pub fn set_provider(&mut self, provider: Arc<dyn Provider>, model: impl Into<String>) {
        self.provider = provider;
        self.model = model.into();
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

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn set_system_prompt(&mut self, prompt: impl Into<String>) {
        self.system_prompt = prompt.into();
        self.reset();
    }

    /// Clear conversation history but keep the current system prompt.
    pub fn reset(&mut self) {
        self.messages = vec![Message::system(self.system_prompt.clone())];
    }

    /// Run one user turn, executing any tool calls the model requests.
    pub async fn run_turn(&mut self, input: &str, emit: EventSink<'_>) -> Result<TurnResult> {
        self.messages.push(Message::user(input));

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

            let (response, text_streamed) = self.stream_response(request, emit).await?;
            if let Some(usage) = response.usage {
                emit(AgentEvent::Usage(usage));
            }

            if !response.content.is_empty() {
                if !text_streamed {
                    // E.g. a thinking model that returned the answer only in
                    // the reasoning channel; surface the recovered text.
                    emit(AgentEvent::TextDelta(response.content.clone()));
                }
                result.text = response.content.clone();
            }
            debug!(
                finish_reason = ?response.finish_reason,
                tool_calls = response.tool_calls.len(),
                "model response complete"
            );
            let calls = response.tool_calls.clone();
            emit(AgentEvent::AssistantMessage);

            let content = if response.content.is_empty() {
                None
            } else {
                Some(response.content)
            };
            self.messages
                .push(Message::assistant(content, calls.clone()));

            if calls.is_empty() {
                return Ok(result);
            }

            for call in calls {
                result.tool_calls += 1;
                let observation = self.execute_tool_call(call, emit).await;
                self.messages
                    .push(Message::tool_result(&observation.id, observation.output));
            }
        }

        emit(AgentEvent::StepLimit {
            steps: self.max_steps,
        });
        bail!(
            "step limit reached ({} steps); the task may need a narrower prompt",
            self.max_steps
        )
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
                    emit(AgentEvent::TextDelta(delta));
                }
                crate::provider::StreamEvent::ReasoningDelta(delta) => {
                    emit(AgentEvent::ReasoningDelta(delta));
                }
                crate::provider::StreamEvent::Finished(finished) => {
                    response = Some(finished);
                }
            }
        }
        let response = response.context("provider stream ended without a completion")?;
        Ok((response, text_streamed))
    }

    async fn execute_tool_call(&self, call: ToolCall, emit: EventSink<'_>) -> Observation {
        emit(AgentEvent::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });

        let started = Instant::now();
        let parsed = if call.arguments.trim().is_empty() {
            Ok(serde_json::json!({}))
        } else {
            serde_json::from_str::<Value>(&call.arguments)
                .with_context(|| "tool arguments are not valid JSON")
        };

        let output = match parsed {
            Ok(args) if args.is_object() => {
                match self.registry.execute(&call.name, args, &self.ctx).await {
                    Ok(ToolOutput { content }) => Ok(content),
                    Err(err) => Err(format!("{err:#}")),
                }
            }
            Ok(_) => Err("tool arguments must be a JSON object".to_string()),
            Err(err) => Err(format!("{err:#}")),
        };

        let duration_ms = started.elapsed().as_millis();
        let (content, is_error) = match output {
            Ok(content) => (content, false),
            Err(error) => (format!("ERROR: {error}"), true),
        };
        let content = truncate_middle(&content, self.ctx.max_output_bytes);
        debug!(
            tool = %call.name,
            ms = duration_ms,
            is_error,
            "tool observation"
        );
        emit(AgentEvent::ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            output: content.clone(),
            is_error,
            duration_ms,
        });

        Observation {
            id: call.id,
            output: content,
        }
    }
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
         - All paths are relative to the working directory and confined to the sandbox.\n\
         - Verify your work with run_shell (tests, build, git diff) before claiming success.\n\
         - Never invent file contents or command output. If something fails, report it and fix it.\n\
         - Keep answers concise and lead with the outcome.",
        cwd = cwd.display()
    )
}
