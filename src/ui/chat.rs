//! Chat transcript state and rendering helpers.

use crate::ui::theme::Theme;
use crate::ui::util::wrapped;
use crate::util::text::one_line;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone)]
pub struct ToolLogEntry {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub output: String,
    pub is_error: bool,
    pub duration_ms: Option<u128>,
    pub expanded: bool,
}

#[derive(Debug, Clone)]
pub enum ChatItem {
    User(String),
    Assistant(String),
    Tool(usize),
    Info(String),
    Error(String),
}

#[derive(Debug, Default)]
pub struct ChatState {
    pub items: Vec<ChatItem>,
    pub streaming: Option<usize>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub line_count: usize,
    pub viewport_height: u16,
}

impl ChatState {
    pub fn new() -> Self {
        Self {
            auto_scroll: true,
            ..Self::default()
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.streaming = None;
        self.scroll = 0;
        self.auto_scroll = true;
        self.line_count = 0;
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.items.push(ChatItem::User(text.into()));
        self.auto_scroll = true;
    }

    pub fn push_info(&mut self, text: impl Into<String>) {
        self.items.push(ChatItem::Info(text.into()));
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self
            .items
            .iter()
            .rev()
            .take(5)
            .any(|item| matches!(item, ChatItem::Error(previous) if *previous == text))
        {
            return;
        }
        self.items.push(ChatItem::Error(text));
    }

    pub fn push_tool(&mut self, index: usize) {
        self.items.push(ChatItem::Tool(index));
    }

    pub fn ensure_streaming(&mut self) -> usize {
        if let Some(index) = self.streaming {
            return index;
        }
        self.items.push(ChatItem::Assistant(String::new()));
        let index = self.items.len() - 1;
        self.streaming = Some(index);
        index
    }

    pub fn append_delta(&mut self, delta: &str) {
        let index = self.ensure_streaming();
        if let Some(ChatItem::Assistant(text)) = self.items.get_mut(index) {
            text.push_str(delta);
        }
    }

    pub fn end_streaming(&mut self) {
        self.streaming = None;
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.auto_scroll = false;
        let max = self
            .line_count
            .saturating_sub(self.viewport_height as usize)
            .min(u16::MAX as usize) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as u16;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
    }

    pub fn scroll_to_top(&mut self) {
        self.auto_scroll = false;
        self.scroll = 0;
    }

    pub fn build_lines(
        &mut self,
        width: u16,
        tools: &[ToolLogEntry],
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let palette = theme.palette;
        let mut lines: Vec<Line<'static>> = Vec::new();

        for (index, item) in self.items.iter().enumerate() {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }

            match item {
                ChatItem::User(text) => {
                    let style = Style::default()
                        .fg(palette.user)
                        .add_modifier(Modifier::BOLD);
                    lines.extend(wrapped(text, width, "❯ ", "  ", style));
                }
                ChatItem::Assistant(text) => {
                    let mut item_lines = wrapped(text, width, "", "", theme.text());
                    if self.streaming == Some(index)
                        && let Some(last) = item_lines.last_mut()
                    {
                        last.spans
                            .push(Span::styled("▌", Style::default().fg(palette.accent)));
                    }
                    lines.extend(item_lines);
                }
                ChatItem::Tool(tool_index) => {
                    let Some(tool) = tools.get(*tool_index) else {
                        continue;
                    };
                    let running = tool.output.is_empty();
                    let (icon, color) = if running {
                        ("▸", palette.tool)
                    } else if tool.is_error {
                        ("✗", palette.error)
                    } else {
                        ("✓", palette.success)
                    };
                    let duration = tool
                        .duration_ms
                        .map(|ms| format!(" ({ms} ms)"))
                        .unwrap_or_default();
                    let args = one_line(&tool.arguments, 120);
                    let header = format!("{icon} {}{duration} {args}", tool.name);
                    lines.extend(wrapped(&header, width, "", "", Style::default().fg(color)));

                    if running {
                        lines.extend(wrapped("running…", width, "  ", "  ", theme.dim()));
                    } else {
                        let output_lines: Vec<&str> = tool.output.lines().collect();
                        let limit = if tool.expanded {
                            output_lines.len()
                        } else {
                            output_lines.len().min(4)
                        };
                        let style = if tool.is_error {
                            Style::default().fg(palette.error)
                        } else {
                            theme.dim()
                        };
                        for line in output_lines.iter().take(limit) {
                            lines.extend(wrapped(line, width, "  ", "  ", style));
                        }
                        if !tool.expanded && output_lines.len() > limit {
                            lines.extend(wrapped(
                                &format!(
                                    "… {} more lines (t to inspect)",
                                    output_lines.len() - limit
                                ),
                                width,
                                "  ",
                                "  ",
                                theme.dim(),
                            ));
                        }
                    }
                }
                ChatItem::Info(text) => {
                    lines.extend(wrapped(text, width, "· ", "  ", theme.dim()));
                }
                ChatItem::Error(text) => {
                    lines.extend(wrapped(
                        text,
                        width,
                        "✗ ",
                        "  ",
                        Style::default().fg(palette.error),
                    ));
                }
            }
        }

        self.line_count = lines.len();
        if self.auto_scroll {
            self.scroll = lines
                .len()
                .saturating_sub(self.viewport_height as usize)
                .min(u16::MAX as usize) as u16;
        } else {
            let max = lines
                .len()
                .saturating_sub(self.viewport_height as usize)
                .min(u16::MAX as usize) as u16;
            self.scroll = self.scroll.min(max);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_consecutive_errors() {
        let mut chat = ChatState::new();
        chat.push_error("boom");
        chat.push_error("boom");
        assert_eq!(chat.items.len(), 1);
    }

    #[test]
    fn streams_into_one_assistant_item() {
        let mut chat = ChatState::new();
        chat.append_delta("hello ");
        chat.append_delta("world");
        chat.end_streaming();
        chat.append_delta("!");
        assert_eq!(chat.items.len(), 2);
        match &chat.items[0] {
            ChatItem::Assistant(text) => assert_eq!(text, "hello world"),
            _ => panic!("expected assistant"),
        }
    }

    #[test]
    fn tool_items_reference_the_log() {
        let mut chat = ChatState::new();
        let tools = vec![ToolLogEntry {
            id: "1".to_string(),
            name: "read_file".to_string(),
            arguments: "{\"path\":\"a\"}".to_string(),
            output: "ok".to_string(),
            is_error: false,
            duration_ms: Some(3),
            expanded: false,
        }];
        chat.push_tool(0);
        let lines = chat.build_lines(40, &tools, &Theme::dark());
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("read_file"));
        assert!(text.contains("ok"));
    }
}
