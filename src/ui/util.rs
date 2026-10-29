//! Shared TUI primitives: text input, wrapping, popups.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use textwrap::wrap;

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_value(value: impl Into<String>) -> Self {
        let chars: Vec<char> = value.into().chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn set(&mut self, value: impl Into<String>) {
        self.chars = value.into().chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// Overwrite the buffer before dropping it (for secrets like API keys).
    pub fn wipe(&mut self) {
        self.chars.fill('\0');
        self.clear();
    }

    pub fn take(&mut self) -> String {
        let value = self.value();
        self.clear();
        value
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Visible slice and cursor column for a fixed-width single-line editor.
    pub fn window(&self, max: usize) -> (String, usize) {
        if max == 0 {
            return (String::new(), 0);
        }
        let skip = self.cursor.saturating_sub(max.saturating_sub(1));
        let text: String = self.chars.iter().skip(skip).take(max).collect();
        (text, self.cursor - skip)
    }
}

pub fn popup_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let width = ((area.width as u32 * percent_x as u32) / 100).clamp(1, area.width as u32) as u16;
    let height =
        ((area.height as u32 * percent_y as u32) / 100).clamp(1, area.height as u32) as u16;
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    )
}

/// Wrap text into styled lines, prefixing the first line separately.
pub fn wrapped(
    text: &str,
    width: u16,
    first_prefix: &str,
    continuation_prefix: &str,
    style: Style,
) -> Vec<Line<'static>> {
    let width = width.max(4) as usize;
    let mut lines = Vec::new();
    for (line_index, source) in text.split('\n').enumerate() {
        let prefix = if line_index == 0 {
            first_prefix
        } else {
            continuation_prefix
        };
        let prefix_width = prefix.chars().count();
        let available = width.saturating_sub(prefix_width).max(1);
        let chunks = wrap(source, available);
        if chunks.is_empty() {
            lines.push(Line::from(vec![Span::styled(prefix.to_string(), style)]));
        } else {
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let current_prefix = if chunk_index == 0 {
                    prefix
                } else {
                    continuation_prefix
                };
                lines.push(Line::from(vec![
                    Span::styled(current_prefix.to_string(), style),
                    Span::styled(chunk.to_string(), style),
                ]));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut result: String = text.chars().take(max.saturating_sub(1)).collect();
    result.push('…');
    result
}

pub fn slice_chars(text: &str, skip: usize, max: usize) -> String {
    text.chars().skip(skip).take(max).collect()
}

pub fn spinner(frame: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[frame % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_input_edits_at_cursor() {
        let mut input = TextInput::with_value("ac");
        input.left();
        input.insert('b');
        assert_eq!(input.value(), "abc");
        input.backspace();
        assert_eq!(input.value(), "ac");
        input.home();
        input.delete();
        assert_eq!(input.value(), "c");
        input.end();
        input.insert('!');
        assert_eq!(input.value(), "c!");
    }

    #[test]
    fn popup_is_centered_and_clamped() {
        let popup = popup_rect(Rect::new(0, 0, 100, 40), 50, 50);
        assert_eq!(popup.width, 50);
        assert_eq!(popup.height, 20);
        assert_eq!(popup.x, 25);
        assert_eq!(popup.y, 10);
    }

    #[test]
    fn wrapping_prefixes_continuation_lines() {
        let lines = wrapped("one two three four five", 12, "> ", "  ", Style::default());
        assert!(lines.len() > 1);
        assert!(lines[0].spans[0].content.starts_with("> "));
        assert_eq!(lines[1].spans[0].content, "  ");
    }
}
