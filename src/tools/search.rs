//! Search tools: `grep` (ripgrep when present, built-in fallback) and `glob`.

use super::{
    Tool, ToolContext, ToolOutput, optional_bool, optional_str, optional_usize, required_str,
    schema_object,
};
use crate::util::path::display_path;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;

const MAX_LINE_CHARS: usize = 400;
const MAX_FALLBACK_FILE_BYTES: u64 = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression (or fixed string). Respects .gitignore, \
         skips binaries, and returns 'path:line:column:text' matches. Use it to locate code before reading."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "pattern": {"type": "string", "description": "Regular expression, or literal text when fixed_strings=true."},
                "path": {"type": "string", "description": "File or directory to search. Defaults to the workspace root."},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. '**/*.rs' or '*.toml'."},
                "case_sensitive": {"type": "boolean", "description": "Default true."},
                "fixed_strings": {"type": "boolean", "description": "Treat pattern as literal text. Default false."},
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5000,
                    "description": "Maximum matches to return. Default 100."
                }
            }),
            &["pattern"],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern = required_str(&args, "pattern")?.to_string();
        if pattern.is_empty() {
            bail!("pattern must not be empty");
        }
        let base = match optional_str(&args, "path").filter(|path| !path.trim().is_empty()) {
            Some(path) => ctx.resolve(path)?,
            None => ctx.cwd.clone(),
        };
        if !base.exists() {
            bail!("{} does not exist", ctx.display(&base));
        }
        let glob = optional_str(&args, "glob")
            .filter(|glob| !glob.trim().is_empty())
            .map(str::to_string);
        let case_sensitive = optional_bool(&args, "case_sensitive", true)?;
        let fixed_strings = optional_bool(&args, "fixed_strings", false)?;
        let max_results = optional_usize(&args, "max_results")?
            .unwrap_or(100)
            .clamp(1, 5_000);

        let matcher = match &glob {
            Some(glob) => Some(build_glob(glob)?),
            None => None,
        };

        // Prefer ripgrep for speed and .gitignore fidelity.
        match search_with_rg(
            &pattern,
            &base,
            glob.as_deref(),
            case_sensitive,
            fixed_strings,
            max_results,
            &ctx.cwd,
        )
        .await
        {
            Ok(Some(results)) => {
                return Ok(ToolOutput::text(format_matches(
                    &pattern,
                    &results,
                    max_results,
                )));
            }
            Ok(None) => {}
            Err(err) => return Err(err),
        }

        let fallback = FallbackGrep {
            pattern: pattern.clone(),
            base,
            cwd: ctx.cwd.clone(),
            matcher,
            case_sensitive,
            fixed_strings,
            max_results,
        };
        let pattern = pattern.clone();
        let results = tokio::task::spawn_blocking(move || fallback.run())
            .await
            .context("grep worker failed")??;
        Ok(ToolOutput::text(format_matches(
            &pattern,
            &results,
            max_results,
        )))
    }
}

fn format_matches(pattern: &str, results: &[String], max_results: usize) -> String {
    if results.is_empty() {
        return format!("No matches for '{pattern}'");
    }
    let mut output = results.join("\n");
    if results.len() >= max_results {
        output.push_str(&format!("\n... (stopped at {max_results} matches)"));
    }
    output
}

struct FallbackGrep {
    pattern: String,
    base: PathBuf,
    cwd: PathBuf,
    matcher: Option<GlobMatcher>,
    case_sensitive: bool,
    fixed_strings: bool,
    max_results: usize,
}

