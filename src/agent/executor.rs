//! Shared plan execution used by both the REPL and the TUI.

use super::plan::{Plan, StepStatus};
use super::{AgentEvent, AgentSession, EventSink, TurnOptions};
use crate::event::Event;
use crate::tools::process;
use anyhow::Result;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanOutcome {
    pub done: usize,
    pub failed: usize,
    pub skipped: usize,
    /// `None` when no verification command is configured.
    pub verified: Option<bool>,
}

/// Execute the selected plan steps in order. Failures stop the run and mark
/// remaining steps skipped; the outcome is returned rather than treated as an
/// error so front-ends can show a summary.
pub async fn execute_plan(
    session: &mut AgentSession,
    plan: &mut Plan,
    selected: &[usize],
    emit: EventSink<'_>,
) -> Result<PlanOutcome> {
    let total = plan.steps.len();
    let mut outcome = PlanOutcome::default();

    for index in 0..total {
        if !selected.contains(&index) {
            if plan.steps[index].status == StepStatus::Pending {
                plan.mark(index, StepStatus::Skipped);
                publish_step(session, plan, index);
            }
            continue;
        }

        plan.mark(index, StepStatus::Running);
        publish_step(session, plan, index);
        emit(AgentEvent::Status(format!(
            "▶ step {}/{}: {}",
            index + 1,
            total,
            plan.steps[index].description
        )));

        let prompt = plan.execution_prompt(index, &session.context().cwd);
        let result = session
            .run_turn_with(
                &prompt,
                TurnOptions {
                    plan_step: Some(index + 1),
                },
                &mut *emit,
            )
            .await;

        match result {
            Ok(turn) => {
                plan.mark(index, StepStatus::Done);
                outcome.done += 1;
                publish_step(session, plan, index);
                emit(AgentEvent::Status(format!(
                    "✓ step {} done ({} steps, {} tool calls)",
                    index + 1,
                    turn.steps,
                    turn.tool_calls
                )));
            }
            Err(err) => {
                plan.mark(index, StepStatus::Failed);
                outcome.failed += 1;
                publish_step(session, plan, index);
                emit(AgentEvent::Status(format!(
                    "✗ step {} failed: {err:#}",
                    index + 1
                )));
                session.bus().publish(Event::Error(format!("{err:#}")));
                // Do not continue into later steps after a failure.
                for later in index + 1..total {
                    if plan.steps[later].status == StepStatus::Pending {
                        plan.mark(later, StepStatus::Skipped);
                        publish_step(session, plan, later);
                    }
                }
                break;
            }
        }
    }

    outcome.skipped = plan
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Skipped)
        .count();

    if outcome.done > 0
        && outcome.failed == 0
        && let Some(command) = session.verify_command().map(str::to_string)
    {
        let retries = session.verify_retries();
        emit(AgentEvent::Status(format!("▶ verifying: {command}")));
        let mut passed = false;
        for attempt in 0..=retries {
            match run_verification(session, &command).await {
                Ok(output) if output.success() => {
                    passed = true;
                    emit(AgentEvent::Status("✓ verification passed".to_string()));
                    break;
                }
                Ok(output) => {
                    let detail = verification_detail(&output);
                    if attempt < retries {
                        emit(AgentEvent::Status(format!(
                            "✗ verification failed; self-heal attempt {}/{}",
                            attempt + 1,
                            retries
                        )));
                        let prompt = format!(
                            "The verification command `{command}` failed. Output:
{detail}

                             Fix the failures in the workspace, then stop. Do not modify the plan."
                        );
                        if session
                            .run_turn_with(&prompt, TurnOptions { plan_step: Some(0) }, &mut *emit)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    } else {
                        emit(AgentEvent::Status(
                            "✗ verification failed after self-heal attempts".to_string(),
                        ));
                        session
                            .bus()
                            .publish(Event::Error(format!("verification failed: {command}")));
                    }
                }
                Err(err) => {
                    emit(AgentEvent::Status(format!(
                        "✗ verification could not run: {err:#}"
                    )));
                    break;
                }
            }
        }
        outcome.verified = Some(passed);
    }

    emit(AgentEvent::Status(plan.summary()));
    session.bus().publish(Event::Plan(plan.clone()));
    session.bus().publish(Event::PlanComplete {
        done: outcome.done,
        failed: outcome.failed,
        skipped: outcome.skipped,
    });
    Ok(outcome)
}

