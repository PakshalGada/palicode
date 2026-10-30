//! Width-aware Markdown renderer for assistant answers.
//!
//! The renderer turns CommonMark/GFM into styled ratatui lines. It supports
//! headings, emphasis, inline code, links, lists (including task lists),
//! block quotes, rules, fenced code blocks with real syntax colors, and
//! bordered tables that shrink to fit the available width.

use crate::ui::theme::{SyntaxEngine, Theme};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub fn render_markdown(
    text: &str,
    width: u16,
    theme: &Theme,
    syntax: &SyntaxEngine,
) -> Vec<Line<'static>> {
    let mut renderer = MarkdownRenderer::new(width.max(12) as usize, theme, syntax);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct ListContext {
    ordered: bool,
    next: u64,
    item_active: bool,
    first_line: bool,
    marker: String,
}

impl ListContext {
    fn new(ordered: Option<u64>) -> Self {
        Self {
            ordered: ordered.is_some(),
            next: ordered.unwrap_or(1),
            item_active: false,
            first_line: false,
            marker: String::new(),
        }
    }

    fn start_item(&mut self) {
        self.marker = if self.ordered {
            let marker = format!("{}. ", self.next);
            self.next = self.next.saturating_add(1);
            marker
        } else {
            "• ".to_string()
        };
        self.item_active = true;
        self.first_line = true;
    }
}

struct TableBuilder {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    header_rows: usize,
    in_head: bool,
    current_row: Option<Vec<String>>,
    current_cell: Option<String>,
}

impl TableBuilder {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            rows: Vec::new(),
            header_rows: 0,
            in_head: false,
            current_row: None,
            current_cell: None,
        }
    }

    fn append(&mut self, text: &str) {
        if let Some(cell) = self.current_cell.as_mut() {
            cell.push_str(text);
        }
    }
}

struct MarkdownRenderer<'a> {
    width: usize,
    theme: &'a Theme,
    syntax: &'a SyntaxEngine,
    lines: Vec<Line<'static>>,
    current: Vec<(char, Style)>,
    current_width: usize,
    last_space: Option<usize>,
    styles: Vec<Style>,
    quote_depth: usize,
    lists: Vec<ListContext>,
    in_code: bool,
    code_lang: String,
    code_buffer: String,
    table: Option<TableBuilder>,
    link_target: Option<String>,
}