impl FallbackGrep {
    fn run(self) -> Result<Vec<String>> {
        let pattern = if self.fixed_strings {
            regex::escape(&self.pattern)
        } else {
            self.pattern.clone()
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .build()
            .with_context(|| format!("invalid pattern '{}'", self.pattern))?;

        let mut results = Vec::new();
        let walker = WalkBuilder::new(&self.base)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
            .build();

        for entry in walker {
            let Ok(entry) = entry else { continue };
            if results.len() >= self.max_results {
                break;
            }
            let path = entry.path();
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(matcher) = &self.matcher {
                let relative = path.strip_prefix(&self.base).unwrap_or(path);
                if !glob_matches(matcher, relative) {
                    continue;
                }
            }
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.len() > MAX_FALLBACK_FILE_BYTES {
                continue;
            }
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if bytes.iter().take(8_000).any(|byte| *byte == 0) {
                continue;
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };

            let display = display_path(&self.cwd, path);
            for (index, line) in text.lines().enumerate() {
                let Some(found) = regex.find(line) else {
                    continue;
                };
                let column = line[..found.start()].chars().count() + 1;
                results.push(format!(
                    "{display}:{}:{}:{}",
                    index + 1,
                    column,
                    truncate_line(line)
                ));
                if results.len() >= self.max_results {
                    break;
                }
            }
        }

        Ok(results)
    }
}

async fn search_with_rg(
    pattern: &str,
    base: &Path,
    glob: Option<&str>,
    case_sensitive: bool,
    fixed_strings: bool,
    max_results: usize,
    cwd: &Path,
) -> Result<Option<Vec<String>>> {
    let mut command = Command::new("rg");
    command
        .arg("--json")
        .arg("--color")
        .arg("never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if !case_sensitive {
        command.arg("--ignore-case");
    }
    if fixed_strings {
        command.arg("--fixed-strings");
    }
    if let Some(glob) = glob {
        command.arg("--glob").arg(glob);
    }
    command.arg("--").arg(pattern).arg(base);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("cannot start ripgrep"),
    };

    let stdout = child
        .stdout
        .take()
        .context("cannot capture ripgrep output")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let mut results = Vec::new();
    let mut hit_limit = false;

    while let Some(line) = lines
        .next_line()
        .await
        .context("cannot read ripgrep output")?
    {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let Some(data) = value.get("data") else {
            continue;
        };
        let path = data
            .get("path")
            .and_then(|path| path.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
        let text = data
            .get("lines")
            .and_then(|lines| lines.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end_matches(['\n', '\r']);
        let start = data
            .get("submatches")
            .and_then(Value::as_array)
            .and_then(|submatches| submatches.first())
            .and_then(|submatch| submatch.get("start"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let column = text
            .get(..start)
            .map(|prefix| prefix.chars().count() + 1)
            .unwrap_or(1);

        results.push(format!(
            "{}:{line_number}:{column}:{}",
            display_path(cwd, Path::new(path)),
            truncate_line(text)
        ));
        if results.len() >= max_results {
            hit_limit = true;
            break;
        }
    }

    if hit_limit {
        let _ = child.kill().await;
    }

    let mut stderr = String::new();
    if let Some(mut stream) = child.stderr.take() {
        let _ = stream.read_to_string(&mut stderr).await;
    }
    let status = child.wait().await.context("cannot wait for ripgrep")?;

    if !hit_limit && !status.success() && status.code() != Some(1) {
        bail!("ripgrep failed: {}", stderr.trim());
    }
    Ok(Some(results))
}

fn truncate_line(line: &str) -> String {
    let trimmed = line.trim_end();
    if trimmed.chars().count() <= MAX_LINE_CHARS {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(MAX_LINE_CHARS).collect();
        format!("{cut}…")
    }
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files and directories by glob pattern (e.g. '**/*.rs', 'src/**/*.toml'). \
         Respects .gitignore and skips hidden entries by default. Results are sorted."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "pattern": {"type": "string", "description": "Glob pattern matched against paths relative to 'path'."},
                "path": {"type": "string", "description": "Directory to search. Defaults to the workspace root."},
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5000,
                    "description": "Maximum entries to return. Default 200."
                },
                "hidden": {"type": "boolean", "description": "Include dotfiles. Default false."}
            }),
            &["pattern"],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let pattern = required_str(&args, "pattern")?.to_string();
        if pattern.trim().is_empty() {
            bail!("pattern must not be empty");
        }
        let base = match optional_str(&args, "path").filter(|path| !path.trim().is_empty()) {
            Some(path) => ctx.resolve(path)?,
            None => ctx.cwd.clone(),
        };
        if !base.is_dir() {
            bail!("{} is not a directory", ctx.display(&base));
        }
        let limit = optional_usize(&args, "limit")?
            .unwrap_or(200)
            .clamp(1, 5_000);
        let hidden = optional_bool(&args, "hidden", false)?;
        let matcher = build_glob(&pattern)?;

        let cwd = ctx.cwd.clone();
        let results = tokio::task::spawn_blocking(move || {
            collect_globs(&base, &cwd, &matcher, limit, hidden, &pattern)
        })
        .await
        .context("glob worker failed")??;

        Ok(ToolOutput::text(results))
    }
}

