//! Diff pane: parse tool diffs, navigate hunks, and accept/reject changes.

use crate::tools::ToolContext;
use crate::tools::patch::{FilePatch, Hunk, HunkLine, apply_hunks, parse_unified_patch};
use crate::ui::theme::{SyntaxEngine, Theme};
use crate::ui::util::{slice_chars, truncate_chars};
use anyhow::{Context, Result, bail};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffMode {
    #[default]
    Unified,
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkStatus {
    Pending,
    Accepted,
    Rejected,
}

impl HunkStatus {
    fn icon(self) -> &'static str {
        match self {
            HunkStatus::Pending => "·",
            HunkStatus::Accepted => "✓",
            HunkStatus::Rejected => "✗",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Pending,
    Partial,
    Accepted,
    Rejected,
}

impl FileStatus {
    fn label(self) -> &'static str {
        match self {
            FileStatus::Pending => "pending",
            FileStatus::Partial => "partial",
            FileStatus::Accepted => "accepted",
            FileStatus::Rejected => "reverted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HunkView {
    pub hunk: Hunk,
    pub status: HunkStatus,
}

#[derive(Debug, Clone)]
pub struct FileDiffView {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<HunkView>,
    pub status: FileStatus,
    pub tool: String,
}

impl FileDiffView {
    pub fn path(&self) -> String {
        self.new_path
            .clone()
            .or_else(|| self.old_path.clone())
            .unwrap_or_else(|| "<unknown>".to_string())
    }

    fn recompute_status(&mut self) {
        let total = self.hunks.len();
        if total == 0 {
            self.status = FileStatus::Pending;
            return;
        }
        let accepted = self
            .hunks
            .iter()
            .filter(|hunk| hunk.status == HunkStatus::Accepted)
            .count();
        let rejected = self
            .hunks
            .iter()
            .filter(|hunk| hunk.status == HunkStatus::Rejected)
            .count();
        self.status = if accepted == total {
            FileStatus::Accepted
        } else if rejected == total {
            FileStatus::Rejected
        } else if accepted + rejected > 0 {
            FileStatus::Partial
        } else {
            FileStatus::Pending
        };
    }
}

#[derive(Debug, Default)]
pub struct DiffPane {
    pub files: Vec<FileDiffView>,
    pub selected_file: usize,
    pub selected_hunk: usize,
    pub mode: DiffMode,
    pub scroll: u16,
    pub x_scroll: u16,
    pub viewport_height: u16,
    hunk_lines: Vec<usize>,
}

impl DiffPane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ingest(&mut self, tool: &str, patch: &str) -> Result<usize> {
        let parsed = parse_unified_patch(patch).context("cannot parse diff")?;
        let mut count = 0;
        for file in parsed {
            if file.old_path.is_none() && file.new_path.is_none() {
                continue;
            }
            let view = from_patch(file, tool);
            let path = view.path();
            if let Some(index) = self.files.iter().position(|file| file.path() == path) {
                self.files[index] = view;
                self.selected_file = index;
            } else {
                self.files.push(view);
                self.selected_file = self.files.len() - 1;
            }
            count += 1;
        }
        if count == 0 {
            bail!("diff contains no file changes");
        }
        self.selected_hunk = 0;
        self.scroll = 0;
        self.x_scroll = 0;
        Ok(count)
    }

    pub fn next_file(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        let max = self.files.len() - 1;
        self.selected_file = (self.selected_file as isize + delta).clamp(0, max as isize) as usize;
        self.selected_hunk = 0;
        self.scroll = 0;
        self.x_scroll = 0;
    }

    pub fn next_hunk(&mut self, delta: isize) {
        let Some(file) = self.files.get(self.selected_file) else {
            return;
        };
        if file.hunks.is_empty() {
            return;
        }
        let max = file.hunks.len() - 1;
        self.selected_hunk = (self.selected_hunk as isize + delta).clamp(0, max as isize) as usize;
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            DiffMode::Unified => DiffMode::Split,
            DiffMode::Split => DiffMode::Unified,
        };
        self.scroll = 0;
    }

    pub fn scroll_horizontal(&mut self, delta: isize) {
        self.x_scroll = (self.x_scroll as isize + delta).max(0) as u16;
    }

    pub fn accept_hunk(&mut self) -> Option<String> {
        let file = self.files.get_mut(self.selected_file)?;
        let hunk = file.hunks.get_mut(self.selected_hunk)?;
        if hunk.status == HunkStatus::Rejected {
            return Some(format!(
                "Hunk {} was reverted; re-run the tool to re-apply it",
                self.selected_hunk + 1
            ));
        }
        hunk.status = HunkStatus::Accepted;
        file.recompute_status();
        Some(format!(
            "Accepted hunk {} in {}",
            self.selected_hunk + 1,
            file.path()
        ))
    }

    pub fn accept_file(&mut self) -> Option<String> {
        let file = self.files.get_mut(self.selected_file)?;
        let mut rejected = 0;
        for hunk in &mut file.hunks {
            if hunk.status == HunkStatus::Rejected {
                rejected += 1;
            } else {
                hunk.status = HunkStatus::Accepted;
            }
        }
        file.recompute_status();
        Some(if rejected > 0 {
            format!(
                "Accepted pending hunks in {} ({} reverted hunk(s) left as-is)",
                file.path(),
                rejected
            )
        } else {
            format!("Accepted all hunks in {}", file.path())
        })
    }

    pub fn reject_hunk(&mut self, ctx: &ToolContext) -> Result<String> {
        let Some(file) = self.files.get(self.selected_file) else {
            bail!("no diff selected");
        };
        if file.hunks.get(self.selected_hunk).map(|hunk| hunk.status) == Some(HunkStatus::Rejected)
        {
            return Ok(format!(
                "Hunk {} is already reverted",
                self.selected_hunk + 1
            ));
        }
        if file.old_path.is_none() || file.new_path.is_none() {
            return self.reject_file(ctx);
        }
        let file_index = self.selected_file;
        let hunk_index = self.selected_hunk;
        let path = self.files[file_index].path();
        let hunk = self.files[file_index]
            .hunks
            .get(hunk_index)
            .context("no hunk selected")?
            .hunk
            .clone();
        let absolute = ctx.resolve(&path)?;
        let current = std::fs::read_to_string(&absolute)
            .with_context(|| format!("cannot read {}", absolute.display()))?;
        let reversed = reverse_hunk(&hunk);
        let updated = apply_hunks(&current, &[reversed])
            .with_context(|| format!("cannot revert hunk in {path}"))?;
        std::fs::write(&absolute, updated)
            .with_context(|| format!("cannot write {}", absolute.display()))?;
        self.files[file_index].hunks[hunk_index].status = HunkStatus::Rejected;
        self.files[file_index].recompute_status();
        Ok(format!("Reverted hunk {} in {path}", hunk_index + 1))
    }

    pub fn reject_file(&mut self, ctx: &ToolContext) -> Result<String> {
        let Some(file) = self.files.get(self.selected_file) else {
            bail!("no diff selected");
        };
        if file.status == FileStatus::Rejected {
            return Ok(format!("{} is already reverted", file.path()));
        }
        let file_index = self.selected_file;
        let old_path = file.old_path.clone();
        let new_path = file.new_path.clone();
        let hunks: Vec<Hunk> = file.hunks.iter().map(|hunk| hunk.hunk.clone()).collect();
        let display = file.path();

        match (old_path.as_deref(), new_path.as_deref()) {
            (None, Some(_)) => {
                // Creation: rejecting means deleting the new file.
                let absolute = ctx.resolve(&display)?;
                if absolute.exists() {
                    std::fs::remove_file(&absolute)
                        .with_context(|| format!("cannot remove {}", absolute.display()))?;
                }
            }
            (Some(old), None) => {
                // Deletion: reconstruct the old content from the removed lines.
                let content = old_content_from_deletion(&hunks);
                let absolute = ctx.resolve(old)?;
                if let Some(parent) = absolute.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&absolute, content)
                    .with_context(|| format!("cannot recreate {}", absolute.display()))?;
            }
            (Some(_), Some(new)) => {
                let absolute = ctx.resolve(new)?;
                let current = std::fs::read_to_string(&absolute)
                    .with_context(|| format!("cannot read {}", absolute.display()))?;
                let reversed: Vec<Hunk> = hunks.iter().rev().map(reverse_hunk).collect();
                let updated = apply_hunks(&current, &reversed)
                    .with_context(|| format!("cannot revert {display}"))?;
                std::fs::write(&absolute, updated)
                    .with_context(|| format!("cannot write {}", absolute.display()))?;
            }
            (None, None) => bail!("diff has no path"),
        }

        let file = &mut self.files[file_index];
        for hunk in &mut file.hunks {
            hunk.status = HunkStatus::Rejected;
        }
        file.recompute_status();
        Ok(format!("Reverted {display}"))
    }

    /// Select a hunk from a mouse click row (relative to the content area).
    pub fn select_row(&mut self, row: u16) {
        let target = self.scroll as usize + row as usize;
        if let Some((index, _)) = self
            .hunk_lines
            .iter()
            .enumerate()
            .rev()
            .find(|(_, line)| **line <= target)
        {
            let max = self
                .files
                .get(self.selected_file)
                .map(|file| file.hunks.len().saturating_sub(1))
                .unwrap_or(0);
            self.selected_hunk = index.min(max);
        }
    }

    pub fn ensure_hunk_visible(&mut self, viewport_height: u16) {
        if self.hunk_lines.is_empty() {
            self.scroll = 0;
            return;
        }
        let line = self
            .hunk_lines
            .get(self.selected_hunk)
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

    pub fn hint(&self, width: usize) -> String {
        let mode = match self.mode {
            DiffMode::Unified => "unified",
            DiffMode::Split => "split",
        };
        truncate_chars(
            &format!("j/k hunk · h/l file · a/A accept · r/R reject · u {mode} · ←/→ scroll"),
            width,
        )
    }

    pub fn build_lines(
        &mut self,
        width: u16,
        theme: &Theme,
        syntax: &SyntaxEngine,
    ) -> Vec<Line<'static>> {
        let palette = theme.palette;
        let mut lines = Vec::new();
        self.hunk_lines.clear();

        if self.files.is_empty() {
            lines.push(Line::from(Span::styled(
                "No diffs yet.",
                Style::default().fg(palette.fg),
            )));
            lines.push(Line::from(Span::styled(
                "File changes from edit_file, write_file and apply_patch appear here.",
                theme.dim(),
            )));
            return lines;
        }

        // File tabs.
        let mut tabs = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            let style = if index == self.selected_file {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.dim()
            };
            tabs.push(Span::styled(
                format!("[{}] ", truncate_chars(&file.path(), 22)),
                style,
            ));
        }
        lines.push(Line::from(tabs));

        let file = &self.files[self.selected_file];
        let path = file.path();
        lines.push(Line::from(Span::styled(
            format!("{} · {} · {}", file.tool, path, file.status.label()),
            theme.dim(),
        )));

        let x_scroll = self.x_scroll as usize;
        match self.mode {
            DiffMode::Unified => {
                for hunk in &file.hunks {
                    self.hunk_lines.push(lines.len());
                    let selected = self.hunk_lines.len() - 1 == self.selected_hunk;
                    lines.push(hunk_header(hunk, selected, width, theme));
                    let mut old_line = hunk.hunk.old_start;
                    let mut new_line = hunk.hunk.new_start;
                    for line in &hunk.hunk.lines {
                        let (old_no, new_no, sign) = match line.kind {
                            '+' => {
                                let current = new_line;
                                new_line += 1;
                                (None, Some(current), '+')
                            }
                            '-' => {
                                let current = old_line;
                                old_line += 1;
                                (Some(current), None, '-')
                            }
                            _ => {
                                let old = old_line;
                                let new = new_line;
                                old_line += 1;
                                new_line += 1;
                                (Some(old), Some(new), ' ')
                            }
                        };
                        lines.push(unified_body_line(
                            old_no, new_no, sign, &line.text, &path, width, x_scroll, theme,
                            syntax, selected,
                        ));
                    }
                    lines.push(Line::from(""));
                }
            }
            DiffMode::Split => {
                for hunk in &file.hunks {
                    self.hunk_lines.push(lines.len());
                    let selected = self.hunk_lines.len() - 1 == self.selected_hunk;
                    lines.push(hunk_header(hunk, selected, width, theme));
                    let half = width.saturating_sub(1) / 2;
                    let mut old_line = hunk.hunk.old_start;
                    let mut new_line = hunk.hunk.new_start;
                    for line in &hunk.hunk.lines {
                        let row = match line.kind {
                            '+' => {
                                let row = (None, Some((new_line, '+', line.text.as_str())));
                                new_line += 1;
                                row
                            }
                            '-' => {
                                let row = (Some((old_line, '-', line.text.as_str())), None);
                                old_line += 1;
                                row
                            }
                            _ => {
                                let row = (
                                    Some((old_line, ' ', line.text.as_str())),
                                    Some((new_line, ' ', line.text.as_str())),
                                );
                                old_line += 1;
                                new_line += 1;
                                row
                            }
                        };
                        lines.push(split_row(
                            row.0, row.1, half, &path, x_scroll, theme, syntax, selected,
                        ));
                    }
                    lines.push(Line::from(""));
                }
            }
        }

        lines
    }
}

