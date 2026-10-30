//! Project memory: instruction discovery, `@import` expansion, path-scoped
//! rules, `/init`, and the `# note` quick-capture command.
//!
//! Instruction precedence inside one directory is:
//! `PALICODE.md` > `AGENTS.md` > `CLAUDE.md`, plus any matching
//! `*.local.md` override file. Files are loaded from the workspace root down to
//! the current directory, so nearer files are more specific.

use crate::config::MemoryConfig;
use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub const INSTRUCTION_NAMES: [&str; 3] = ["PALICODE.md", "AGENTS.md", "CLAUDE.md"];
const MAX_MEMORY_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Rule {
    /// Stable identity (the rule file path).
    pub id: PathBuf,
    pub path: PathBuf,
    pub description: Option<String>,
    pub patterns: Vec<String>,
    pub body: String,
    /// Rules without `paths:` are loaded eagerly.
    pub always: bool,
    matchers: Vec<GlobMatcher>,
}

impl Rule {
    pub fn matches(&self, path: &Path, cwd: &Path) -> bool {
        if self.always {
            return true;
        }
        let relative = path.strip_prefix(cwd).unwrap_or(path);
        let relative = relative.to_string_lossy().replace('\\', "/");
        let absolute = path.to_string_lossy().replace('\\', "/");
        self.matchers
            .iter()
            .any(|matcher| matcher.is_match(&relative) || matcher.is_match(&absolute))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProjectMemory {
    pub cwd: PathBuf,
    pub files: Vec<MemoryFile>,
    pub rules: Vec<Rule>,
    pub warnings: Vec<String>,
    pub trusted: bool,
}

impl ProjectMemory {
    /// Discover memory with the trust state carried by the config.
    pub fn load(cwd: &Path, config: &MemoryConfig, trusted: bool) -> Self {
        let cwd = cwd
            .canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf());
        let mut memory = Self {
            cwd: cwd.clone(),
            files: Vec::new(),
            rules: Vec::new(),
            warnings: Vec::new(),
            trusted,
        };
        if !config.enabled {
            return memory;
        }

        let excluded = build_excludes(&config.exclude);
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for path in discover_instruction_files(&cwd) {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !seen.insert(canonical.clone()) {
                continue;
            }
            let content = expand_imports(
                &canonical,
                0,
                config.max_import_depth,
                &excluded,
                &cwd,
                &mut seen,
                &mut memory.warnings,
            );
            if content.trim().is_empty() {
                continue;
            }
            memory.files.push(MemoryFile {
                path: canonical,
                content,
            });
        }

        // Project rules are inert until the workspace is trusted.
        if trusted {
            for rule_path in discover_rule_files(&cwd) {
                match parse_rule(&rule_path, &cwd) {
                    Ok(Some(rule)) => memory.rules.push(rule),
                    Ok(None) => {}
                    Err(err) => memory
                        .warnings
                        .push(format!("{}: {err:#}", rule_path.display())),
                }
            }
        }

        memory
    }

    /// Load without a trust record. Kept for tests and for callers that have
    /// already performed their own trust decision.
    pub fn load_trusted(cwd: &Path, config: &MemoryConfig) -> Self {
        Self::load(cwd, config, true)
    }

    pub fn always_rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter(|rule| rule.always)
    }

    pub fn lazy_rule_count(&self) -> usize {
        self.rules.iter().filter(|rule| !rule.always).count()
    }

    /// Render the static portion of memory for the system prompt. Path-scoped
    /// rules are deliberately absent; they are injected when a matching file
    /// is touched.
    pub fn render_prompt(&self) -> String {
        let mut sections = Vec::new();
        if !self.files.is_empty() {
            let mut text = String::from(
                "<project-memory>\nThese instructions come from the workspace and are authoritative for this project.\n",
            );
            for file in &self.files {
                text.push_str(&format!(
                    "\n### {}\n{}\n",
                    file.path.display(),
                    file.content.trim()
                ));
            }
            text.push_str("</project-memory>");
            sections.push(text);
        }
        let always: Vec<&Rule> = self.always_rules().collect();
        if !always.is_empty() {
            let mut text = String::from("<project-rules>\nAlways-on project rules:\n");
            for rule in always {
                if let Some(description) = &rule.description {
                    text.push_str(&format!("\n### {description}\n"));
                } else {
                    text.push_str(&format!("\n### {}\n", rule.path.display()));
                }
                text.push_str(rule.body.trim());
                text.push('\n');
            }
            text.push_str("</project-rules>");
            sections.push(text);
        }
        sections.join("\n\n")
    }

