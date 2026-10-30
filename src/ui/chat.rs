//! Chat transcript state and rendering helpers.

use crate::provider::Message;
use crate::provider::types::Role;
use crate::ui::markdown::render_markdown;
use crate::ui::theme::{SyntaxEngine, Theme, ThemeMode};
use crate::ui::util::wrapped;
use crate::util::text::one_line;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;

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

#[derive(Debug, Clone)]
pub struct ReplyStats {
    pub duration_ms: u128,
    pub completion_tokens: u64,
    pub estimated: bool,
    pub model: String,
}

#[derive(Debug, Clone)]
struct MarkdownCache {
    text: String,
    width: u16,
    mode: ThemeMode,
    lines: Vec<Line<'static>>,
}

#[derive(Debug, Default)]
pub struct ChatState {
    pub items: Vec<ChatItem>,
    pub streaming: Option<usize>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub line_count: usize,
    pub viewport_height: u16,
    assistant_stats: HashMap<usize, ReplyStats>,
    markdown_cache: HashMap<usize, MarkdownCache>,
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
        self.assistant_stats.clear();
        self.markdown_cache.clear();
    }

    pub fn set_assistant_stats(&mut self, index: usize, stats: ReplyStats) {
        self.assistant_stats.insert(index, stats);
    }

    pub fn last_assistant_index(&self) -> Option<usize> {
        self.items
            .iter()
            .rposition(|item| matches!(item, ChatItem::Assistant(_)))
    }

    pub fn has_conversation(&self) -> bool {
        self.items.iter().any(|item| {
            matches!(
                item,
                ChatItem::User(_) | ChatItem::Assistant(_) | ChatItem::Tool(_)
            )
        })
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

    /// Rebuild a transcript and tool log from persisted provider messages.
    pub fn restore(messages: &[Message]) -> (Self, Vec<ToolLogEntry>) {
        let mut chat = Self::new();
        let mut tools: Vec<ToolLogEntry> = Vec::new();
        let mut call_index: HashMap<String, usize> = HashMap::new();
        let mut pending: HashMap<String, usize> = HashMap::new();

        for message in messages {
            match message.role {
                Role::System => {
                    if let Some(content) = &message.content
                        && content.starts_with("[context summary")
                    {
                        chat.push_info(content);
                    }
                }
                Role::User => {
                    if let Some(content) = &message.content {
                        chat.push_user(content);
                    }
                }
                Role::Assistant => {
                    if let Some(content) = &message.content
                        && !content.trim().is_empty()
                    {
                        chat.items.push(ChatItem::Assistant(content.clone()));
                    }
                    for call in &message.tool_calls {
                        let index = tools.len();
                        tools.push(ToolLogEntry {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            output: String::new(),
                            is_error: false,
                            duration_ms: None,
                            expanded: false,
                        });
                        chat.push_tool(index);
                        call_index.insert(call.id.clone(), index);
                        pending.insert(call.id.clone(), index);
                    }
                }
                Role::Tool => {
                    let Some(content) = &message.content else {
                        continue;
                    };
                    let Some(call_id) = &message.tool_call_id else {
                        continue;
                    };
                    if let Some(index) = call_index.get(call_id).copied() {
                        tools[index].output = content.clone();
                        tools[index].is_error = content.starts_with("ERROR:");
                        pending.remove(call_id);
                    } else {
                        let index = tools.len();
                        tools.push(ToolLogEntry {
                            id: call_id.clone(),
                            name: "tool".to_string(),
                            arguments: String::new(),
                            output: content.clone(),
                            is_error: content.starts_with("ERROR:"),
                            duration_ms: None,
                            expanded: false,
                        });
                        chat.push_tool(index);
                    }
                }
            }
        }

        for index in pending.values() {
            if tools[*index].output.is_empty() {
                tools[*index].output =
                    "ERROR: interrupted tool call; state may be partial.".to_string();
                tools[*index].is_error = true;
            }
        }
        chat.streaming = None;
        chat.auto_scroll = true;
        (chat, tools)
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
        syntax: &SyntaxEngine,
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
                    let cached = self.markdown_cache.get(&index).filter(|cached| {
                        cached.text == *text && cached.width == width && cached.mode == theme.mode
                    });
                    let mut item_lines = if let Some(cached) = cached {
                        cached.lines.clone()
                    } else {
                        let lines = render_markdown(text, width, theme, syntax);
                        self.markdown_cache.insert(
                            index,
                            MarkdownCache {
                                text: text.clone(),
                                width,
                                mode: theme.mode,
                                lines: lines.clone(),
                            },
                        );
                        lines
                    };
                    if item_lines.is_empty() {
                        item_lines.push(Line::from(""));
                    }
                    if self.streaming == Some(index)
                        && let Some(last) = item_lines.last_mut()
                    {
                        last.spans
                            .push(Span::styled("▌", Style::default().fg(palette.accent)));
                    }
                    lines.extend(item_lines);
                    if self.streaming != Some(index)
                        && let Some(stats) = self.assistant_stats.get(&index)
                    {
                        let seconds = stats.duration_ms as f64 / 1000.0;
                        let tokens_per_second = stats.completion_tokens as f64 / seconds.max(0.001);
                        let estimate = if stats.estimated { "~" } else { "" };
                        let footer = format!(
                            "  {seconds:.2}s · {estimate}{tokens_per_second:.1} tok/s · {}",
                            stats.model
                        );
                        lines.extend(wrapped(&footer, width, "", "", theme.dim()));
                    }
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
    fn assistant_footer_has_timing_and_model() {
        let mut chat = ChatState::new();
        chat.push_user("question");
        let index = chat.ensure_streaming();
        chat.append_delta("answer");
        chat.end_streaming();
        chat.set_assistant_stats(
            index,
            ReplyStats {
                duration_ms: 2_000,
                completion_tokens: 100,
                estimated: false,
                model: "ollama/qwen3.5:9b".to_string(),
            },
        );
        let lines = chat.build_lines(60, &[], &Theme::dark(), &SyntaxEngine::new());
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("2.00s"));
        assert!(text.contains("50.0 tok/s"));
        assert!(text.contains("ollama/qwen3.5:9b"));
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
        let lines = chat.build_lines(40, &tools, &Theme::dark(), &SyntaxEngine::new());
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("read_file"));
        assert!(text.contains("ok"));
    }
}