fn from_patch(file: FilePatch, tool: &str) -> FileDiffView {
    FileDiffView {
        old_path: file.old_path,
        new_path: file.new_path,
        hunks: file
            .hunks
            .into_iter()
            .map(|hunk| HunkView {
                hunk,
                status: HunkStatus::Pending,
            })
            .collect(),
        status: FileStatus::Pending,
        tool: tool.to_string(),
    }
}

fn reverse_hunk(hunk: &Hunk) -> Hunk {
    Hunk {
        old_start: hunk.new_start,
        old_len: hunk.new_len,
        new_start: hunk.old_start,
        new_len: hunk.old_len,
        lines: hunk
            .lines
            .iter()
            .map(|line| HunkLine {
                kind: match line.kind {
                    '+' => '-',
                    '-' => '+',
                    other => other,
                },
                text: line.text.clone(),
            })
            .collect(),
    }
}

fn old_content_from_deletion(hunks: &[Hunk]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for hunk in hunks {
        let at = hunk.old_start.saturating_sub(1).min(lines.len());
        let content: Vec<String> = hunk
            .lines
            .iter()
            .filter(|line| line.kind != '+')
            .map(|line| line.text.clone())
            .collect();
        lines.splice(at..at, content);
    }
    let mut output = lines.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    output
}