    /// Rule indexes matching any of the supplied paths.
    pub fn matching_rules(&self, paths: &[PathBuf]) -> Vec<usize> {
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| {
                !rule.always
                    && paths
                        .iter()
                        .any(|path| rule.matches(path, &self.cwd))
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("workspace: {}", self.cwd.display()));
        lines.push(format!(
            "instructions: {} file(s)",
            self.files.len()
        ));
        for file in &self.files {
            lines.push(format!(
                "  {} ({} bytes)",
                file.path.display(),
                file.content.len()
            ));
        }
        let always = self.rules.iter().filter(|rule| rule.always).count();
        let lazy = self.lazy_rule_count();
        lines.push(format!("rules: {always} always-on, {lazy} path-scoped"));
        for rule in &self.rules {
            let scope = if rule.always {
                "always".to_string()
            } else {
                rule.patterns.join(", ")
            };
            let description = rule.description.as_deref().unwrap_or("(no description)");
            lines.push(format!(
                "  {} [{scope}] {description}",
                rule.path.display()
            ));
        }
        if !self.trusted {
            lines.push(
                "project rules/config are ignored until this workspace is trusted (run `palicode trust`)"
                    .to_string(),
            );
        }
        for warning in &self.warnings {
            lines.push(format!("warning: {warning}"));
        }
        lines.join("\n")
    }
}

fn build_excludes(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = GlobBuilder::new(pattern)
            .literal_separator(false)
            .backslash_escape(false)
            .build()
        {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

fn discover_instruction_files(cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dirs: Vec<&Path> = cwd.ancestors().collect();
    dirs.reverse();
    for dir in dirs {
        // One primary instruction file per directory, in priority order.
        if let Some(primary) = INSTRUCTION_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
        {
            found.push(primary.clone());
            if let Some(stem) = primary.file_stem().and_then(|stem| stem.to_str()) {
                let local = dir.join(format!("{stem}.local.md"));
                if local.is_file() {
                    found.push(local);
                }
            }
        }
        // Also pick up an explicit local override even when there is no primary.
        for name in INSTRUCTION_NAMES {
            let local = dir.join(name.replace(".md", ".local.md"));
            if local.is_file() && !found.contains(&local) {
                found.push(local);
            }
        }
    }
    found
}

fn discover_rule_files(cwd: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut ancestors: Vec<&Path> = cwd.ancestors().collect();
    ancestors.reverse();
    for dir in ancestors {
        let rules_dir = dir.join(".palicode").join("rules");
        let Ok(entries) = std::fs::read_dir(&rules_dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
            })
            .collect();
        paths.sort();
        files.extend(paths);
    }
    files
}

#[allow(clippy::too_many_arguments)]
fn expand_imports(
    path: &Path,
    depth: usize,
    max_depth: usize,
    excluded: &GlobSet,
    cwd: &Path,
    seen: &mut HashSet<PathBuf>,
    warnings: &mut Vec<String>,
) -> String {
    let Ok(metadata) = std::fs::metadata(path) else {
        warnings.push(format!("cannot read memory file {}", path.display()));
        return String::new();
    };
    if metadata.len() > MAX_MEMORY_BYTES {
        warnings.push(format!(
            "memory file {} is larger than {} bytes; skipped",
            path.display(),
            MAX_MEMORY_BYTES
        ));
        return String::new();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            warnings.push(format!("cannot read {}: {err}", path.display()));
            return String::new();
        }
    };

    let mut output = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            output.push_str(line);
            output.push('\n');
            continue;
        }
        if in_fence {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        let masked = mask_inline_code(line);
        let imports = find_imports(&masked);
        if imports.is_empty() {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        let only_import = imports.len() == 1
            && line[..imports[0].0].trim().is_empty()
            && line[imports[0].1..].trim().is_empty();
        if !only_import {
            output.push_str(line);
            output.push('\n');
        }
        for (_, end, raw) in imports {
            let import = raw.trim().trim_matches('"').trim_matches('\'');
            if import.is_empty() {
                continue;
            }
            let target = resolve_import(path, import);
            if is_excluded(&target, cwd, excluded) {
                continue;
            }
            if depth + 1 > max_depth {
                warnings.push(format!(
                    "{}: import depth exceeds {max_depth}; skipped @{}",
                    path.display(),
                    import
                ));
                continue;
            }
            let canonical = target.canonicalize().unwrap_or_else(|_| target.clone());
            if !canonical.is_file() {
                warnings.push(format!(
                    "{}: imported file not found: {}",
                    path.display(),
                    target.display()
                ));
                continue;
            }
            if !seen.insert(canonical.clone()) {
                continue;
            }
            output.push_str(&format!("<!-- imported from {} -->\n", target.display()));
            let nested = expand_imports(
                &canonical,
                depth + 1,
                max_depth,
                excluded,
                cwd,
                seen,
                warnings,
            );
            output.push_str(&nested);
            if !nested.ends_with('\n') {
                output.push('\n');
            }
            let _ = end;
        }
    }
    output
}