async fn run_verification(session: &AgentSession, command: &str) -> Result<process::ProcessOutput> {
    let ctx = session.context();
    let mut builder = tokio::process::Command::new(&ctx.shell);
    builder
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.cwd)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");
    process::run(builder, Duration::from_secs(600), ctx.max_output_bytes).await
}

fn verification_detail(output: &process::ProcessOutput) -> String {
    let mut text = String::new();
    if !output.stdout.trim().is_empty() {
        text.push_str(
            "stdout:
",
        );
        text.push_str(output.stdout.trim());
        text.push('\n');
    }
    if !output.stderr.trim().is_empty() {
        text.push_str(
            "stderr:
",
        );
        text.push_str(output.stderr.trim());
        text.push('\n');
    }
    crate::util::text::truncate_middle(&text, 6_000)
}

fn publish_step(session: &AgentSession, plan: &Plan, index: usize) {
    let Some(step) = plan.steps.get(index) else {
        return;
    };
    session.bus().publish(Event::PlanStep {
        index,
        status: step.status,
        description: step.description.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentSession;
    use crate::config::Config;
    use crate::provider::types::ProviderStream;
    use crate::provider::{ChatRequest, ChatResponse, StreamEvent};
    use crate::tools::{ToolContext, ToolRegistry};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tempfile::tempdir;

    struct IdleProvider;

    #[async_trait]
    impl crate::provider::Provider for IdleProvider {
        fn name(&self) -> &str {
            "idle"
        }

        fn base_url(&self) -> &str {
            "http://localhost"
        }

        async fn chat_stream(&self, _request: ChatRequest) -> anyhow::Result<ProviderStream> {
            let (tx, stream) = ProviderStream::channel(2);
            tx.send(Ok(StreamEvent::Finished(ChatResponse::default())))
                .await
                .unwrap();
            drop(tx);
            Ok(stream)
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn session(dir: &std::path::Path) -> AgentSession {
        let config = Config::default();
        let ctx = ToolContext::from_config(dir, &config.tools).unwrap();
        AgentSession::new(
            Arc::new(IdleProvider),
            "idle".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        )
    }

    #[tokio::test]
    async fn plan_outcome_records_verification() {
        use crate::agent::plan::{Plan, PlanStep, Risk, StepStatus};

        let dir = tempdir().unwrap();
        let plan = Plan {
            title: "Verify".to_string(),
            goal: "Do a thing".to_string(),
            steps: vec![PlanStep {
                description: "Do it".to_string(),
                files: Vec::new(),
                risk: Risk::Low,
                status: StepStatus::Pending,
            }],
            notes: None,
        };

        let mut config = Config::default();
        config.general.verify_command = Some("true".to_string());
        config.general.verify_retries = 0;
        let ctx = ToolContext::from_config(dir.path(), &config.tools).unwrap();
        let mut session = AgentSession::new(
            Arc::new(IdleProvider),
            "idle".to_string(),
            ToolRegistry::with_defaults(),
            ctx.clone(),
            &config,
        );
        let mut ok_plan = plan.clone();
        let outcome = execute_plan(&mut session, &mut ok_plan, &[0], &mut |_| {})
            .await
            .unwrap();
        assert_eq!(outcome.verified, Some(true));

        config.general.verify_command = Some("false".to_string());
        let mut failing = AgentSession::new(
            Arc::new(IdleProvider),
            "idle".to_string(),
            ToolRegistry::with_defaults(),
            ctx,
            &config,
        );
        let mut failing_plan = plan;
        let outcome = execute_plan(&mut failing, &mut failing_plan, &[0], &mut |_| {})
            .await
            .unwrap();
        assert_eq!(outcome.verified, Some(false));
    }

    #[tokio::test]
    async fn verification_commands_report_success() {
        let dir = tempdir().unwrap();
        let session = session(dir.path());
        assert!(run_verification(&session, "true").await.unwrap().success());
        assert!(!run_verification(&session, "false").await.unwrap().success());
    }
}