fn hunk_header(hunk: &HunkView, selected: bool, width: u16, theme: &Theme) -> Line<'static> {
    let palette = theme.palette;
    let style = if selected {
        Style::default()
            .bg(palette.selection_bg)
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.accent2)
    };
    let text = format!(
        "{} @@ -{},{} +{},{} @@",
        hunk.status.icon(),
        hunk.hunk.old_start,
        hunk.hunk.old_len,
        hunk.hunk.new_start,
        hunk.hunk.new_len
    );
    Line::from(Span::styled(truncate_chars(&text, width as usize), style))
}

#[allow(clippy::too_many_arguments)]
fn unified_body_line(
    old_no: Option<usize>,
    new_no: Option<usize>,
    sign: char,
    text: &str,
    path: &str,
    width: u16,
    x_scroll: usize,
    theme: &Theme,
    syntax: &SyntaxEngine,
    selected: bool,
) -> Line<'static> {
    let palette = theme.palette;
    let (sign_color, background) = match sign {
        '+' => (palette.success, Some(palette.added_bg)),
        '-' => (palette.error, Some(palette.removed_bg)),
        _ => (palette.dim, None),
    };
    let old = old_no
        .map(|number| format!("{number:>4}"))
        .unwrap_or_else(|| "    ".to_string());
    let new = new_no
        .map(|number| format!("{number:>4}"))
        .unwrap_or_else(|| "    ".to_string());
    let base = if selected {
        Style::default().bg(palette.selection_bg)
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::styled(format!("{old} {new} "), base.fg(palette.dim)),
        Span::styled(format!("{sign} "), base.fg(sign_color)),
    ];

    let available = width.saturating_sub(12) as usize;
    let visible = slice_chars(text, x_scroll, available);
    let highlighted = syntax.highlight(&visible, path, theme.mode);
    let mut used = 0usize;
    for (style, chunk) in highlighted {
        used += chunk.chars().count();
        let style = match background {
            Some(background) => style.bg(background),
            None => style,
        };
        spans.push(Span::styled(chunk, style));
    }
    if visible.chars().count() > used {
        let pad = " ".repeat(visible.chars().count() - used);
        spans.push(Span::styled(
            pad,
            match background {
                Some(background) => Style::default().bg(background),
                None => Style::default(),
            },
        ));
    }
    Line::from(spans)
}

