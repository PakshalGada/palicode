//! Structured plans and the LLM planner.
//!
//! The planner asks the model for a single JSON object. Parsing is deliberately
//! forgiving about prose/fences but strict about step quality, and retries once
//! with a repair instruction.

use crate::provider::{ChatRequest, Message, Provider};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tracing::debug;

const MAX_PLAN_STEPS: usize = 12;
const PLANNER_MAX_TOKENS: u32 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn icon(self) -> &'static str {
        match self {
            StepStatus::Pending => "[ ]",
            StepStatus::Running => "[>]",
            StepStatus::Done => "[+]",
            StepStatus::Failed => "[!]",
            StepStatus::Skipped => "[-]",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    #[default]
    Medium,
    High,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub description: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub status: StepStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub title: String,
    /// The original user task.
    pub goal: String,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Plan {
    pub fn render(&self, selected: Option<&[bool]>) -> String {
        let mut output = String::new();
        output.push_str(&format!("Plan: {}\n", self.title));
        output.push_str(&format!("Goal: {}\n", self.goal));
        for (index, step) in self.steps.iter().enumerate() {
            let icon = match step.status {
                StepStatus::Pending => match selected.and_then(|selected| selected.get(index)) {
                    Some(true) => "[x]",
                    _ => "[ ]",
                },
                status => status.icon(),
            };
            output.push_str(&format!(
                "{:>3}. {} {}\n",
                index + 1,
                icon,
                step.description
            ));
            if !step.files.is_empty() || step.risk != Risk::Medium {
                let files = if step.files.is_empty() {
                    String::new()
                } else {
                    format!("files: {}", step.files.join(", "))
                };
                let risk = if step.risk == Risk::Medium {
                    String::new()
                } else {
                    format!("risk: {}", step.risk.label())
                };
                let detail = [files, risk]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("  ·  ");
                if !detail.is_empty() {
                    output.push_str(&format!("      {detail}\n"));
                }
            }
        }
        if let Some(notes) = &self.notes {
            output.push_str(&format!("Notes: {notes}\n"));
        }
        output
    }

    pub fn mark(&mut self, index: usize, status: StepStatus) {
        if let Some(step) = self.steps.get_mut(index) {
            step.status = status;
        }
    }

    /// Prompt for executing exactly one approved step.
    pub fn execution_prompt(&self, index: usize, cwd: &Path) -> String {
        let Some(step) = self.steps.get(index) else {
            return String::new();
        };
        let files = if step.files.is_empty() {
            "(not specified; locate what you need)".to_string()
        } else {
            step.files.join(", ")
        };
        format!(
            "Execute step {number} of {total} of an approved plan in {cwd}.\n\n\
             Goal: {goal}\n\n\
             Plan:\n{plan}\n\
             Current step: {description}\n\
             Files likely involved: {files}\n\n\
             This is a controlled step execution. Complete ONLY the current step; other steps are\n\
             handled separately by the controller.\n\n\
             Rules:\n\
             - Work only on this step; do not start or perform later steps.\n\
             - Inspect files before editing. Verify with tests/builds where practical.\n\
             - If a tool fails, reflect on the error, adjust, and continue.\n\
             - Finish with a short report: what changed and how it was verified.",
            number = index + 1,
            total = self.steps.len(),
            cwd = cwd.display(),
            goal = self.goal,
            plan = self.render(None),
            description = step.description,
            files = files,
        )
    }

    pub fn summary(&self) -> String {
        let done = self
            .steps
            .iter()
            .filter(|step| step.status == StepStatus::Done)
            .count();
        let failed = self
            .steps
            .iter()
            .filter(|step| step.status == StepStatus::Failed)
            .count();
        let skipped = self
            .steps
            .iter()
            .filter(|step| step.status == StepStatus::Skipped)
            .count();
        format!(
            "Plan finished: {done} done, {failed} failed, {skipped} skipped, {} total.",
            self.steps.len()
        )
    }
}

/// Generates plans through the active provider.
pub struct Planner {
    provider: Arc<dyn Provider>,
    model: String,
    max_steps: usize,
}

impl Planner {
    pub fn new(provider: Arc<dyn Provider>, model: String, max_steps: usize) -> Self {
        Self {
            provider,
            model,
            max_steps: max_steps.clamp(2, MAX_PLAN_STEPS),
        }
    }

    pub async fn create(&self, task: &str, cwd: &Path, tools: &[&str]) -> Result<Plan> {
        let system = "You are palicode's planning engine. Turn a coding task into a short, \
                      concrete, ordered plan.\n\
                      Return ONLY one JSON object (no markdown fences, no prose) with these keys:\n\
                      - \"title\": a short plan title (string)\n\
                      - \"steps\": an array of 2-8 objects. Each object has:\n\
                        - \"description\": one concrete, verifiable action (string, never a placeholder)\n\
                        - \"files\": array of workspace-relative file paths likely involved (may be empty)\n\
                        - \"risk\": \"low\", \"medium\", or \"high\"\n\
                      - \"notes\": optional string with caveats\n\
                      Start with investigation, then make the smallest safe edits, then verify with tests/builds.\n\
                      Do not include code blocks or tool call syntax. Do not repeat the plan in prose.";

        let user = format!(
            "Workspace: {}\nAvailable tools: {}\nMaximum steps: {}\n\nTask:\n{}",
            cwd.display(),
            tools.join(", "),
            self.max_steps,
            task.trim()
        );

        let mut messages = vec![Message::system(system), Message::user(user)];
        let mut last_error = String::new();

        for attempt in 0..2 {
            let response = self
                .provider
                .chat(ChatRequest {
                    model: self.model.clone(),
                    messages: messages.clone(),
                    tools: Vec::new(),
                    temperature: Some(0.2),
                    max_tokens: Some(PLANNER_MAX_TOKENS),
                })
                .await
                .context("planner request failed")?;

            let content = response.content;
            match parse_plan_response(&content, task, self.max_steps) {
                Ok(plan) => {
                    debug!(
                        title = %plan.title,
                        steps = plan.steps.len(),
                        "plan created"
                    );
                    return Ok(plan);
                }
                Err(err) => {
                    last_error = format!("{err:#}");
                    if attempt == 0 {
                        messages.push(Message::assistant(Some(content), Vec::new()));
                        messages.push(Message::user(
                            "That response was not a valid plan. Return ONLY the corrected JSON \
                             object with keys title, steps, notes. Every step needs a non-empty, \
                             concrete description. No prose or fences."
                                .to_string(),
                        ));
                    }
                }
            }
        }

        bail!("planner could not produce a valid plan: {last_error}")
    }
}

/// Parse a model response into a plan. Scans all JSON objects and prefers the
/// one with the most usable steps (models sometimes echo schema examples).
pub fn parse_plan_response(text: &str, goal: &str, max_steps: usize) -> Result<Plan> {
    let max_steps = max_steps.clamp(1, MAX_PLAN_STEPS);
    let candidates = json_objects(text);
    if candidates.is_empty() {
        bail!("no JSON object found in planner response");
    }

    let mut best: Option<Plan> = None;
    let mut last_error = String::new();
    for candidate in candidates {
        match serde_json::from_str::<RawPlan>(candidate) {
            Ok(raw) => match build_plan(raw, goal, max_steps) {
                Ok(plan) => {
                    let better = best
                        .as_ref()
                        .map(|current| plan.steps.len() > current.steps.len())
                        .unwrap_or(true);
                    if better {
                        best = Some(plan);
                    }
                }
                Err(err) => last_error = format!("{err:#}"),
            },
            Err(err) => last_error = err.to_string(),
        }
    }

    best.with_context(|| {
        if last_error.is_empty() {
            "planner response contained no usable plan".to_string()
        } else {
            format!("planner response contained no usable plan: {last_error}")
        }
    })
}

#[derive(Debug, Deserialize)]
struct RawPlan {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    steps: Vec<RawStep>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStep {
    #[serde(default)]
    description: String,
    #[serde(default)]
    files: Vec<Value>,
    #[serde(default)]
    risk: Option<String>,
}

fn build_plan(raw: RawPlan, goal: &str, max_steps: usize) -> Result<Plan> {
    let mut steps = Vec::new();
    let total = raw.steps.len();
    for step in raw.steps.into_iter().take(max_steps) {
        let description = step.description.trim().to_string();
        if is_placeholder(&description) {
            continue;
        }
        let mut files: Vec<String> = step
            .files
            .iter()
            .filter_map(|file| file.as_str())
            .map(str::trim)
            .filter(|file| !file.is_empty())
            .map(str::to_string)
            .collect();
        files.sort();
        files.dedup();
        files.truncate(20);
        steps.push(PlanStep {
            description,
            files,
            risk: parse_risk(step.risk.as_deref()),
            status: StepStatus::Pending,
        });
    }

    if steps.is_empty() {
        bail!("plan has no usable steps");
    }

    let mut notes = raw
        .notes
        .map(|notes| notes.trim().to_string())
        .filter(|notes| !notes.is_empty());
    if total > max_steps {
        let note = format!("Plan truncated to {max_steps} steps.");
        notes = Some(match notes {
            Some(existing) => format!("{existing} {note}"),
            None => note,
        });
    }

    let title = raw
        .title
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| fallback_title(goal));

    Ok(Plan {
        title,
        goal: goal.trim().to_string(),
        steps,
        notes,
    })
}

fn is_placeholder(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    description.is_empty()
        || description == "..."
        || description == "…"
        || (description.starts_with('<') && description.ends_with('>'))
        || lower == "description"
        || lower.contains("short description")
        || lower.starts_with("brief description")
}

fn parse_risk(value: Option<&str>) -> Risk {
    match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
        "low" => Risk::Low,
        "high" => Risk::High,
        _ => Risk::Medium,
    }
}