fn mask_inline_code(line: &str) -> String {
    let mut masked = String::with_capacity(line.len());
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_code = !in_code;
            masked.push(' ');
        } else if in_code {
            masked.push(' ');
        } else {
            masked.push(ch);
        }
    }
    masked
}

/// Returns `(start, end, path)` for each `@import` token outside code spans.
fn find_imports(masked: &str) -> Vec<(usize, usize, String)> {
    let bytes = masked.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'@' {
            index += 1;
            continue;
        }
        let preceded_ok = index == 0
            || matches!(
                bytes[index - 1],
                b' ' | b'\t' | b'\n' | b'(' | b'[' | b'"' | b'\''
            );
        if !preceded_ok {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        let mut end = index;
        while end < bytes.len()
            && !matches!(
                bytes[end],
                b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' | b'[' | b']' | b',' | b';'
            )
        {
            end += 1;
        }
        if end > start + 1 {
            found.push((
                start,
                end,
                masked[start + 1..end].trim().to_string(),
            ));
        }
        index = end;
    }
    found
}

fn resolve_import(importing: &Path, raw: &str) -> PathBuf {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return if raw == "~" {
                home
            } else {
                home.join(&raw[2..])
            };
        }
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        importing
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

fn is_excluded(path: &Path, cwd: &Path, excluded: &GlobSet) -> bool {
    let absolute = path.to_string_lossy().replace('\\', "/");
    let relative = path
        .strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if path.components().any(|component| {
        matches!(component, Component::Normal(name) if name == ".git" || name == "node_modules")
    }) {
        return true;
    }
    excluded.is_match(&absolute) || excluded.is_match(&relative)
}

fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(rest) = text.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        let front = rest[..end].trim_start_matches(['\r', '\n']);
        let body_start = end + 4;
        return (Some(front), text[body_start..].trim_start_matches(['\r', '\n']));
    }
    (None, text)
}

fn parse_rule(path: &Path, cwd: &Path) -> Result<Option<Rule>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read rule {}", path.display()))?;
    let (front, body) = split_frontmatter(&text);
    let Some(front) = front else {
        return Ok(Some(Rule {
            id: path.to_path_buf(),
            path: path.to_path_buf(),
            description: None,
            patterns: Vec::new(),
            body: body.trim().to_string(),
            always: true,
            matchers: Vec::new(),
        }));
    };

    let fields = parse_frontmatter(front);
    let mut patterns = Vec::new();
    for key in ["paths", "path", "globs", "glob"] {
        if let Some(values) = fields.get(key) {
            patterns.extend(values.iter().cloned());
        }
    }
    patterns.retain(|pattern| !pattern.trim().is_empty());
    let always = fields
        .get("alwaysapply")
        .or_else(|| fields.get("always"))
        .map(|values| values.iter().any(|value| value.eq_ignore_ascii_case("true")))
        .unwrap_or(false)
        || patterns.is_empty();
    let description = fields
        .get("description")
        .and_then(|values| values.first())
        .cloned();

    let mut matchers = Vec::new();
    for pattern in &patterns {
        let normalized = normalize_rule_pattern(pattern);
        let matcher = GlobBuilder::new(&normalized)
            .literal_separator(false)
            .backslash_escape(false)
            .build()
            .with_context(|| format!("invalid path glob '{pattern}'"))?
            .compile_matcher();
        matchers.push(matcher);
    }

    // Keep relative rule paths stable for summaries.
    let display_path = path
        .strip_prefix(cwd)
        .unwrap_or(path)
        .to_path_buf();
    Ok(Some(Rule {
        id: path.to_path_buf(),
        path: display_path,
        description,
        patterns,
        body: body.trim().to_string(),
        always,
        matchers,
    }))
}

