//! Shell command execution tool.

use super::{
    Tool, ToolContext, ToolOutput, optional_str, optional_u64, required_str, schema_object,
};
use crate::util::text::truncate_middle;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }

    fn description(&self) -> &'static str {
        "Run a non-interactive shell command in the workspace and return stdout, stderr and the exit code. \
         Use it for builds, tests, git, and file inspection. Commands have no TTY; avoid interactive programs. \
         A timeout kills the whole process group."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "command": {
                    "type": "string",
                    "description": "Shell command line, executed with the configured shell (default /bin/sh -c)."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory, relative to the workspace. Defaults to the workspace root."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "description": "Timeout in seconds. Defaults to tools.shell_timeout_secs (120)."
                }
            }),
            &["command"],
        )
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let command = required_str(&args, "command")?.trim().to_string();
        if command.is_empty() {
            bail!("command must not be empty");
        }

        let cwd = match optional_str(&args, "cwd")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(path) => ctx.resolve(path)?,
            None => ctx.cwd.clone(),
        };
        if !cwd.is_dir() {
            bail!("working directory {} does not exist", ctx.display(&cwd));
        }

        let timeout_secs = optional_u64(&args, "timeout_secs")?
            .unwrap_or(ctx.shell_timeout.as_secs())
            .clamp(1, 600);

        let mut command_builder = Command::new(&ctx.shell);
        command_builder
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("GIT_TERMINAL_PROMPT", "0");

        let output = super::process::run(
            command_builder,
            Duration::from_secs(timeout_secs),
            ctx.max_output_bytes,
        )
        .await
        .with_context(|| format!("cannot run command: {command}"))?;

        let status_line = if output.timed_out {
            format!("timed out after {timeout_secs}s (process group killed)")
        } else if let Some(code) = output.exit_code() {
            format!("exit code: {code}")
        } else {
            "terminated by signal".to_string()
        };

        let stdout = if output.stdout.is_empty() {
            "(empty)".to_string()
        } else {
            output.stdout.trim_end_matches('\n').to_string()
        };
        let stderr = if output.stderr.is_empty() {
            "(empty)".to_string()
        } else {
            output.stderr.trim_end_matches('\n').to_string()
        };

        let mut rendered = format!(
            "status: {status_line}\ncwd: {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            ctx.display(&cwd)
        );
        if output.stdout_truncated {
            rendered.push_str("\n(stdout truncated)");
        }
        if output.stderr_truncated {
            rendered.push_str("\n(stderr truncated)");
        }

        Ok(ToolOutput::text(truncate_middle(
            &rendered,
            ctx.max_output_bytes,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;
    use serde_json::json;
    use tempfile::tempdir;

    fn context(root: &std::path::Path) -> ToolContext {
        ToolContext::from_config(root, &ToolsConfig::default()).unwrap()
    }

    #[tokio::test]
    async fn runs_commands_in_workspace() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let output = RunShell
            .execute(json!({"command": "pwd && printf hi"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("exit code: 0"));
        assert!(output.content.contains("hi"));
        assert!(output.content.contains("--- stdout ---"));
    }

    #[tokio::test]
    async fn reports_nonzero_exit() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let output = RunShell
            .execute(json!({"command": "echo bad >&2; exit 7"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("exit code: 7"));
        assert!(output.content.contains("bad"));
    }

    #[tokio::test]
    async fn refuses_path_outside_sandbox() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let err = RunShell
            .execute(json!({"command": "pwd", "cwd": ".."}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside the sandbox"));
    }
}