fn fallback_title(goal: &str) -> String {
    let first = goal.lines().next().unwrap_or("Plan").trim();
    let title: String = first.chars().take(80).collect();
    if title.is_empty() {
        "Plan".to_string()
    } else {
        title
    }
}

/// Extract every balanced `{...}` object, ignoring braces in strings.
fn json_objects(text: &str) -> Vec<&str> {
    let mut objects = Vec::new();
    let mut search_from = 0;
    while let Some(relative) = text[search_from..].find('{') {
        let start = search_from + relative;
        let Some(end) = matching_brace(text, start) else {
            break;
        };
        objects.push(&text[start..end]);
        search_from = end;
    }
    objects
}

fn matching_brace(text: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOAL: &str = "Add permission checks to the shell tool";

    #[test]
    fn parses_fenced_json() {
        let text = r#"Here is the plan:
```json
{"title": "Add permissions", "steps": [
  {"description": "Inspect the tool registry", "files": ["src/tools/mod.rs"], "risk": "low"},
  {"description": "Add a permission engine", "files": [], "risk": "high"}
], "notes": "Keep it safe"}
```"#;
        let plan = parse_plan_response(text, GOAL, 8).unwrap();
        assert_eq!(plan.title, "Add permissions");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].files, vec!["src/tools/mod.rs"]);
        assert_eq!(plan.steps[1].risk, Risk::High);
        assert_eq!(plan.notes.as_deref(), Some("Keep it safe"));
    }

    #[test]
    fn prefers_the_plan_with_most_steps_over_echoed_schema() {
        let text = r#"Schema example: {"title": "example", "steps": [{"description": "Example step"}]}
        Actual plan: {"title": "Real", "steps": [
          {"description": "First concrete action"},
          {"description": "Second concrete action"},
          {"description": "Third concrete action"}
        ]}"#;
        let plan = parse_plan_response(text, GOAL, 8).unwrap();
        assert_eq!(plan.title, "Real");
        assert_eq!(plan.steps.len(), 3);
    }

    #[test]
    fn rejects_placeholder_and_empty_steps() {
        let text = r#"{"title": "Bad", "steps": [{"description": "..."}, {"description": ""}]}"#;
        assert!(parse_plan_response(text, GOAL, 8).is_err());
    }

    #[test]
    fn rejects_prose_without_json() {
        assert!(parse_plan_response("I will do the task", GOAL, 8).is_err());
    }

    #[test]
    fn truncates_long_plans_and_fills_defaults() {
        let steps = (0..15)
            .map(|index| format!(r#"{{"description": "Step number {index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let text = format!(r#"{{"steps": [{steps}]}}"#);
        let plan = parse_plan_response(&text, GOAL, 5).unwrap();
        assert_eq!(plan.steps.len(), 5);
        assert_eq!(plan.steps[0].risk, Risk::Medium);
        assert_eq!(plan.title, GOAL);
        assert!(plan.notes.unwrap().contains("truncated"));
    }

    #[test]
    fn renders_status_and_execution_prompt() {
        let mut plan = Plan {
            title: "Test plan".to_string(),
            goal: GOAL.to_string(),
            steps: vec![PlanStep {
                description: "Do the thing".to_string(),
                files: vec!["src/a.rs".to_string()],
                risk: Risk::High,
                status: StepStatus::Pending,
            }],
            notes: None,
        };
        let rendered = plan.render(Some(&[true]));
        assert!(rendered.contains("[x] Do the thing"));
        assert!(rendered.contains("risk: high"));

        plan.mark(0, StepStatus::Done);
        assert!(plan.render(None).contains("[+] Do the thing"));

        let prompt = plan.execution_prompt(0, Path::new("/tmp/work"));
        assert!(prompt.contains("Current step: Do the thing"));
        assert!(prompt.contains("src/a.rs"));
        assert!(prompt.contains(GOAL));
    }
}
