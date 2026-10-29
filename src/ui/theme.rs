//! Monochrome theme + syntect-backed grayscale syntax highlighting.

use ratatui::style::{Color, Style};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub accent2: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub user: Color,
    pub tool: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub input_bg: Color,
    pub added_bg: Color,
    pub removed_bg: Color,
    pub status_bg: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub mode: ThemeMode,
    pub palette: Palette,
}

impl Theme {
    pub fn from_name(name: &str) -> Self {
        if name.to_ascii_lowercase().contains("light") {
            Self::light()
        } else {
            Self::dark()
        }
    }

    /// Black background, white/gray text. No hue anywhere.
    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            palette: Palette {
                bg: Color::Rgb(0, 0, 0),
                fg: Color::Rgb(245, 245, 245),
                dim: Color::Rgb(135, 135, 135),
                accent: Color::Rgb(255, 255, 255),
                accent2: Color::Rgb(195, 195, 195),
                success: Color::Rgb(235, 235, 235),
                error: Color::Rgb(255, 255, 255),
                warning: Color::Rgb(185, 185, 185),
                user: Color::Rgb(255, 255, 255),
                tool: Color::Rgb(185, 185, 185),
                border: Color::Rgb(60, 60, 60),
                border_focus: Color::Rgb(255, 255, 255),
                selection_bg: Color::Rgb(38, 38, 38),
                input_bg: Color::Rgb(10, 10, 10),
                added_bg: Color::Rgb(32, 32, 32),
                removed_bg: Color::Rgb(52, 52, 52),
                status_bg: Color::Rgb(16, 16, 16),
            },
        }
    }

    /// White background, black/gray text. Also strictly monochrome.
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            palette: Palette {
                bg: Color::Rgb(255, 255, 255),
                fg: Color::Rgb(10, 10, 10),
                dim: Color::Rgb(105, 105, 105),
                accent: Color::Rgb(0, 0, 0),
                accent2: Color::Rgb(65, 65, 65),
                success: Color::Rgb(20, 20, 20),
                error: Color::Rgb(0, 0, 0),
                warning: Color::Rgb(70, 70, 70),
                user: Color::Rgb(0, 0, 0),
                tool: Color::Rgb(70, 70, 70),
                border: Color::Rgb(200, 200, 200),
                border_focus: Color::Rgb(0, 0, 0),
                selection_bg: Color::Rgb(228, 228, 228),
                input_bg: Color::Rgb(245, 245, 245),
                added_bg: Color::Rgb(232, 232, 232),
                removed_bg: Color::Rgb(210, 210, 210),
                status_bg: Color::Rgb(240, 240, 240),
            },
        }
    }

    pub fn toggle(&mut self) {
        *self = match self.mode {
            ThemeMode::Dark => Self::light(),
            ThemeMode::Light => Self::dark(),
        };
    }

    pub fn text(&self) -> Style {
        Style::default().fg(self.palette.fg)
    }

    pub fn dim(&self) -> Style {
        Style::default().fg(self.palette.dim)
    }
}

type CacheKey = (String, String, bool);

/// Loads syntect defaults lazily and highlights individual diff lines.
/// Results are cached per (extension, text, mode) so redraws stay cheap.
pub struct SyntaxEngine {
    set: SyntaxSet,
    themes: ThemeSet,
    cache: Mutex<HashMap<CacheKey, Vec<(Style, String)>>>,
}

impl SyntaxEngine {
    pub fn new() -> Self {
        Self {
            set: SyntaxSet::load_defaults_newlines(),
            themes: ThemeSet::load_defaults(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn highlight(&self, text: &str, path: &str, mode: ThemeMode) -> Vec<(Style, String)> {
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_string();
        let key = (extension.clone(), text.to_string(), mode == ThemeMode::Dark);
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.get(&key)
        {
            return hit.clone();
        }

        let spans = self.highlight_uncached(text, &extension, mode);
        if let Ok(mut cache) = self.cache.lock() {
            if cache.len() > 8_192 {
                cache.clear();
            }
            cache.insert(key, spans.clone());
        }
        spans
    }

    fn highlight_uncached(
        &self,
        text: &str,
        extension: &str,
        mode: ThemeMode,
    ) -> Vec<(Style, String)> {
        let fallback = vec![(Style::default(), text.to_string())];
        let theme_name = match mode {
            ThemeMode::Dark => "base16-ocean.dark",
            ThemeMode::Light => "base16-ocean.light",
        };
        let Some(theme) = self
            .themes
            .themes
            .get(theme_name)
            .or_else(|| self.themes.themes.values().next())
        else {
            return fallback;
        };

        let syntax = self
            .set
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.set.find_syntax_plain_text());

        let mut highlighter = HighlightLines::new(syntax, theme);
        match highlighter.highlight_line(text, &self.set) {
            Ok(ranges) => {
                let spans: Vec<(Style, String)> = ranges
                    .into_iter()
                    .filter(|(_, text)| !text.is_empty())
                    .map(|(style, text)| {
                        (
                            Style::default().fg(mono_color(style.foreground, mode)),
                            text.to_string(),
                        )
                    })
                    .collect();
                if spans.is_empty() { fallback } else { spans }
            }
            Err(_) => fallback,
        }
    }
}

impl Default for SyntaxEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Collapse syntect's colors to white/gray (dark) or black/gray (light).
fn mono_color(color: syntect::highlighting::Color, mode: ThemeMode) -> Color {
    let luminance =
        (0.2126 * color.r as f32 + 0.7152 * color.g as f32 + 0.0722 * color.b as f32) / 255.0;
    let value = match mode {
        ThemeMode::Dark => {
            if luminance > 0.62 {
                245
            } else if luminance > 0.35 {
                185
            } else {
                120
            }
        }
        ThemeMode::Light => {
            if luminance < 0.30 {
                10
            } else if luminance < 0.58 {
                70
            } else {
                120
            }
        }
    };
    Color::Rgb(value, value, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_is_grayscale() {
        for theme in [Theme::dark(), Theme::light()] {
            for color in [
                theme.palette.bg,
                theme.palette.fg,
                theme.palette.dim,
                theme.palette.accent,
                theme.palette.error,
                theme.palette.success,
                theme.palette.warning,
                theme.palette.user,
                theme.palette.tool,
                theme.palette.selection_bg,
                theme.palette.added_bg,
                theme.palette.removed_bg,
            ] {
                if let Color::Rgb(r, g, b) = color {
                    assert_eq!(r, g);
                    assert_eq!(g, b);
                }
            }
        }
    }

    #[test]
    fn syntax_highlighting_is_cached_and_monochrome() {
        let engine = SyntaxEngine::new();
        let first = engine.highlight("fn main() {}", "a.rs", ThemeMode::Dark);
        let second = engine.highlight("fn main() {}", "a.rs", ThemeMode::Dark);
        assert_eq!(first.len(), second.len());
        for (style, text) in first {
            assert!(!text.is_empty());
            if let Some(color) = style.fg
                && let Color::Rgb(r, g, b) = color
            {
                assert_eq!(r, g);
                assert_eq!(g, b);
            }
        }
    }

    #[test]
    fn from_name_defaults_to_dark() {
        assert_eq!(Theme::from_name("minimal-dark").mode, ThemeMode::Dark);
        assert_eq!(Theme::from_name("mono-light").mode, ThemeMode::Light);
    }
}