fn collect_globs(
    base: &Path,
    cwd: &Path,
    matcher: &GlobMatcher,
    limit: usize,
    hidden: bool,
    pattern: &str,
) -> Result<String> {
    let mut matches = Vec::new();
    let walker = WalkBuilder::new(base)
        .hidden(!hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
        .build();

    for entry in walker {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry.path().strip_prefix(base).unwrap_or(entry.path());
        if !glob_matches(matcher, relative) {
            continue;
        }
        let mut display = display_path(cwd, entry.path());
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            display.push('/');
        }
        matches.push(display);
        if matches.len() >= limit {
            break;
        }
    }

    if matches.is_empty() {
        return Ok(format!("No files matching '{pattern}'"));
    }
    matches.sort();
    matches.dedup();
    let mut output = matches.join("\n");
    if matches.len() >= limit {
        output.push_str(&format!("\n... (stopped at {limit} entries)"));
    }
    Ok(output)
}

fn build_glob(pattern: &str) -> Result<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .with_context(|| format!("invalid glob pattern '{pattern}'"))
        .map(|glob| glob.compile_matcher())
}

fn glob_matches(matcher: &GlobMatcher, relative: &Path) -> bool {
    if matcher.is_match(relative) {
        return true;
    }
    // Friendly behaviour: "*.rs" should also match nested files.
    if !relative.to_string_lossy().contains('/') {
        false
    } else {
        relative
            .file_name()
            .map(|name| matcher.is_match(name))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolsConfig;
    use serde_json::json;
    use tempfile::tempdir;

    fn context(root: &Path) -> ToolContext {
        ToolContext::from_config(root, &ToolsConfig::default()).unwrap()
    }

    #[tokio::test]
    async fn grep_finds_matches() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/a.rs"),
            "fn main() {\n    let needle = 1;\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.txt"), "nothing here\n").unwrap();
        let ctx = context(dir.path());

        let output = Grep
            .execute(json!({"pattern": "needle"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("src/a.rs:2:"));
        assert!(output.content.contains("needle"));
        assert!(!output.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_respects_glob_and_fixed_strings() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "let x = a.b;\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "a.b\n").unwrap();
        let ctx = context(dir.path());

        let output = Grep
            .execute(
                json!({"pattern": "a.b", "fixed_strings": true, "glob": "**/*.rs"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(output.content.contains("a.rs:1:"));
        assert!(!output.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_reports_no_matches() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let ctx = context(dir.path());
        let output = Grep
            .execute(json!({"pattern": "absent"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.starts_with("No matches"));
    }

    #[tokio::test]
    async fn glob_finds_nested_files() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/nested/b.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/c.txt"), "").unwrap();
        let ctx = context(dir.path());

        let output = Glob
            .execute(json!({"pattern": "**/*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("src/a.rs"));
        assert!(output.content.contains("src/nested/b.rs"));
        assert!(!output.content.contains("c.txt"));
    }

    #[tokio::test]
    async fn glob_without_slash_matches_basenames() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
        let ctx = context(dir.path());

        let output = Glob
            .execute(json!({"pattern": "*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("src/a.rs"));
    }
}