impl<'a> MarkdownRenderer<'a> {
    fn new(width: usize, theme: &'a Theme, syntax: &'a SyntaxEngine) -> Self {
        Self {
            width,
            theme,
            syntax,
            lines: Vec::new(),
            current: Vec::new(),
            current_width: 0,
            last_space: None,
            styles: Vec::new(),
            quote_depth: 0,
            lists: Vec::new(),
            in_code: false,
            code_lang: String::new(),
            code_buffer: String::new(),
            table: None,
            link_target: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if self.in_code {
            match event {
                Event::Text(text) => self.code_buffer.push_str(&text),
                Event::SoftBreak | Event::HardBreak => self.code_buffer.push('\n'),
                Event::End(TagEnd::CodeBlock) => self.finish_code(),
                _ => {}
            }
            return;
        }

        if self.table.is_some() {
            self.table_event(event);
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.append_text(&text, self.current_style()),
            Event::Code(text) | Event::InlineMath(text) => {
                let style = self
                    .current_style()
                    .fg(self.theme.palette.inline_code)
                    .bg(self.theme.palette.inline_code_bg);
                self.append_text_preserving_spaces(&text, style);
            }
            Event::DisplayMath(text) => {
                self.flush_current();
                let style = Style::default()
                    .fg(self.theme.palette.inline_code)
                    .bg(self.theme.palette.inline_code_bg);
                for line in text.lines() {
                    self.append_text_preserving_spaces(line, style);
                    self.flush_current();
                }
                self.blank();
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.append_text(&text, self.theme.dim());
            }
            Event::FootnoteReference(label) => {
                self.append_text(&format!("[^{label}]"), self.theme.dim());
            }
            Event::SoftBreak => self.append_text(" ", self.current_style()),
            Event::HardBreak => self.flush_current(),
            Event::Rule => {
                self.flush_current();
                let width = self.width.saturating_sub(self.prefix_width()).clamp(1, 80);
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(width),
                    Style::default().fg(self.theme.palette.table_border),
                )));
                self.blank();
            }
            Event::TaskListMarker(checked) => {
                if let Some(list) = self.lists.iter_mut().rev().find(|list| list.item_active) {
                    list.marker = if checked {
                        "[x] ".to_string()
                    } else {
                        "[ ] ".to_string()
                    };
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if !self.current.is_empty() {
                    self.flush_current();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_current();
                let style = self
                    .theme
                    .text()
                    .fg(self.theme.palette.heading)
                    .add_modifier(Modifier::BOLD);
                self.styles.push(style);
                let _ = level;
            }
            Tag::BlockQuote(_) => {
                self.flush_current();
                let style = self
                    .current_style()
                    .fg(self.theme.palette.quote)
                    .add_modifier(Modifier::ITALIC);
                self.styles.push(style);
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_current();
                self.in_code = true;
                self.code_buffer.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang
                        .split(|ch: char| ch.is_whitespace() || ch == ',')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                if !self.current.is_empty() {
                    self.flush_current();
                }
                self.lists.push(ListContext::new(start));
            }
            Tag::Item => {
                self.flush_current();
                if let Some(list) = self.lists.last_mut() {
                    list.start_item();
                }
            }
            Tag::Table(alignments) => {
                self.flush_current();
                self.table = Some(TableBuilder::new(alignments));
            }
            Tag::Strong => {
                let style = self.current_style().add_modifier(Modifier::BOLD);
                self.styles.push(style);
            }
            Tag::Emphasis => {
                let style = self.current_style().add_modifier(Modifier::ITALIC);
                self.styles.push(style);
            }
            Tag::Strikethrough => {
                let style = self.current_style().add_modifier(Modifier::CROSSED_OUT);
                self.styles.push(style);
            }
            Tag::Link { dest_url, .. } => {
                let style = self
                    .current_style()
                    .fg(self.theme.palette.link)
                    .add_modifier(Modifier::UNDERLINED);
                self.styles.push(style);
                self.link_target = Some(dest_url.to_string());
            }
            Tag::Image { .. } => {
                let style = self.current_style().add_modifier(Modifier::ITALIC);
                self.styles.push(style);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_current();
                if self.any_item_active().is_none() {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush_current();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.styles.pop();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.flush_current();
                if self.any_item_active().is_none() {
                    self.blank();
                }
            }
            TagEnd::List(_) => {
                self.flush_current();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush_current();
                if let Some(list) = self.lists.last_mut() {
                    list.item_active = false;
                }
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(target) = self.link_target.take() {
                    self.append_text(&format!(" ({target})"), self.theme.dim());
                }
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Image => {
                self.styles.pop();
            }
            _ => {}
        }
    }

    fn table_event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    table.in_head = true;
                    table.current_row = Some(Vec::new());
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = self.table.as_mut() {
                    if let Some(row) = table.current_row.take() {
                        table.header_rows += 1;
                        table.rows.push(row);
                    }
                    table.in_head = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row = Some(Vec::new());
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    table.current_cell = Some(String::new());
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(table) = self.table.as_mut() {
                    table.append(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(table) = self.table.as_mut() {
                    table.append(" ");
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = self.table.as_mut()
                    && let Some(cell) = table.current_cell.take()
                {
                    table.current_row.get_or_insert_with(Vec::new).push(cell);
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = self.table.as_mut()
                    && let Some(row) = table.current_row.take()
                {
                    if table.in_head {
                        table.header_rows += 1;
                    }
                    table.rows.push(row);
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
            }
            _ => {}
        }
    }

    fn current_style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or_else(|| self.theme.text())
    }

    fn any_item_active(&self) -> Option<(usize, &ListContext)> {
        self.lists
            .iter()
            .enumerate()
            .rev()
            .find(|(_, list)| list.item_active)
    }

    fn prefix(&self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if self.quote_depth > 0 {
            spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                Style::default().fg(self.theme.palette.quote),
            ));
        }
        if let Some((index, list)) = self.any_item_active() {
            let indent = "  ".repeat(index);
            if !indent.is_empty() {
                spans.push(Span::raw(indent));
            }
            if list.first_line {
                spans.push(Span::styled(
                    list.marker.clone(),
                    Style::default().fg(self.theme.palette.accent2),
                ));
            } else {
                spans.push(Span::raw(" ".repeat(list.marker.chars().count())));
            }
        }
        spans
    }

    fn prefix_width(&self) -> usize {
        let quote = self.quote_depth * 2;
        let list = self
            .any_item_active()
            .map(|(index, list)| index * 2 + list.marker.chars().count())
            .unwrap_or(0);
        quote + list
    }

    fn available(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(4)
    }

    fn append_text(&mut self, text: &str, style: Style) {
        self.append_text_inner(text, style, false);
    }

    fn append_text_preserving_spaces(&mut self, text: &str, style: Style) {
        self.append_text_inner(text, style, true);
    }

    fn append_text_inner(&mut self, text: &str, style: Style, preserve_spaces: bool) {
        for ch in text.chars() {
            if ch == '\n' {
                self.flush_current();
                continue;
            }
            if !preserve_spaces
                && ch == ' '
                && (self.current.is_empty() || self.current.last().is_some_and(|c| c.0 == ' '))
            {
                continue;
            }
            self.current.push((ch, style));
            self.current_width += 1;
            if ch == ' ' {
                self.last_space = Some(self.current.len() - 1);
            }
            self.wrap_current();
        }
    }

    fn wrap_current(&mut self) {
        let available = self.available();
        while self.current_width > available {
            let split = self
                .last_space
                .filter(|index| *index > 0 && *index < self.current.len())
                .unwrap_or_else(|| available.min(self.current.len()));
            if split == 0 {
                break;
            }
            let line: Vec<(char, Style)> = self.current.drain(..split).collect();
            self.emit_chars(line);
            while self.current.first().is_some_and(|c| c.0 == ' ') {
                self.current.remove(0);
            }
            self.current_width = self.current.len();
            self.last_space = self
                .current
                .iter()
                .enumerate()
                .rev()
                .find(|(_, c)| c.0 == ' ')
                .map(|(index, _)| index);
        }
    }

    fn flush_current(&mut self) {
        if self.current.is_empty() {
            return;
        }
        while self.current.last().is_some_and(|c| c.0 == ' ') {
            self.current.pop();
        }
        if self.current.is_empty() {
            return;
        }
        let chars = std::mem::take(&mut self.current);
        self.current_width = 0;
        self.last_space = None;
        self.emit_chars(chars);
    }

    fn emit_chars(&mut self, chars: Vec<(char, Style)>) {
        let mut spans = self.prefix();
        let mut buffer = String::new();
        let mut active: Option<Style> = None;
        for (ch, style) in chars {
            if active == Some(style) {
                buffer.push(ch);
            } else {
                if let Some(previous) = active
                    && !buffer.is_empty()
                {
                    spans.push(Span::styled(std::mem::take(&mut buffer), previous));
                }
                active = Some(style);
                buffer.push(ch);
            }
        }
        if let Some(style) = active
            && !buffer.is_empty()
        {
            spans.push(Span::styled(buffer, style));
        }
        self.lines.push(Line::from(spans));
        if let Some((_, list)) = self
            .lists
            .iter_mut()
            .enumerate()
            .rev()
            .find(|(_, list)| list.item_active)
        {
            list.first_line = false;
        }
    }

    fn blank(&mut self) {
        self.flush_current();
        if self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            return;
        }
        self.lines.push(Line::from(""));
    }

    fn finish_code(&mut self) {
        self.in_code = false;
        let code = std::mem::take(&mut self.code_buffer);
        let lang = std::mem::take(&mut self.code_lang);
        self.render_code(&code, &lang);
        self.blank();
    }

    fn render_code(&mut self, code: &str, lang: &str) {
        let border = Style::default().fg(self.theme.palette.code_border);
        let code_bg = Style::default().bg(self.theme.palette.code_bg);
        let label = if lang.is_empty() { "code" } else { lang };
        let label = truncate_chars(label, 24).to_string();
        self.lines.push(Line::from(vec![
            Span::styled("  ┌─ ", border),
            Span::styled(label, border.add_modifier(Modifier::ITALIC)),
        ]));

        let available = self.width.saturating_sub(4).max(1);
        let path = format!("code.{}", syntax_extension(lang));
        let plain_lang = matches!(
            lang.to_ascii_lowercase().as_str(),
            "" | "text" | "txt" | "plain" | "plaintext"
        );
        let mut source_lines: Vec<&str> = code.split('\n').collect();
        if code.ends_with('\n') {
            source_lines.pop();
        }
        if source_lines.is_empty() {
            source_lines.push("");
        }

        for source in source_lines {
            let expanded = source.replace('\t', "    ");
            let chars: Vec<char> = expanded.chars().collect();
            let chunks: Vec<String> = if chars.is_empty() {
                vec![String::new()]
            } else {
                chars
                    .chunks(available)
                    .map(|chunk| chunk.iter().collect())
                    .collect()
            };
            for chunk in chunks {
                let mut spans = vec![Span::styled("  │ ", border.patch(code_bg))];
                if plain_lang {
                    let style = Style::default()
                        .fg(self.theme.palette.fg)
                        .bg(self.theme.palette.code_bg);
                    spans.push(Span::styled(chunk.clone(), style));
                } else {
                    for (style, text) in
                        self.syntax
                            .highlight_colored(&chunk, &path, self.theme.mode)
                    {
                        spans.push(Span::styled(text, style.bg(self.theme.palette.code_bg)));
                    }
                }
                let used = chunk.chars().count();
                if used < available {
                    spans.push(Span::styled(
                        " ".repeat(available - used),
                        Style::default().bg(self.theme.palette.code_bg),
                    ));
                }
                self.lines.push(Line::from(spans));
            }
        }
        self.lines.push(Line::from(Span::styled("  └─", border)));
    }

    fn render_table(&mut self, table: TableBuilder) {
        let mut rows = table.rows;
        if rows.is_empty() {
            return;
        }
        let cols = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(table.alignments.len());
        if cols == 0 {
            return;
        }
        let mut alignments = table.alignments;
        alignments.resize(cols, Alignment::None);
        for row in &mut rows {
            row.resize(cols, String::new());
        }

        let mut widths = vec![3usize; cols];
        for row in &rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count().min(60));
            }
        }
        let overhead = 3 * cols + 1;
        while widths.iter().sum::<usize>() + overhead > self.width {
            let Some((index, width)) = widths
                .iter()
                .enumerate()
                .filter(|(_, width)| **width > 3)
                .max_by_key(|(_, width)| **width)
            else {
                break;
            };
            widths[index] = width.saturating_sub(1);
        }

        if widths.iter().sum::<usize>() + overhead > self.width {
            self.render_plain_table(&rows, table.header_rows);
            self.blank();
            return;
        }

        let border = Style::default().fg(self.theme.palette.table_border);
        let header = Style::default()
            .fg(self.theme.palette.table_header)
            .add_modifier(Modifier::BOLD);
        let body = self.theme.text();

        let top = format!(
            "┌{}┐",
            widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join("┬")
        );
        self.lines.push(Line::from(Span::styled(top, border)));
        for (row_index, row) in rows.iter().enumerate() {
            let is_header = row_index < table.header_rows;
            let style = if is_header { header } else { body };
            let mut spans = vec![Span::styled("│", border)];
            for (column, cell) in row.iter().enumerate() {
                spans.push(Span::styled(" ", border));
                spans.push(Span::styled(
                    pad_cell(cell, widths[column], alignments[column]),
                    style,
                ));
                spans.push(Span::styled(" ", border));
                spans.push(Span::styled("│", border));
            }
            self.lines.push(Line::from(spans));
            if is_header && row_index + 1 == table.header_rows {
                let separator = format!(
                    "├{}┤",
                    widths
                        .iter()
                        .map(|width| "─".repeat(width + 2))
                        .collect::<Vec<_>>()
                        .join("┼")
                );
                self.lines.push(Line::from(Span::styled(separator, border)));
            }
        }
        let bottom = format!(
            "└{}┘",
            widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join("┴")
        );
        self.lines.push(Line::from(Span::styled(bottom, border)));
    }

    fn render_plain_table(&mut self, rows: &[Vec<String>], header_rows: usize) {
        let border = Style::default().fg(self.theme.palette.table_border);
        let header = Style::default()
            .fg(self.theme.palette.table_header)
            .add_modifier(Modifier::BOLD);
        for (index, row) in rows.iter().enumerate() {
            let line = truncate_chars(&row.join("  │  "), self.width);
            let style = if index < header_rows {
                header
            } else {
                self.theme.text()
            };
            self.lines
                .push(Line::from(Span::styled(line.to_string(), style)));
            if index + 1 == header_rows {
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(self.width.min(80)),
                    border,
                )));
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_current();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        if self.lines.is_empty() {
            self.lines.push(Line::from(""));
        }
        self.lines
    }
}

