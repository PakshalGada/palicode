//! Git inspection tools (read-only).

use super::{
    Tool, ToolContext, ToolOutput, optional_bool, optional_str, optional_u64, schema_object,
};
use crate::tools::process::{self, ProcessOutput};
use crate::util::text::truncate_middle;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

pub struct GitStatus;
pub struct GitDiff;

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &'static str {
        "git_status"
    }

    fn description(&self) -> &'static str {
        "Show git status in porcelain format, including the current branch."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "path": {
                    "type": "string",
                    "description": "Limit output to this path. Defaults to the whole repository."
                }
            }),
            &[],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let mut git_args = vec![
            "status".to_string(),
            "--porcelain=v1".to_string(),
            "--branch".to_string(),
        ];
        if let Some(path) = optional_str(&args, "path").filter(|path| !path.trim().is_empty()) {
            let resolved = ctx.resolve(path)?;
            git_args.push("--".to_string());
            git_args.push(resolved.display().to_string());
        }

        let output = run_git(&git_args, ctx).await?;
        ensure_success(&output, "git status")?;
        let mut text = output.stdout.trim_end().to_string();
        if text.is_empty() {
            text = "(clean working tree)".to_string();
        }
        if output.stdout_truncated {
            text.push_str("\n(status output truncated)");
        }
        Ok(ToolOutput::text(text))
    }
}

#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &'static str {
        "Show unstaged or staged git changes as a unified diff. Use stat=true for a summary."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "staged": {"type": "boolean", "description": "Show staged changes (--staged). Default false."},
                "stat": {"type": "boolean", "description": "Show a diffstat summary instead of the full diff. Default false."},
                "path": {"type": "string", "description": "Limit the diff to this path."},
                "context": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "description": "Lines of context. Default 3."
                }
            }),
            &[],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let staged = optional_bool(&args, "staged", false)?;
        let stat = optional_bool(&args, "stat", false)?;
        let context_lines = optional_u64(&args, "context")?.unwrap_or(3).min(20);

        let mut git_args = vec![
            "diff".to_string(),
            "--no-color".to_string(),
            "--no-ext-diff".to_string(),
        ];
        if staged {
            git_args.push("--staged".to_string());
        }
        if stat {
            git_args.push("--stat".to_string());
        } else {
            git_args.push(format!("-U{context_lines}"));
        }
        if let Some(path) = optional_str(&args, "path").filter(|path| !path.trim().is_empty()) {
            let resolved = ctx.resolve(path)?;
            git_args.push("--".to_string());
            git_args.push(resolved.display().to_string());
        }

        let output = run_git(&git_args, ctx).await?;
        ensure_success(&output, "git diff")?;

        let mut text = output.stdout.trim_end().to_string();
        if text.is_empty() {
            text = "(no changes)".to_string();
        } else {
            text = truncate_middle(&text, ctx.max_output_bytes);
        }
        if output.stdout_truncated {
            text.push_str("\n(diff truncated)");
        }
        Ok(ToolOutput::text(text))
    }
}

async fn run_git(args: &[String], ctx: &ToolContext) -> Result<ProcessOutput> {
    let mut command = tokio::process::Command::new("git");
    command.arg("--no-pager").arg("-C").arg(&ctx.cwd).args(args);
    process::run(command, Duration::from_secs(60), ctx.max_output_bytes)
        .await
        .context("cannot run git")
}

fn ensure_success(output: &ProcessOutput, what: &str) -> Result<()> {
    if output.timed_out {
        bail!("{what} timed out");
    }
    if !output.success() {
        let stderr = output.stderr.trim();
        let message = if stderr.is_empty() {
            format!("{what} failed with exit code {:?}", output.exit_code())
        } else {
            format!("{what} failed: {stderr}")
        };
        bail!("{message}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;
    use serde_json::json;
    use std::path::Path;
    use tempfile::tempdir;

    fn context(root: &Path) -> ToolContext {
        ToolContext::from_config(root, &ToolsConfig::default()).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn reports_status_and_diff() {
        if !process::command_exists("git") {
            return;
        }
        let dir = tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();

        let ctx = context(dir.path());
        let status = GitStatus.execute(json!({}), &ctx).await.unwrap();
        assert!(status.content.contains("a.txt"));

        let diff = GitDiff.execute(json!({}), &ctx).await.unwrap();
        assert!(diff.content.contains("+two"));
    }

    #[tokio::test]
    async fn errors_outside_a_repository() {
        if !process::command_exists("git") {
            return;
        }
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        let err = GitStatus.execute(json!({}), &ctx).await.unwrap_err();
        assert!(err.to_string().to_lowercase().contains("git"));
    }
}
