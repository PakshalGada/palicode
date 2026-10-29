//! Shared plan execution used by both the REPL and the TUI.

use super::plan::{Plan, StepStatus};
use super::{AgentEvent, AgentSession, EventSink, TurnOptions};
use crate::event::Event;
use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanOutcome {
    pub done: usize,
    pub failed: usize,
    pub skipped: usize,
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
    emit(AgentEvent::Status(plan.summary()));
    session.bus().publish(Event::Plan(plan.clone()));
    session.bus().publish(Event::PlanComplete {
        done: outcome.done,
        failed: outcome.failed,
        skipped: outcome.skipped,
    });
    Ok(outcome)
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