fn syntax_extension(lang: &str) -> String {
    let lang = lang.to_ascii_lowercase();
    match lang.as_str() {
        "rust" | "rs" => "rs",
        "javascript" | "js" | "jsx" | "node" => "js",
        "typescript" | "ts" | "tsx" => "ts",
        "python" | "py" => "py",
        "bash" | "sh" | "shell" | "zsh" | "console" => "sh",
        "c++" | "cpp" | "cxx" | "h++" => "cpp",
        "objective-c" | "objc" => "m",
        "markdown" | "md" => "md",
        "yml" | "yaml" => "yaml",
        "dockerfile" | "docker" => "dockerfile",
        "makefile" | "make" => "make",
        "text" | "txt" | "plain" | "plaintext" => "txt",
        other => other,
    }
    .to_string()
}

fn pad_cell(text: &str, width: usize, alignment: Alignment) -> String {
    let text = truncate_chars(text, width);
    let padding = width.saturating_sub(text.chars().count());
    match alignment {
        Alignment::Right => format!("{}{}", " ".repeat(padding), text),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        Alignment::None | Alignment::Left => format!("{}{}", text, " ".repeat(padding)),
    }
}

fn truncate_chars(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn render(markdown: &str, width: u16) -> Vec<Line<'static>> {
        let theme = Theme::dark();
        let syntax = SyntaxEngine::new();
        render_markdown(markdown, width, &theme, &syntax)
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_headings_bold_and_colored() {
        let lines = render("## Results", 60);
        assert!(text(&lines).contains("Results"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.style.add_modifier.contains(Modifier::BOLD)
                && span.style.fg == Some(Theme::dark().palette.heading)
        }));
    }

    #[test]
    fn renders_emphasis_and_strikethrough() {
        let lines = render("This is **bold**, *italic*, and ~~gone~~.", 60);
        let spans: Vec<&Span<'_>> = lines.iter().flat_map(|line| &line.spans).collect();
        assert!(spans.iter().any(|span| {
            span.content.contains("bold") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(spans.iter().any(|span| {
            span.content.contains("italic") && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
        assert!(spans.iter().any(|span| {
            span.content.contains("gone") && span.style.add_modifier.contains(Modifier::CROSSED_OUT)
        }));
    }

    #[test]
    fn renders_inline_code_and_links() {
        let lines = render("Use `cargo test` and [docs](https://example.com).", 80);
        let spans: Vec<&Span<'_>> = lines.iter().flat_map(|line| &line.spans).collect();
        assert!(spans.iter().any(|span| {
            span.content.contains("cargo test")
                && span.style.fg == Some(Theme::dark().palette.inline_code)
        }));
        assert!(spans.iter().any(|span| {
            span.content.contains("docs")
                && span.style.fg == Some(Theme::dark().palette.link)
                && span.style.add_modifier.contains(Modifier::UNDERLINED)
        }));
    }

    #[test]
    fn renders_lists_and_task_markers() {
        let output = text(&render("- one\n- [x] done\n\n1. first\n2. second", 60));
        assert!(output.contains("• one"));
        assert!(output.contains("[x] done"));
        assert!(output.contains("1. first"));
        assert!(output.contains("2. second"));
    }

    #[test]
    fn renders_quotes_and_rules() {
        let output = text(&render("> quoted text\n\n---", 40));
        assert!(output.contains("│ quoted text"));
        assert!(output.contains('─'));
    }

    #[test]
    fn renders_bordered_table_with_header() {
        let lines = render(
            "| Name | Value |\n| :--- | ---: |\n| a | 1 |\n| b | 22 |",
            60,
        );
        let output = text(&lines);
        assert!(output.contains('┌'));
        assert!(output.contains('┼'));
        assert!(output.contains('└'));
        assert!(output.contains("Name"));
        assert!(output.contains("Value"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("Name") && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn renders_fenced_code_without_fences() {
        let lines = render(
            "Intro\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```",
            60,
        );
        let output = text(&lines);
        assert!(output.contains("fn main()"));
        assert!(!output.contains("```"));
        assert!(output.contains('┌'));
        assert!(output.contains('└'));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            match span.style.fg {
                Some(Color::Rgb(r, g, b)) => r != g || g != b,
                _ => false,
            }
        }));
    }

    #[test]
    fn wraps_long_paragraphs_to_width() {
        let lines = render(
            "This is a long paragraph that should wrap cleanly inside a narrow terminal window.",
            24,
        );
        assert!(lines.len() > 2);
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
                <= 24
        }));
    }

    #[test]
    fn table_fits_available_width_or_falls_back() {
        let lines = render(
            "| Very long heading | Another long heading |\n| --- | --- |\n| long content here | more content |",
            24,
        );
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>()
                <= 24
        }));
    }
}
