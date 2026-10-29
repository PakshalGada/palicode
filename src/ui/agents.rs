//! Subagent sidebar: live status reported through the event bus.

use crate::event::SubagentState;
use crate::ui::theme::Theme;
use crate::ui::util::truncate_chars;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct SubagentEntry {
    pub description: String,
    pub state: SubagentState,
    pub message: Option<String>,
    pub started: Instant,
}

#[derive(Debug, Default)]
pub struct AgentPane {
    pub entries: Vec<SubagentEntry>,
    pub cursor: usize,
    pub scroll: u16,
    pub viewport_height: u16,
    entry_lines: Vec<usize>,
}

impl AgentPane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, description: String, state: SubagentState, message: Option<String>) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.description == description)
        {
            entry.state = state;
            if message.is_some() {
                entry.message = message;
            }
        } else {
            self.entries.push(SubagentEntry {
                description,
                state,
                message,
                started: Instant::now(),
            });
            self.cursor = self.entries.len().saturating_sub(1);
        }
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.entries.len() - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max as isize) as usize;
    }

    pub fn build_lines(&mut self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let palette = theme.palette;
        let mut lines = Vec::new();
        self.entry_lines.clear();

        if self.entries.is_empty() {
            lines.push(Line::from(Span::styled("No subagents yet.", theme.dim())));
            lines.push(Line::from(Span::styled(
                "The model can call task for read-only research.",
                theme.dim(),
            )));
            return lines;
        }

        for (index, entry) in self.entries.iter().enumerate() {
            self.entry_lines.push(lines.len());
            let (icon, color) = match entry.state {
                SubagentState::Started => ("●", palette.warning),
                SubagentState::Finished => ("✓", palette.success),
                SubagentState::Failed => ("✗", palette.error),
            };
            let elapsed = entry.started.elapsed().as_secs();
            let suffix = match entry.state {
                SubagentState::Started => format!("  {elapsed}s"),
                _ => String::new(),
            };
            let selected = index == self.cursor;
            let base = if selected {
                Style::default().bg(palette.selection_bg)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{icon} "), base.fg(color)),
                Span::styled(
                    truncate_chars(&entry.description, width.saturating_sub(4) as usize),
                    base.fg(palette.fg),
                ),
                Span::styled(suffix, base.fg(palette.dim)),
            ]));
            if let Some(message) = &entry.message {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        truncate_chars(message, width.saturating_sub(2) as usize)
                    ),
                    Style::default().fg(palette.dim),
                )));
            }
        }
        lines
    }

    pub fn ensure_cursor_visible(&mut self, viewport_height: u16) {
        if self.entry_lines.is_empty() {
            self.scroll = 0;
            return;
        }
        let line = self
            .entry_lines
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
        let running = self
            .entries
            .iter()
            .filter(|entry| entry.state == SubagentState::Started)
            .count();
        let text = if running > 0 {
            format!("{running} running · j/k select")
        } else {
            format!("{} total · j/k select", self.entries.len())
        };
        Line::from(Span::styled(
            truncate_chars(&text, 60),
            Style::default()
                .fg(theme.palette.dim)
                .add_modifier(Modifier::DIM),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_existing_subagents_by_description() {
        let mut pane = AgentPane::new();
        pane.update("research".to_string(), SubagentState::Started, None);
        pane.update(
            "research".to_string(),
            SubagentState::Finished,
            Some("done".to_string()),
        );
        assert_eq!(pane.entries.len(), 1);
        assert_eq!(pane.entries[0].state, SubagentState::Finished);
        assert_eq!(pane.entries[0].message.as_deref(), Some("done"));
    }
}