#[allow(clippy::too_many_arguments)]
fn split_row(
    left: Option<(usize, char, &str)>,
    right: Option<(usize, char, &str)>,
    half_width: u16,
    path: &str,
    x_scroll: usize,
    theme: &Theme,
    syntax: &SyntaxEngine,
    selected: bool,
) -> Line<'static> {
    let palette = theme.palette;
    let base = if selected {
        Style::default().bg(palette.selection_bg)
    } else {
        Style::default()
    };
    let separator_style = base.fg(palette.border);
    let mut spans = side_spans(left, half_width, path, x_scroll, theme, syntax, base);
    spans.push(Span::styled("│", separator_style));
    spans.extend(side_spans(
        right, half_width, path, x_scroll, theme, syntax, base,
    ));
    Line::from(spans)
}

fn side_spans(
    side: Option<(usize, char, &str)>,
    width: u16,
    path: &str,
    x_scroll: usize,
    theme: &Theme,
    syntax: &SyntaxEngine,
    base: Style,
) -> Vec<Span<'static>> {
    let palette = theme.palette;
    let width = width as usize;
    let Some((number, sign, text)) = side else {
        return vec![Span::styled(" ".repeat(width), base)];
    };
    let (sign_color, background) = match sign {
        '+' => (palette.success, Some(palette.added_bg)),
        '-' => (palette.error, Some(palette.removed_bg)),
        _ => (palette.dim, None),
    };
    let prefix = format!("{number:>4} {sign} ");
    let available = width.saturating_sub(prefix.chars().count());
    let visible = slice_chars(text, x_scroll, available);
    let mut spans = vec![Span::styled(
        prefix.clone(),
        base.fg(if sign == ' ' { palette.dim } else { sign_color }),
    )];
    let highlighted = syntax.highlight(&visible, path, theme.mode);
    let mut used = 0usize;
    for (style, chunk) in highlighted {
        used += chunk.chars().count();
        let style = match background {
            Some(background) => style.bg(background),
            None => style,
        };
        spans.push(Span::styled(chunk, style));
    }
    let consumed = prefix.chars().count() + used;
    if consumed < width {
        spans.push(Span::styled(
            " ".repeat(width - consumed),
            match background {
                Some(background) => Style::default().bg(background),
                None => base,
            },
        ));
    }
    spans
}