fn normalize_rule_pattern(pattern: &str) -> String {
    let mut pattern = pattern.trim().to_string();
    if let Some(rest) = pattern.strip_prefix("./") {
        pattern = rest.to_string();
    }
    if pattern.starts_with('/') && !pattern.starts_with("//") {
        pattern = pattern.trim_start_matches('/').to_string();
    }
    if pattern.ends_with('/') {
        pattern.push_str("**");
    } else if !pattern.contains(['*', '?', '[', '{']) {
        pattern.push_str("/**");
    }
    pattern
}

fn parse_frontmatter(front: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut fields: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut current: Option<String> = None;
    for line in front.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            if let Some(key) = &current {
                fields
                    .entry(key.clone())
                    .or_default()
                    .push(unquote(item.trim()));
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            current = Some(key);
            continue;
        }
        current = None;
        let values = if value.starts_with('[') && value.ends_with(']') {
            value[1..value.len() - 1]
                .split(',')
                .map(|item| unquote(item.trim()))
                .filter(|item| !item.is_empty())
                .collect()
        } else {
            vec![unquote(value)]
        };
        fields.insert(key, values);
    }
    fields
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

// -- `/init`, notes, and summaries -----------------------------------------

#[derive(Debug, Clone)]
pub struct InitReport {
    pub path: PathBuf,
    pub bytes: usize,
}

/// Analyze the repository and write a usable `PALICODE.md`. Refuses to
/// overwrite an existing file unless `force` is set.
pub fn init_memory(cwd: &Path, force: bool) -> Result<InitReport> {
    let path = cwd.join("PALICODE.md");
    if path.exists() && !force {
        bail!(
            "{} already exists (use /init --force to overwrite)",
            path.display()
        );
    }
    let content = render_init(cwd)?;
    std::fs::write(&path, &content)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(InitReport {
        path,
        bytes: content.len(),
    })
}

fn render_init(cwd: &Path) -> Result<String> {
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let (stack, commands) = detect_stack(cwd);
    let overview = readme_summary(cwd).unwrap_or_else(|| {
        format!("`{name}` is a software project. Describe its purpose, users, and the main entry points here.")
    });
    let layout = directory_layout(cwd);
    let branch = git_branch(cwd);

    let mut out = String::new();
    out.push_str(&format!("# {name}\n\n"));
    out.push_str("<!-- Generated by `/init`. Keep this file current; palicode loads it automatically. -->\n\n");
    out.push_str("## Overview\n\n");
    out.push_str(overview.trim());
    out.push_str("\n\n## Stack\n\n");
    out.push_str(&format!("- {stack}\n"));
    if let Some(branch) = branch {
        out.push_str(&format!("- Git branch: `{branch}`\n"));
    }
    out.push_str("\n## Commands\n\n");
    out.push_str("```bash\n");
    for (label, command) in commands {
        out.push_str(&format!("# {label}\n{command}\n"));
    }
    out.push_str("```\n\n");
    out.push_str("## Layout\n\n");
    for entry in layout {
        out.push_str(&format!("- `{entry}`\n"));
    }
    out.push_str(
        "\n## Conventions\n\n\
         - Read a file before editing it; use `grep`/`glob` to find code instead of guessing paths.\n\
         - Prefer small, focused changes and run the relevant tests before claiming success.\n\
         - Do not commit generated build output or secrets.\n\
         - Add a `# note` or update this file when you learn a durable project fact.\n\n\
         ## Notes\n\n\
         - Add short, durable facts here. Path-scoped rules belong in `.palicode/rules/*.md`.\n",
    );
    Ok(out)
}

fn readme_summary(cwd: &Path) -> Option<String> {
    for name in ["README.md", "README", "readme.md"] {
        let path = cwd.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut paragraph = String::new();
        let mut started = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                if started {
                    break;
                }
                continue;
            }
            if trimmed.is_empty() {
                if started {
                    break;
                }
                continue;
            }
            if trimmed.starts_with("```") || trimmed.starts_with("![") || trimmed.starts_with("[![") {
                continue;
            }
            started = true;
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(trimmed);
            if paragraph.chars().count() > 700 {
                break;
            }
        }
        if !paragraph.trim().is_empty() {
            return Some(paragraph);
        }
    }
    None
}

