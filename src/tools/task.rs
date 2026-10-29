//! Read-only research subagent tool (`task`).
//!
//! The actual runner lives in the agent layer because it needs a provider.
//! Phase 2 keeps subagents read-only; parallel dispatch arrives in Phase 5.

use super::{Tool, ToolContext, ToolOutput, required_str, schema_object};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct SubagentRequest {
    pub description: String,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct SubagentOutput {
    pub summary: String,
    pub steps: usize,
    pub tool_calls: usize,
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(&self, request: SubagentRequest) -> Result<SubagentOutput>;
}

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Delegate a focused research question to a read-only subagent. The subagent can read files, \
         search, list directories and inspect git, but cannot modify the workspace. Use it for broad \
         exploration so the main conversation stays small. Provide a complete, self-contained prompt."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "description": {
                    "type": "string",
                    "description": "Short label shown in logs, e.g. 'find config loading'."
                },
                "prompt": {
                    "type": "string",
                    "description": "Complete task for the subagent, including paths and what to report."
                }
            }),
            &["description", "prompt"],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let description = required_str(&args, "description")?.trim().to_string();
        let prompt = required_str(&args, "prompt")?.trim().to_string();
        if description.is_empty() || prompt.is_empty() {
            bail!("description and prompt must not be empty");
        }

        let runner = ctx
            .subagent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("subagents are not available in this context"))?;
        let result = runner
            .run(SubagentRequest {
                description: description.clone(),
                prompt,
            })
            .await?;

        let summary = if result.summary.trim().is_empty() {
            "(subagent produced no final answer)".to_string()
        } else {
            result.summary.trim().to_string()
        };
        Ok(ToolOutput::text(format!(
            "Subagent '{description}' finished ({} steps, {} tool calls):\n{summary}",
            result.steps, result.tool_calls
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;
    use crate::tools::Tool;
    use std::path::Path;
    use std::sync::Arc;

    struct FakeRunner;

    #[async_trait]
    impl SubagentRunner for FakeRunner {
        async fn run(&self, request: SubagentRequest) -> Result<SubagentOutput> {
            assert_eq!(request.description, "research");
            Ok(SubagentOutput {
                summary: "Found it in src/a.rs".to_string(),
                steps: 2,
                tool_calls: 3,
            })
        }
    }

    fn context(root: &Path) -> ToolContext {
        ToolContext::from_config(root, &ToolsConfig::default()).unwrap()
    }

    #[tokio::test]
    async fn fails_without_a_runner() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let err = TaskTool
            .execute(
                json!({"description": "research", "prompt": "find it"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not available"));
    }

    #[tokio::test]
    async fn delegates_to_the_runner() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = context(dir.path());
        ctx.subagent = Some(Arc::new(FakeRunner));
        let output = TaskTool
            .execute(
                json!({"description": "research", "prompt": "find it"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.content.contains("Found it in src/a.rs"));
        assert!(output.content.contains("2 steps"));
    }
}