/// Build a synthetic diff for a newly created file. Used by tests.
#[cfg(test)]
fn creation_diff(path: &str, content: &str) -> String {
    let lines = content.lines().count().max(1);
    let mut patch = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{lines} @@\n");
    for line in content.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;
    use std::path::Path;
    use tempfile::tempdir;

    fn context(root: &Path) -> ToolContext {
        ToolContext::from_config(root, &ToolsConfig::default()).unwrap()
    }

    #[test]
    fn ingests_and_selects_files() {
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n";
        let mut pane = DiffPane::new();
        assert_eq!(pane.ingest("apply_patch", patch).unwrap(), 2);
        assert_eq!(pane.files.len(), 2);
        assert_eq!(pane.selected_file, 1);
        assert_eq!(pane.files[1].path(), "b.txt");
        assert_eq!(pane.files[1].hunks.len(), 1);
    }

    #[test]
    fn accepts_hunks_and_files() {
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
        let mut pane = DiffPane::new();
        pane.ingest("edit_file", patch).unwrap();
        assert!(pane.accept_hunk().is_some());
        assert_eq!(pane.files[0].status, FileStatus::Accepted);
        assert_eq!(pane.files[0].hunks[0].status, HunkStatus::Accepted);
    }

    #[test]
    fn accepting_a_reverted_hunk_does_not_lie() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\nTWO\n").unwrap();
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";
        let mut pane = DiffPane::new();
        pane.ingest("edit_file", patch).unwrap();
        pane.reject_hunk(&context(dir.path())).unwrap();
        let message = pane.accept_hunk().unwrap();
        assert!(message.contains("reverted"));
        assert_eq!(pane.files[0].hunks[0].status, HunkStatus::Rejected);
        assert_eq!(pane.files[0].status, FileStatus::Rejected);
    }

    #[test]
    fn rejects_hunk_and_reverts_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\nTWO\nthree\n").unwrap();
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let mut pane = DiffPane::new();
        pane.ingest("edit_file", patch).unwrap();
        pane.reject_hunk(&context(dir.path())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\nthree\n");
        assert_eq!(pane.files[0].status, FileStatus::Rejected);
    }

    #[test]
    fn rejects_created_file_by_deleting_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.txt");
        std::fs::write(&path, "hi\n").unwrap();
        let mut pane = DiffPane::new();
        pane.ingest("write_file", &creation_diff("new.txt", "hi\n"))
            .unwrap();
        pane.reject_file(&context(dir.path())).unwrap();
        assert!(!path.exists());
        assert_eq!(pane.files[0].status, FileStatus::Rejected);
    }

    #[test]
    fn rejects_multi_hunk_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\nB\nc\nd\ne\nF\n").unwrap();
        let patch =
            "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -5,2 +5,2 @@\n e\n-f\n+F\n";
        let mut pane = DiffPane::new();
        pane.ingest("apply_patch", patch).unwrap();
        pane.reject_file(&context(dir.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\nb\nc\nd\ne\nf\n"
        );
    }

    #[test]
    fn rejects_deletion_by_recreating_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gone.txt");
        let patch = "--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n";
        let mut pane = DiffPane::new();
        pane.ingest("apply_patch", patch).unwrap();
        pane.reject_file(&context(dir.path())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn rejects_update_file_and_restores_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\nTWO\n").unwrap();
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n";
        let mut pane = DiffPane::new();
        pane.ingest("edit_file", patch).unwrap();
        pane.reject_file(&context(dir.path())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }
}