fn detect_stack(cwd: &Path) -> (String, Vec<(&'static str, &'static str)>) {
    if cwd.join("Cargo.toml").is_file() {
        return (
            "Rust (Cargo)".to_string(),
            vec![
                ("format", "cargo fmt --all"),
                ("lint", "cargo clippy --all-targets -- -D warnings"),
                ("test", "cargo test"),
                ("build", "cargo build"),
            ],
        );
    }
    if cwd.join("package.json").is_file() {
        let mut commands = vec![("install", "npm install"), ("test", "npm test")];
        if let Ok(text) = std::fs::read_to_string(cwd.join("package.json")) {
            if text.contains("\"build\"") {
                commands.push(("build", "npm run build"));
            }
            if text.contains("\"lint\"") {
                commands.push(("lint", "npm run lint"));
            }
        }
        return ("JavaScript/TypeScript (Node)".to_string(), commands);
    }
    if cwd.join("pyproject.toml").is_file()
        || cwd.join("requirements.txt").is_file()
        || cwd.join("setup.py").is_file()
    {
        return (
            "Python".to_string(),
            vec![
                ("test", "pytest"),
                ("install", "pip install -e ."),
            ],
        );
    }
    if cwd.join("go.mod").is_file() {
        return (
            "Go".to_string(),
            vec![("test", "go test ./..."), ("build", "go build ./...")],
        );
    }
    if cwd.join("Makefile").is_file() {
        return (
            "Make".to_string(),
            vec![("test", "make test"), ("build", "make")],
        );
    }
    (
        "Unknown — update this section with the toolchain.".to_string(),
        vec![("test", "replace with the project's test command")],
    )
}

