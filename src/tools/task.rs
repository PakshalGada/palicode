//! Read-only research subagent tool (`task`).
//!
//! The actual runner lives in the agent layer because it needs a provider.
//! Phase 2 keeps subagents read-only; parallel dispatch arrives in Phase 5.

use super::{Tool, ToolContext, ToolOutput, required_str, schema_object};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
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
    use tempfile::tempdir;

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

    struct ConcurrentRunner {
        active: Arc<std::sync::atomic::AtomicUsize>,
        max_active: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl SubagentRunner for ConcurrentRunner {
        async fn run(&self, request: SubagentRequest) -> Result<SubagentOutput> {
            use std::sync::atomic::Ordering;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(SubagentOutput {
                summary: format!("finished {}", request.description),
                steps: 1,
                tool_calls: 0,
            })
        }
    }

    #[tokio::test]
    async fn runs_independent_dag_tasks_concurrently() {
        let dir = tempdir().unwrap();
        let mut ctx = context(dir.path());
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        ctx.subagent = Some(Arc::new(ConcurrentRunner {
            active: active.clone(),
            max_active: max_active.clone(),
        }));

        let output = TaskDagTool
            .execute(
                json!({
                    "tasks": [
                        {"id": "a", "description": "task a", "prompt": "do a"},
                        {"id": "b", "description": "task b", "prompt": "do b"},
                        {"id": "c", "description": "task c", "prompt": "do c", "depends_on": ["a", "b"]}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.content.contains("3 succeeded"));
        assert!(output.content.contains("[a] ✓"));
        assert!(output.content.contains("[c] ✓"));
        assert!(max_active.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn rejects_dag_cycles_and_missing_dependencies() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let cycle = TaskDagTool
            .execute(
                json!({
                    "tasks": [
                        {"id": "a", "description": "a", "prompt": "a", "depends_on": ["b"]},
                        {"id": "b", "description": "b", "prompt": "b", "depends_on": ["a"]}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(cycle.to_string().contains("cycle"));

        let missing = TaskDagTool
            .execute(
                json!({"tasks": [{"id": "a", "description": "a", "prompt": "a", "depends_on": ["x"]}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("unknown task"));
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

#[derive(Debug, Clone, Deserialize)]
struct DagTaskSpec {
    id: String,
    description: String,
    prompt: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

pub struct TaskDagTool;

#[async_trait]
impl Tool for TaskDagTool {
    fn name(&self) -> &'static str {
        "task_dag"
    }

    fn description(&self) -> &'static str {
        "Run several read-only research subagents in parallel with dependency ordering. \
         Tasks with no remaining dependencies run concurrently; a failed dependency skips \
         its dependents. Use this to explore independent areas of the codebase quickly."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "tasks": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "description": "DAG nodes. Independent nodes run concurrently.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Unique task id."},
                            "description": {"type": "string"},
                            "prompt": {"type": "string", "description": "Self-contained task for the subagent."},
                            "depends_on": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Task ids that must finish successfully first."
                            }
                        },
                        "required": ["id", "description", "prompt"],
                        "additionalProperties": false
                    }
                }
            }),
            &["tasks"],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let tasks: Vec<DagTaskSpec> = serde_json::from_value(
            args.get("tasks")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing required array argument 'tasks'"))?,
        )
        .context("invalid task DAG")?;
        validate_dag(&tasks)?;
        let runner = ctx
            .subagent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("subagents are not available in this context"))?
            .clone();

        let mut pending: std::collections::HashMap<String, DagTaskSpec> = tasks
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        let mut results: std::collections::HashMap<String, Result<SubagentOutput, String>> =
            std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();

        while !pending.is_empty() {
            let mut ready = Vec::new();
            let mut blocked = Vec::new();
            for task in pending.values() {
                let mut missing = false;
                let mut failed = false;
                for dependency in &task.depends_on {
                    match results.get(dependency) {
                        None => missing = true,
                        Some(Err(_)) => failed = true,
                        Some(Ok(_)) => {}
                    }
                }
                if failed {
                    blocked.push(task.id.clone());
                } else if !missing {
                    ready.push(task.clone());
                }
            }
            if ready.is_empty() && blocked.is_empty() {
                anyhow::bail!("task DAG contains a cycle");
            }
            for id in blocked {
                pending.remove(&id);
                results.insert(
                    id.clone(),
                    Err("dependency failed or was skipped".to_string()),
                );
                order.push(id);
            }
            if ready.is_empty() {
                continue;
            }
            ready.sort_by(|left, right| left.id.cmp(&right.id));
            let futures = ready.iter().map(|task| {
                let runner = runner.clone();
                let request = SubagentRequest {
                    description: task.description.clone(),
                    prompt: task.prompt.clone(),
                };
                async move {
                    let result = runner.run(request).await.map_err(|err| format!("{err:#}"));
                    (task.id.clone(), result)
                }
            });
            let outputs = futures_util::future::join_all(futures).await;
            for (id, result) in outputs {
                pending.remove(&id);
                results.insert(id.clone(), result);
                order.push(id);
            }
        }

        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut rendered = String::new();
        for id in order {
            match results.get(&id) {
                Some(Ok(output)) => {
                    succeeded += 1;
                    rendered.push_str(&format!(
                        "[{id}] ✓ ({} steps, {} tool calls) {}\n",
                        output.steps,
                        output.tool_calls,
                        crate::util::text::truncate_middle(
                            &output.summary.trim().replace('\n', " "),
                            800
                        )
                    ));
                }
                Some(Err(error)) => {
                    failed += 1;
                    rendered.push_str(&format!("[{id}] ✗ {error}\n"));
                }
                None => {}
            }
        }
        Ok(ToolOutput::text(format!(
            "Task DAG finished: {succeeded} succeeded, {failed} failed/skipped.\n{rendered}"
        )))
    }
}

fn validate_dag(tasks: &[DagTaskSpec]) -> Result<()> {
    if tasks.is_empty() {
        anyhow::bail!("task DAG must contain at least one task");
    }
    if tasks.len() > 8 {
        anyhow::bail!("task DAG supports at most 8 tasks");
    }
    let mut ids = std::collections::HashSet::new();
    for task in tasks {
        if task.id.trim().is_empty() {
            anyhow::bail!("task id must not be empty");
        }
        if !ids.insert(task.id.clone()) {
            anyhow::bail!("duplicate task id '{}'", task.id);
        }
        if task.description.trim().is_empty() || task.prompt.trim().is_empty() {
            anyhow::bail!("task '{}' has an empty description or prompt", task.id);
        }
    }
    for task in tasks {
        for dependency in &task.depends_on {
            if dependency == &task.id {
                anyhow::bail!("task '{}' depends on itself", task.id);
            }
            if !ids.contains(dependency) {
                anyhow::bail!(
                    "task '{}' depends on unknown task '{}'",
                    task.id,
                    dependency
                );
            }
        }
    }

    // Kahn's algorithm catches cycles before any subagent runs.
    let mut indegree: std::collections::HashMap<&str, usize> = tasks
        .iter()
        .map(|task| (task.id.as_str(), task.depends_on.len()))
        .collect();
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0usize;
    while let Some(id) = ready.pop() {
        visited += 1;
        for task in tasks
            .iter()
            .filter(|task| task.depends_on.iter().any(|dep| dep == id))
        {
            if let Some(degree) = indegree.get_mut(task.id.as_str()) {
                *degree -= 1;
                if *degree == 0 {
                    ready.push(task.id.as_str());
                }
            }
        }
    }
    if visited != tasks.len() {
        anyhow::bail!("task DAG contains a cycle");
    }
    Ok(())
}
