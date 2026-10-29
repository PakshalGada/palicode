//! Plan review, approval, and execution UI.

use crate::agent::AgentSession;
use crate::agent::plan::Plan;
use crate::cli::input::SharedInput;
use crate::cli::print::Printer;
use crate::config::ApprovalMode;
use crate::event::Event;
use anyhow::{Result, bail};
use std::io::{IsTerminal, Write};

/// Ask the provider for a plan and publish it on the event bus.
pub async fn generate_plan(
    session: &AgentSession,
    task: &str,
    printer: &mut Printer,
) -> Result<Plan> {
    printer.info("planning...");
    let tools = session.registry().names();
    let plan = match session
        .planner()
        .create(task, &session.context().cwd, &tools)
        .await
    {
        Ok(plan) => plan,
        Err(err) => {
            session
                .bus()
                .publish(Event::Error(format!("planning failed: {err:#}")));
            return Err(err);
        }
    };
    session.bus().publish(Event::Plan(plan.clone()));
    Ok(plan)
}

/// Interactive `/plan` flow: review, edit, select, approve, execute.
pub async fn interactive(
    session: &mut AgentSession,
    input: &SharedInput,
    task: &str,
    printer: &mut Printer,
) -> Result<()> {
    let plan = generate_plan(session, task, printer).await?;
    let mut editor = PlanEditor::new(plan);
    printer.info(&editor.plan.render(Some(&editor.selected)));

    if session.approval_mode() == ApprovalMode::FullAuto {
        printer.info("full-auto: executing the plan without review");
        let selected: Vec<usize> = (0..editor.plan.steps.len()).collect();
        let outcome = crate::agent::executor::execute_plan(
            session,
            &mut editor.plan,
            &selected,
            &mut |event| printer.event(event),
        )
        .await?;
        if outcome.failed > 0 {
            printer.error(&format!("{} plan step(s) failed", outcome.failed));
        }
        if outcome.verified == Some(false) {
            printer.error("verification failed after self-heal attempts");
        }
        return Ok(());
    }

    print_plan_help();
    let mut line = String::new();
    loop {
        print!("plan> ");
        let _ = std::io::stdout().flush();
        if input.read_line(&mut line).await? == 0 {
            println!();
            printer.info("plan cancelled");
            return Ok(());
        }
        if !std::io::stdin().is_terminal() {
            println!();
        }
        match editor.handle(&line) {
            Ok(ApprovalAction::Approve) => break,
            Ok(ApprovalAction::Cancel) => {
                printer.info("plan cancelled");
                return Ok(());
            }
            Ok(ApprovalAction::Updated) => {
                printer.info(&editor.plan.render(Some(&editor.selected)));
            }
            Ok(ApprovalAction::Help) => print_plan_help(),
            Ok(ApprovalAction::Noop) => {}
            Err(err) => printer.error(&format!("{err:#}")),
        }
    }

    let selected = editor.selected_indices();
    let outcome =
        crate::agent::executor::execute_plan(session, &mut editor.plan, &selected, &mut |event| {
            printer.event(event)
        })
        .await?;
    if outcome.failed > 0 {
        printer.error(&format!("{} plan step(s) failed", outcome.failed));
    }
    if outcome.verified == Some(false) {
        printer.error("verification failed after self-heal attempts");
    }
    Ok(())
}

