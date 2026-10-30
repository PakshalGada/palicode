//! Path permission patterns with anchors and negation.
//!
//! Supported anchors:
//! - `//path` — absolute filesystem path
//! - `~/path` — home-relative
//! - `./path` — workspace-relative
//! - plain relative paths — workspace-relative
//!
//! A leading `!` negates a pattern (excludes matching paths). Matching checks
//! both the lexical path and the canonical real path so symlinks cannot be used
//! to bypass a rule.

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathPattern {
    raw: String,
    negated: bool,
    matcher: GlobMatcher,
    /// Match against the absolute path instead of the workspace-relative path.
    absolute: bool,
    base: Option<PathBuf>,
}

impl PathPattern {
    pub fn compile(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("empty path pattern");
        }
        let (negated, raw) = match raw.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, raw),
        };
        let (pattern, absolute, base) = if let Some(rest) = raw.strip_prefix("//") {
            (format!("/{rest}"), true, None)
        } else if raw == "~" || raw.starts_with("~/") {
            let home = dirs::home_dir().context("cannot determine home directory")?;
            let joined = if raw == "~" {
                home.clone()
            } else {
                home.join(&raw[2..])
            };
            (joined.to_string_lossy().to_string(), true, Some(home))
        } else if let Some(rest) = raw.strip_prefix("./") {
            (rest.to_string(), false, None)
        } else {
            (raw.to_string(), false, None)
        };
        let matcher = GlobBuilder::new(&pattern)
            .literal_separator(false)
            .backslash_escape(false)
            .build()
            .with_context(|| format!("invalid path pattern '{raw}'"))?
            .compile_matcher();
        Ok(Self {
            raw: raw.to_string(),
            negated,
            matcher,
            absolute,
            base,
        })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn matches(&self, path: &Path, cwd: &Path) -> bool {
        let lexical = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let canonical = lexical.canonicalize().unwrap_or_else(|_| lexical.clone());
        let hit = self.matches_one(&lexical, cwd) || self.matches_one(&canonical, cwd);
        if self.negated { !hit } else { hit }
    }

    fn matches_one(&self, path: &Path, cwd: &Path) -> bool {
        if self.absolute {
            let value = path.to_string_lossy().replace('\\', "/");
            self.matcher.is_match(value)
        } else {
            let relative = path.strip_prefix(cwd).unwrap_or(path);
            let value = relative.to_string_lossy().replace('\\', "/");
            if self.matcher.is_match(&value) {
                return true;
            }
            // Also match against the basename for patterns such as `*.pem`.
            if let Some(name) = relative.file_name() {
                if self.matcher.is_match(name) {
                    return true;
                }
            }
            // And the absolute form, for patterns written that way.
            let absolute = path.to_string_lossy().replace('\\', "/");
            self.matcher.is_match(absolute)
        }
    }

    pub fn base(&self) -> Option<&Path> {
        self.base.as_deref()
    }
}

/// True when any component is a protected metadata directory.
pub fn is_protected(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name)
                if name == ".git" || name == ".palicode"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn relative_workspace_patterns() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let pattern = PathPattern::compile("./src/**/*.rs").unwrap();
        assert!(pattern.matches(&cwd.join("src/main.rs"), &cwd));
        assert!(!pattern.matches(&cwd.join("tests/main.rs"), &cwd));
    }

    #[test]
    fn supports_negation() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let pattern = PathPattern::compile("!./.env").unwrap();
        assert!(!pattern.matches(&cwd.join(".env"), &cwd));
        assert!(pattern.matches(&cwd.join("src/main.rs"), &cwd));
    }

    #[test]
    fn absolute_anchor() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let pattern = PathPattern::compile("//etc/**").unwrap();
        assert!(pattern.matches(Path::new("/etc/hosts"), &cwd));
        assert!(!pattern.matches(&cwd.join("etc/hosts"), &cwd));
    }

    #[test]
    fn detects_protected_metadata() {
        assert!(is_protected(Path::new(".git/config")));
        assert!(is_protected(Path::new("nested/.palicode/config.toml")));
        assert!(!is_protected(Path::new("src/.gitignore")));
    }
}
