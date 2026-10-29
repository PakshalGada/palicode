//! Provider-backed runner for the read-only `task` tool.

use super::AgentSession;
use crate::config::Config;
use crate::event::{Event, EventBus, SubagentState};
use crate::provider::Provider;
use crate::tools::task::{SubagentOutput, SubagentRequest, SubagentRunner};
use crate::tools::{ToolContext, ToolRegistry};
use crate::util::text::one_line;
use anyhow::{Result, bail};
use async_trait::async_trait;
use std::sync::Arc;

const MAX_SUBAGENT_DEPTH: usize = 2;
const MAX_SUBAGENT_STEPS: usize = 12;

pub struct SessionSubagentRunner {
    provider: Arc<dyn Provider>,
    model: String,
    registry: ToolRegistry,
    ctx: ToolContext,
    config: Arc<Config>,
    bus: EventBus,
    depth: usize,
}

impl SessionSubagentRunner {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: String,
        registry: ToolRegistry,
        ctx: ToolContext,
        config: Arc<Config>,
        bus: EventBus,
        depth: usize,
    ) -> Self {
        Self {
            provider,
            model,
            registry,
            ctx,
            config,
            bus,
            depth,
        }
    }
}

#[async_trait]
impl SubagentRunner for SessionSubagentRunner {
    async fn run(&self, request: SubagentRequest) -> Result<SubagentOutput> {
        if self.depth > MAX_SUBAGENT_DEPTH {
            bail!("subagent recursion limit reached");
        }
        let description = request.description.clone();
        self.bus.publish(Event::Subagent {
            description: description.clone(),
            state: SubagentState::Started,
            message: None,
        });

        let mut config = (*self.config).clone();
        config.general.max_steps = config.general.max_steps.clamp(2, MAX_SUBAGENT_STEPS);
        let config = Arc::new(config);

        let mut session = AgentSession::new_internal(
            Arc::clone(&self.provider),
            self.model.clone(),
            self.registry.for_subagent(),
            self.ctx.clone(),
            config,
            EventBus::new(64),
            self.depth + 1,
        );
        session.set_system_prompt(subagent_system_prompt(&self.ctx.cwd));

        let mut sink = |_event| {};
        match session.run_turn(&request.prompt, &mut sink).await {
            Ok(turn) => {
                let summary = if turn.text.trim().is_empty() {
                    "(no final answer)".to_string()
                } else {
                    turn.text.trim().to_string()
                };
                self.bus.publish(Event::Subagent {
                    description,
                    state: SubagentState::Finished,
                    message: Some(one_line(&summary, 240)),
                });
                Ok(SubagentOutput {
                    summary,
                    steps: turn.steps,
                    tool_calls: turn.tool_calls,
                })
            }
            Err(err) => {
                self.bus.publish(Event::Subagent {
                    description,
                    state: SubagentState::Failed,
                    message: Some(one_line(&format!("{err:#}"), 240)),
                });
                Err(err)
            }
        }
    }
}

fn subagent_system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "You are a read-only research subagent working in {cwd}.\n\
         Use read_file, grep, glob, list_dir, git_status and git_diff to investigate.\n\
         You cannot modify files or run shell commands. Never claim to have changed anything.\n\
         Return a concise report with concrete file paths and line numbers.\n\
         If you cannot answer, state exactly what is missing.",
        cwd = cwd.display()
    )
}