fn directory_layout(cwd: &Path) -> Vec<String> {
    let mut entries: Vec<String> = std::fs::read_dir(cwd)
        .map(|read| {
            read.flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if matches!(
                        name.as_str(),
                        ".git" | "target" | "node_modules" | ".venv" | "dist" | "build"
                    ) || (name.starts_with('.') && name != ".github")
                    {
                        return None;
                    }
                    let suffix = if entry.path().is_dir() { "/" } else { "" };
                    Some(format!("{name}{suffix}"))
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    entries.truncate(40);
    entries
}

fn git_branch(cwd: &Path) -> Option<String> {
    let head = cwd.join(".git").join("HEAD");
    let text = std::fs::read_to_string(head).ok()?;
    text.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// Append a durable note to `PALICODE.md` (creating the file when needed).
pub fn append_note(cwd: &Path, note: &str) -> Result<PathBuf> {
    let note = note.trim();
    if note.is_empty() {
        bail!("note must not be empty");
    }
    let path = cwd.join("PALICODE.md");
    let mut text = if path.is_file() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?
    } else {
        let name = cwd
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        format!("# {name}\n\n## Notes\n\n")
    };
    if !text.contains("## Notes") {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\n## Notes\n\n");
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("- {note}\n"));
    std::fs::write(&path, text)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

/// Resolve tool arguments to workspace paths for lazy rule matching.
pub fn paths_for_tool(name: &str, args: &serde_json::Value, cwd: &Path) -> Vec<PathBuf> {
    let string = |key: &str| args.get(key).and_then(serde_json::Value::as_str);
    let mut paths = Vec::new();
    match name {
        "read_file" | "write_file" | "edit_file" | "list_dir" | "grep" | "glob" | "git_diff"
        | "git_status" => {
            if let Some(path) = string("path").filter(|value| !value.trim().is_empty()) {
                paths.push(resolve(cwd, path));
            }
        }
        "apply_patch" => {
            if let Some(patch) = string("patch") {
                for line in patch.lines() {
                    let header = line
                        .strip_prefix("+++ ")
                        .or_else(|| line.strip_prefix("--- "));
                    let Some(header) = header else { continue };
                    let path = header
                        .split('\t')
                        .next()
                        .unwrap_or(header)
                        .trim()
                        .trim_matches('"');
                    if path == "/dev/null" || path.is_empty() {
                        continue;
                    }
                    let path = path
                        .strip_prefix("a/")
                        .or_else(|| path.strip_prefix("b/"))
                        .unwrap_or(path);
                    paths.push(resolve(cwd, path));
                }
            }
        }
        _ => {}
    }
    paths.sort();
    paths.dedup();
    paths
}

fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryConfig;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_priority_and_local_overrides() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root.join("PALICODE.md"), "root instructions");
        write(&root.join("AGENTS.md"), "should not win");
        write(&root.join("PALICODE.local.md"), "local override");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        write(&nested.join("CLAUDE.md"), "nested instructions");

        let memory = ProjectMemory::load_trusted(&nested, &MemoryConfig::default());
        let joined = memory.render_prompt();
        assert!(joined.contains("root instructions"));
        assert!(!joined.contains("should not win"));
        assert!(joined.contains("local override"));
        assert!(joined.contains("nested instructions"));
    }

    #[test]
    fn expands_imports_but_not_inside_code() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root.join("PALICODE.md"), "start\n@extra.md\n```\n@ignored.md\n```\nend");
        write(&root.join("extra.md"), "imported text");
        write(&root.join("ignored.md"), "must not appear");

        let memory = ProjectMemory::load_trusted(&root, &MemoryConfig::default());
        let text = memory.render_prompt();
        assert!(text.contains("imported text"));
        assert!(!text.contains("must not appear"));
        assert!(text.contains("@ignored.md"));
    }

    #[test]
    fn import_depth_is_limited() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for index in 0..6 {
            let next = if index == 5 {
                String::new()
            } else {
                format!("@file{}.md", index + 1)
            };
            write(
                &root.join(format!("file{index}.md")),
                &format!("level{index}\n{next}"),
            );
        }
        write(&root.join("PALICODE.md"), "@file0.md");
        let mut config = MemoryConfig::default();
        config.max_import_depth = 2;
        let memory = ProjectMemory::load_trusted(&root, &config);
        let text = memory.render_prompt();
        assert!(text.contains("level0"));
        assert!(text.contains("level2"));
        assert!(!text.contains("level3"));
    }

    #[test]
    fn parses_path_scoped_rules() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(
            &root.join(".palicode/rules/rust.md"),
            "---\ndescription: Rust files\npaths:\n  - src/**/*.rs\n---\nUse rustfmt.",
        );
        write(
            &root.join(".palicode/rules/always.md"),
            "---\ndescription: Always\n---\nBe concise.",
        );
        let memory = ProjectMemory::load_trusted(&root, &MemoryConfig::default());
        assert_eq!(memory.rules.len(), 2);
        assert!(memory.always_rules().any(|rule| rule.body.contains("Be concise")));
        assert!(memory.render_prompt().contains("Be concise"));
        assert!(!memory.render_prompt().contains("Use rustfmt"));

        let matches = memory.matching_rules(&[root.join("src/lib.rs")]);
        assert_eq!(matches.len(), 1);
        assert!(memory.rules[matches[0]].body.contains("rustfmt"));
        assert!(memory
            .matching_rules(&[root.join("README.md")])
            .is_empty());
    }

    #[test]
    fn init_writes_usable_memory() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root.join("Cargo.toml"), "[package]\nname = \"demo\"\n");
        write(&root.join("README.md"), "# Demo\n\nA useful demo project.\n");
        write(&root.join("src/main.rs"), "fn main() {}");
        let report = init_memory(&root, false).unwrap();
        let text = std::fs::read_to_string(report.path).unwrap();
        assert!(text.contains("# "));
        assert!(text.contains("A useful demo project."));
        assert!(text.contains("cargo test"));
        assert!(text.contains("src/"));
        assert!(init_memory(&root, false).is_err());
    }

    #[test]
    fn appends_notes() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write(&root.join("PALICODE.md"), "# Demo\n\n## Notes\n\n- existing\n");
        append_note(&root, "tests use a fixture server").unwrap();
        let text = std::fs::read_to_string(root.join("PALICODE.md")).unwrap();
        assert!(text.contains("- tests use a fixture server"));
        assert!(text.contains("- existing"));
    }

    #[test]
    fn resolves_tool_paths_for_rules() {
        let dir = tempdir().unwrap();
        let paths = paths_for_tool(
            "read_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            dir.path(),
        );
        assert_eq!(paths, vec![dir.path().join("src/lib.rs")]);
        let patch = paths_for_tool(
            "apply_patch",
            &serde_json::json!({"patch": "--- a/a.rs\n+++ b/a.rs\n"}),
            dir.path(),
        );
        assert_eq!(patch, vec![dir.path().join("a.rs")]);
    }
}
