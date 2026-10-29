//! Plan pane: editable checklist with selection and reordering.

use crate::agent::plan::{Plan, StepStatus};
use crate::ui::theme::Theme;
use crate::ui::util::{truncate_chars, wrapped};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Default)]
pub struct PlanPane {
    pub plan: Option<Plan>,
    pub selected: Vec<bool>,
    pub cursor: usize,
    pub scroll: u16,
    pub viewport_height: u16,
    pub reorder: bool,
    pub running: bool,
    pub review: bool,
    pub pending: bool,
    step_lines: Vec<usize>,
}

impl PlanPane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear_for_new(&mut self) {
        self.plan = None;
        self.selected.clear();
        self.cursor = 0;
        self.scroll = 0;
        self.reorder = false;
        self.running = false;
        self.review = false;
        self.pending = true;
        self.step_lines.clear();
    }

    pub fn set_plan(&mut self, plan: Plan) {
        let is_new = self
            .plan
            .as_ref()
            .map(|current| current.title != plan.title || current.goal != plan.goal)
            .unwrap_or(true);
        if is_new {
            self.selected = vec![true; plan.steps.len()];
            self.cursor = 0;
            self.scroll = 0;
            self.reorder = false;
            self.review = true;
            self.running = false;
        } else {
            self.selected.resize(plan.steps.len(), true);
            self.cursor = self.cursor.min(plan.steps.len().saturating_sub(1));
        }
        self.pending = false;
        self.plan = Some(plan);
    }

    pub fn update_step(&mut self, index: usize, status: StepStatus) {
        let Some(plan) = &mut self.plan else {
            return;
        };
        let Some(step) = plan.steps.get_mut(index) else {
            return;
        };
        step.status = status;
        if status == StepStatus::Running {
            self.running = true;
            self.review = false;
        }
    }

    pub fn finish(&mut self) {
        self.running = false;
        self.review = false;
    }

    pub fn steps_len(&self) -> usize {
        self.plan.as_ref().map(|plan| plan.steps.len()).unwrap_or(0)
    }

    pub fn selected_indices(&self) -> Vec<usize> {
        self.selected
            .iter()
            .enumerate()
            .filter_map(|(index, selected)| selected.then_some(index))
            .collect()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.steps_len() == 0 {
            self.cursor = 0;
            return;
        }
        let max = self.steps_len() - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max as isize) as usize;
    }

    pub fn toggle_selected(&mut self) {
        if let Some(selected) = self.selected.get_mut(self.cursor) {
            *selected = !*selected;
        }
    }

    pub fn select_all(&mut self) {
        self.selected.fill(true);
    }

    pub fn select_none(&mut self) {
        self.selected.fill(false);
    }

    pub fn toggle_reorder(&mut self) {
        self.reorder = !self.reorder;
    }

    pub fn move_step(&mut self, direction: isize) -> bool {
        let Some(plan) = &mut self.plan else {
            return false;
        };
        if plan.steps.is_empty() {
            return false;
        }
        let target = self.cursor as isize + direction;
        if target < 0 || target >= plan.steps.len() as isize {
            return false;
        }
        let target = target as usize;
        plan.steps.swap(self.cursor, target);
        self.selected.swap(self.cursor, target);
        self.cursor = target;
        true
    }

    pub fn delete_selected(&mut self) -> bool {
        let Some(plan) = &mut self.plan else {
            return false;
        };
        if plan.steps.len() <= 1 {
            return false;
        }
        plan.steps.remove(self.cursor);
        self.selected.remove(self.cursor);
        self.cursor = self.cursor.min(plan.steps.len().saturating_sub(1));
        true
    }

    pub fn edit_description(&mut self, index: usize, description: String) -> bool {
        let Some(plan) = &mut self.plan else {
            return false;
        };
        let Some(step) = plan.steps.get_mut(index) else {
            return false;
        };
        if description.trim().is_empty() {
            return false;
        }
        step.description = description.trim().to_string();
        true
    }

    pub fn build_lines(&mut self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let palette = theme.palette;
        let mut lines = Vec::new();
        self.step_lines.clear();

        if self.pending {
            lines.extend(wrapped(
                "Planning…",
                width,
                "",
                "",
                Style::default().fg(palette.accent),
            ));
            self.step_lines.push(0);
            return lines;
        }

        let Some(plan) = &self.plan else {
            lines.extend(wrapped(
                "No plan yet.",
                width,
                "",
                "",
                Style::default().fg(palette.fg),
            ));
            lines.extend(wrapped(
                "Type /plan <task> in the chat, or open the command palette (Ctrl+K) and choose \"Plan a task\".",
                width,
                "",
                "",
                theme.dim(),
            ));
            return lines;
        };

        lines.push(Line::from(vec![
            Span::styled("Plan: ", theme.dim()),
            Span::styled(
                truncate_chars(&plan.title, width.saturating_sub(6) as usize),
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.extend(wrapped(
            &format!("Goal: {}", plan.goal),
            width,
            "",
            "",
            theme.dim(),
        ));
        lines.push(Line::from(""));

        for (index, step) in plan.steps.iter().enumerate() {
            self.step_lines.push(lines.len());
            let icon = match step.status {
                StepStatus::Pending => {
                    if self.selected.get(index).copied().unwrap_or(true) {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                }
                status => status.icon(),
            };
            let icon_color = match step.status {
                StepStatus::Done => palette.success,
                StepStatus::Failed => palette.error,
                StepStatus::Running => palette.warning,
                StepStatus::Skipped => palette.dim,
                StepStatus::Pending => palette.accent,
            };
            let selected = index == self.cursor;
            let base = if selected {
                Style::default().bg(palette.selection_bg)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(format!("{:>2}. ", index + 1), base.fg(palette.dim)),
                Span::styled(format!("{icon} "), base.fg(icon_color)),
                Span::styled(
                    truncate_chars(&step.description, width.saturating_sub(9) as usize),
                    base.fg(palette.fg),
                ),
            ];
            if !step.files.is_empty() {
                spans.push(Span::styled(
                    format!("  · {}", step.files.join(", ")),
                    base.fg(palette.dim),
                ));
            }
            lines.push(Line::from(spans));
        }

        if let Some(notes) = &plan.notes {
            lines.push(Line::from(""));
            lines.extend(wrapped(
                &format!("Notes: {notes}"),
                width,
                "",
                "",
                theme.dim(),
            ));
        }

        lines
    }

    /// Select a step from a mouse click row (relative to the content area).
    pub fn select_row(&mut self, row: u16) {
        let target = self.scroll as usize + row as usize;
        if let Some((index, _)) = self
            .step_lines
            .iter()
            .enumerate()
            .rev()
            .find(|(_, line)| **line <= target)
        {
            self.cursor = index.min(self.steps_len().saturating_sub(1));
        }
    }

    pub fn ensure_cursor_visible(&mut self, viewport_height: u16) {
        if self.step_lines.is_empty() {
            self.scroll = 0;
            return;
        }
        let line = self
            .step_lines
            .get(self.cursor)
            .copied()
            .unwrap_or(0)
            .min(u16::MAX as usize) as u16;
        let height = viewport_height.max(1);
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + height {
            self.scroll = line + 1 - height;
        }
    }

    pub fn hint(&self, theme: &Theme) -> Line<'static> {
        let text = if self.pending {
            "waiting for the planner…"
        } else if self.running {
            "plan running… (Esc/Chat to watch)"
        } else if self.reorder {
            "REORDER: j/k move step · Enter/Esc done"
        } else if self.review {
            "space approve · e edit · r reorder · d delete · Enter execute · a/n all/none"
        } else {
            "idle · /plan <task> to create another plan"
        };
        Line::from(Span::styled(
            truncate_chars(text, 200),
            Style::default().fg(theme.palette.dim),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{PlanStep, Risk};

    fn plan() -> Plan {
        Plan {
            title: "Test plan".to_string(),
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
    fn new_plan_selects_everything() {
        let mut pane = PlanPane::new();
        pane.set_plan(plan());
        assert_eq!(pane.selected_indices(), vec![0, 1, 2]);
        assert!(pane.review);
        assert!(!pane.pending);
    }

    #[test]
    fn toggles_and_moves_steps() {
        let mut pane = PlanPane::new();
        pane.set_plan(plan());
        pane.move_cursor(1);
        pane.toggle_selected();
        assert_eq!(pane.selected_indices(), vec![0, 2]);

        pane.toggle_reorder();
        assert!(pane.move_step(-1));
        assert_eq!(pane.plan.as_ref().unwrap().steps[0].description, "two");
        assert_eq!(pane.cursor, 0);

        assert!(pane.delete_selected());
        assert_eq!(pane.steps_len(), 2);
    }

    #[test]
    fn update_status_tracks_running() {
        let mut pane = PlanPane::new();
        pane.set_plan(plan());
        pane.update_step(0, StepStatus::Running);
        assert!(pane.running);
        assert!(!pane.review);
        pane.finish();
        assert!(!pane.running);
    }
}