fn print_plan_help() {
    println!(
        "  [a]ll / [n]one   toggle with 1 2 3 or 1,3   e N text   r N M   d N\n  \
         [y] execute selected   [q] cancel   [?] help"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAction {
    Noop,
    Updated,
    Approve,
    Cancel,
    Help,
}

#[derive(Debug, Clone)]
pub struct PlanEditor {
    pub plan: Plan,
    pub selected: Vec<bool>,
}

impl PlanEditor {
    pub fn new(plan: Plan) -> Self {
        let selected = vec![true; plan.steps.len()];
        Self { plan, selected }
    }

    pub fn selected_indices(&self) -> Vec<usize> {
        self.selected
            .iter()
            .enumerate()
            .filter_map(|(index, selected)| selected.then_some(index))
            .collect()
    }

    pub fn handle(&mut self, line: &str) -> Result<ApprovalAction> {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            return Ok(ApprovalAction::Noop);
        }

        match tokens[0].to_ascii_lowercase().as_str() {
            "y" | "yes" | "approve" | "run" => {
                if self.selected_indices().is_empty() {
                    bail!("select at least one step before executing");
                }
                Ok(ApprovalAction::Approve)
            }
            "q" | "quit" | "cancel" => Ok(ApprovalAction::Cancel),
            "a" | "all" => {
                self.selected.fill(true);
                Ok(ApprovalAction::Updated)
            }
            "n" | "none" => {
                self.selected.fill(false);
                Ok(ApprovalAction::Updated)
            }
            "?" | "help" => Ok(ApprovalAction::Help),
            "v" | "view" => Ok(ApprovalAction::Updated),
            "e" | "edit" => {
                if tokens.len() < 3 {
                    bail!("usage: e N new description");
                }
                let index = self.parse_index(tokens[1])?;
                let description = tokens[2..].join(" ").trim().to_string();
                if description.is_empty() {
                    bail!("step description must not be empty");
                }
                self.plan.steps[index].description = description;
                Ok(ApprovalAction::Updated)
            }
            "r" | "move" => {
                if tokens.len() < 3 {
                    bail!("usage: r FROM TO (1-based)");
                }
                let from = self.parse_index(tokens[1])?;
                let to = self.parse_index(tokens[2])?;
                self.move_step(from, to);
                Ok(ApprovalAction::Updated)
            }
            "d" | "delete" => {
                if tokens.len() < 2 {
                    bail!("usage: d N");
                }
                let index = self.parse_index(tokens[1])?;
                self.delete_step(index)?;
                Ok(ApprovalAction::Updated)
            }
            _ => {
                let indices = parse_number_list(&tokens)?;
                if indices.is_empty() {
                    bail!("unknown plan command '{}' (try ?)", tokens[0]);
                }
                for index in indices {
                    let index = self.parse_index(&(index + 1).to_string())?;
                    self.selected[index] = !self.selected[index];
                }
                Ok(ApprovalAction::Updated)
            }
        }
    }

    fn parse_index(&self, token: &str) -> Result<usize> {
        let number: usize = token
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("'{token}' is not a step number"))?;
        if number == 0 || number > self.plan.steps.len() {
            bail!(
                "step {number} is out of range (1..={})",
                self.plan.steps.len()
            );
        }
        Ok(number - 1)
    }

    fn move_step(&mut self, from: usize, to: usize) {
        if from == to {
            return;
        }
        let step = self.plan.steps.remove(from);
        let selected = self.selected.remove(from);
        let target = to.min(self.plan.steps.len());
        self.plan.steps.insert(target, step);
        self.selected.insert(target, selected);
    }

    fn delete_step(&mut self, index: usize) -> Result<()> {
        if self.plan.steps.len() <= 1 {
            bail!("a plan must keep at least one step");
        }
        self.plan.steps.remove(index);
        self.selected.remove(index);
        Ok(())
    }
}

fn parse_number_list(tokens: &[&str]) -> Result<Vec<usize>> {
    let mut numbers = Vec::new();
    for token in tokens {
        for part in token.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let number: usize = part
                .parse()
                .map_err(|_| anyhow::anyhow!("'{part}' is not a step number"))?;
            if number == 0 {
                bail!("step numbers are 1-based");
            }
            numbers.push(number - 1);
        }
    }
    Ok(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{PlanStep, Risk, StepStatus};

    fn plan() -> Plan {
        Plan {
            title: "Test".to_string(),
            goal: "Do the task".to_string(),
            steps: ["one", "two", "three"]
                .into_iter()
                .map(|description| PlanStep {
                    description: description.to_string(),
                    files: Vec::new(),
                    risk: Risk::Medium,
                    status: StepStatus::Pending,
                })
                .collect(),
            notes: None,
        }
    }

    #[test]
    fn toggles_and_selects_steps() {
        let mut editor = PlanEditor::new(plan());
        assert_eq!(editor.selected_indices(), vec![0, 1, 2]);

        editor.handle("2").unwrap();
        assert_eq!(editor.selected_indices(), vec![0, 2]);

        editor.handle("1,3").unwrap();
        assert!(editor.selected_indices().is_empty());

        editor.handle("a").unwrap();
        assert_eq!(editor.selected_indices(), vec![0, 1, 2]);

        editor.handle("n").unwrap();
        assert!(editor.selected_indices().is_empty());
        assert!(editor.handle("y").is_err());
    }

    #[test]
    fn edits_reorders_and_deletes_steps() {
        let mut editor = PlanEditor::new(plan());
        editor.handle("e 2 better description").unwrap();
        assert_eq!(editor.plan.steps[1].description, "better description");

        editor.handle("r 1 3").unwrap();
        assert_eq!(editor.plan.steps[2].description, "one");
        assert_eq!(editor.plan.steps[0].description, "better description");
        assert_eq!(editor.selected.len(), 3);

        editor.handle("d 1").unwrap();
        assert_eq!(editor.plan.steps.len(), 2);
        assert_eq!(editor.selected.len(), 2);
    }

    #[test]
    fn refuses_to_delete_the_last_step() {
        let mut editor = PlanEditor::new(plan());
        editor.handle("d 1").unwrap();
        editor.handle("d 1").unwrap();
        assert!(editor.handle("d 1").is_err());
    }

    #[test]
    fn rejects_out_of_range_and_unknown_commands() {
        let mut editor = PlanEditor::new(plan());
        assert!(editor.handle("9").is_err());
        assert!(editor.handle("frobnicate").is_err());
    }
}
