//! Tool abstraction and registry.
//!
//! Every tool receives a [`ToolContext`] which enforces the path sandbox and
//! supplies limits (output size, timeouts).

pub mod fs;
pub mod git;
mod patch;
pub mod process;
pub mod search;
pub mod shell;

use crate::config::ToolsConfig;
use crate::util::path::{display_path, resolve_path};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::debug;

/// Shared, sandboxed execution context for tools.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// `None` disables path sandboxing.
    pub sandbox_root: Option<PathBuf>,
    pub shell: String,
    pub shell_timeout: Duration,
    pub max_output_bytes: usize,
    pub max_read_lines: usize,
}

impl ToolContext {
    pub fn from_config(cwd: &Path, config: &ToolsConfig) -> Result<Self> {
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("cannot resolve working directory {}", cwd.display()))?;
        if !cwd.is_dir() {
            bail!("working directory {} is not a directory", cwd.display());
        }

        let sandbox_root = if config.path_sandbox {
            let root = match &config.sandbox_root {
                Some(root) if root.is_absolute() => root.clone(),
                Some(root) => cwd.join(root),
                None => cwd.clone(),
            };
            let root = root
                .canonicalize()
                .with_context(|| format!("cannot resolve sandbox root {}", root.display()))?;
            if !cwd.starts_with(&root) {
                bail!(
                    "working directory {} is outside sandbox_root {}",
                    cwd.display(),
                    root.display()
                );
            }
            Some(root)
        } else {
            None
        };

        Ok(Self {
            cwd,
            sandbox_root,
            shell: config.shell.clone(),
            shell_timeout: Duration::from_secs(config.shell_timeout_secs),
            max_output_bytes: config.max_output_bytes,
            max_read_lines: config.max_read_lines,
        })
    }

    /// Resolve a path, enforcing the sandbox when enabled.
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        resolve_path(&self.cwd, self.sandbox_root.as_deref(), raw)
    }

    /// Render an absolute path relative to the working directory.
    pub fn display(&self, path: &Path) -> String {
        display_path(&self.cwd, path)
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    /// Tools that don't mutate the workspace. Used by the Phase 2 approval gate.
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

/// All tools available to the agent.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Registry with every built-in Phase 1 tool.
    pub fn with_defaults() -> Self {
        let mut registry = Self::default();
        registry.register(fs::ReadFile);
        registry.register(fs::WriteFile);
        registry.register(fs::EditFile);
        registry.register(fs::ApplyPatch);
        registry.register(fs::ListDir);
        registry.register(shell::RunShell);
        registry.register(search::Grep);
        registry.register(search::Glob);
        registry.register(git::GitStatus);
        registry.register(git::GitDiff);
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    pub fn specs(&self) -> Vec<crate::provider::ToolSpec> {
        self.tools
            .values()
            .map(|tool| {
                crate::provider::ToolSpec::new(tool.name(), tool.description(), tool.schema())
            })
            .collect()
    }

    pub async fn execute(&self, name: &str, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let tool = self
            .get(name)
            .with_context(|| format!("unknown tool '{name}'"))?;
        if !args.is_object() {
            bail!("arguments for tool '{name}' must be a JSON object");
        }

        debug!(tool = name, args = %crate::util::text::one_line(&args.to_string(), 300), "tool start");
        let started = Instant::now();
        let result = tool.execute(args, ctx).await;
        let elapsed = started.elapsed();

        match &result {
            Ok(output) => debug!(
                tool = name,
                ms = elapsed.as_millis(),
                bytes = output.content.len(),
                "tool ok"
            ),
            Err(err) => debug!(
                tool = name,
                ms = elapsed.as_millis(),
                error = %err,
                "tool failed"
            ),
        }
        result
    }
}

// -- argument helpers -------------------------------------------------------

pub fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing required string argument '{key}'"))
}

pub fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

pub fn optional_bool(args: &Value, key: &str, default: bool) -> Result<bool> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => bail!("argument '{key}' must be a boolean, got {other}"),
    }
}

pub fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .with_context(|| format!("argument '{key}' must be a non-negative integer")),
        Some(other) => bail!("argument '{key}' must be a non-negative integer, got {other}"),
    }
}

pub fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>> {
    Ok(optional_u64(args, key)?.map(|value| value as usize))
}

pub fn schema_object(properties: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_all_phase_one_tools() {
        let registry = ToolRegistry::with_defaults();
        let names = registry.names();
        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "apply_patch",
            "list_dir",
            "run_shell",
            "grep",
            "glob",
            "git_status",
            "git_diff",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
        assert_eq!(registry.specs().len(), names.len());
    }

    #[test]
    fn argument_helpers_validate_types() {
        let args = serde_json::json!({"n": 5, "flag": true});
        assert_eq!(optional_u64(&args, "n").unwrap(), Some(5));
        assert!(optional_bool(&args, "flag", false).unwrap());
        assert!(required_str(&args, "missing").is_err());

        let bad = serde_json::json!({"n": -1});
        assert!(optional_u64(&bad, "n").is_err());
    }
}
