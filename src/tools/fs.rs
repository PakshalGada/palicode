//! Filesystem tools: read, write, edit, apply_patch, list_dir.

use super::patch::{apply_hunks, parse_unified_patch};
use super::{
    Tool, ToolContext, ToolOutput, optional_bool, optional_str, optional_u64, optional_usize,
    required_str, schema_object,
};
use crate::util::text::{one_line, truncate_middle};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use similar::TextDiff;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const BINARY_SNIFF_BYTES: usize = 8_000;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace. Returns line-numbered content. \
         Use offset/limit for large files. Read a file before editing it."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "path": {
                    "type": "string",
                    "description": "File path, relative to the workspace (absolute paths inside the sandbox also work)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number to start at. Default 1."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10000,
                    "description": "Maximum number of lines to return. Defaults to tools.max_read_lines."
                }
            }),
            &["path"],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path = ctx.resolve(required_str(&args, "path")?)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("cannot stat {}", ctx.display(&path)))?;
        if metadata.is_dir() {
            bail!(
                "{} is a directory; use list_dir instead",
                ctx.display(&path)
            );
        }
        if metadata.len() > MAX_FILE_BYTES {
            bail!(
                "{} is too large ({} bytes, limit {MAX_FILE_BYTES}); narrow it down with grep",
                ctx.display(&path),
                metadata.len()
            );
        }

        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("cannot read {}", ctx.display(&path)))?;
        if looks_binary(&bytes) {
            bail!("{} appears to be a binary file", ctx.display(&path));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(ToolOutput::text(format!("{} is empty", ctx.display(&path))));
        }

        let start = optional_u64(&args, "offset")?.unwrap_or(1).max(1) as usize;
        let limit = optional_usize(&args, "limit")?
            .unwrap_or(ctx.max_read_lines)
            .clamp(1, 10_000);

        if start > total {
            bail!(
                "offset {start} is past the end of {} ({total} lines)",
                ctx.display(&path)
            );
        }

        let end = start.saturating_sub(1).saturating_add(limit).min(total);
        let mut output = String::new();
        for (index, line) in lines[start - 1..end].iter().enumerate() {
            let number = start + index;
            writeln!(&mut output, "{number:>6}\t{line}")?;
        }
        if end < total {
            writeln!(
                &mut output,
                "... ({} more lines; continue with offset={})",
                total - end,
                end + 1
            )?;
        }

        Ok(ToolOutput::text(truncate_middle(
            &output,
            ctx.max_output_bytes,
        )))
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a text file with the given content. Parent directories are created \
         by default. For targeted changes prefer edit_file or apply_patch."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "path": {"type": "string", "description": "File path, relative to the workspace."},
                "content": {"type": "string", "description": "Complete file content."},
                "create_dirs": {
                    "type": "boolean",
                    "description": "Create missing parent directories. Default true."
                }
            }),
            &["path", "content"],
        )
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path = ctx.resolve(required_str(&args, "path")?)?;
        let content = required_str(&args, "content")?;
        let create_dirs = optional_bool(&args, "create_dirs", true)?;

        if path.is_dir() {
            bail!("{} is a directory", ctx.display(&path));
        }
        let existed = path.exists();

        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            if create_dirs {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("cannot create {}", ctx.display(parent)))?;
            } else {
                bail!(
                    "parent directory {} does not exist (set create_dirs=true)",
                    ctx.display(parent)
                );
            }
        }

        tokio::fs::write(&path, content.as_bytes())
            .await
            .with_context(|| format!("cannot write {}", ctx.display(&path)))?;

        let lines = content.lines().count();
        Ok(ToolOutput::text(format!(
            "{} {} ({} bytes, {} lines)",
            if existed { "Overwrote" } else { "Created" },
            ctx.display(&path),
            content.len(),
            lines
        )))
    }
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace an exact string in a text file. old_string must match exactly and uniquely \
         (including whitespace); pass replace_all=true to change every occurrence. Returns a diff."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "path": {"type": "string", "description": "File path, relative to the workspace."},
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace. Include enough surrounding context to be unique."
                },
                "new_string": {"type": "string", "description": "Replacement text (may be empty to delete)."},
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique match. Default false."
                }
            }),
            &["path", "old_string", "new_string"],
        )
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let path = ctx.resolve(required_str(&args, "path")?)?;
        let old_string = required_str(&args, "old_string")?;
        let new_string = required_str(&args, "new_string")?;
        let replace_all = optional_bool(&args, "replace_all", false)?;

        if old_string.is_empty() {
            bail!("old_string must not be empty");
        }
        if old_string == new_string {
            bail!("old_string and new_string are identical");
        }

        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("cannot read {}", ctx.display(&path)))?;
        if looks_binary(&bytes) {
            bail!("{} appears to be a binary file", ctx.display(&path));
        }
        let original = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", ctx.display(&path)))?;

        let occurrences = original.matches(old_string).count();
        if occurrences == 0 {
            bail!(
                "old_string was not found in {} (looked for: {})",
                ctx.display(&path),
                one_line(old_string, 120)
            );
        }
        if occurrences > 1 && !replace_all {
            bail!(
                "old_string matches {occurrences} places in {}; add context or set replace_all=true",
                ctx.display(&path)
            );
        }

        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(&path, updated.as_bytes())
            .await
            .with_context(|| format!("cannot write {}", ctx.display(&path)))?;

        let display = ctx.display(&path);
        let diff = TextDiff::from_lines(&original, &updated);
        let mut added = 0usize;
        let mut removed = 0usize;
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => added += 1,
                similar::ChangeTag::Delete => removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }

        let unified = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{display}"), &format!("b/{display}"))
            .to_string();

        let mut output = format!(
            "Edited {display} (+{added} -{removed}, {occurrences} match{})\n{}",
            if occurrences == 1 { "" } else { "es" },
            truncate_middle(&unified, 8_000)
        );
        if output.len() > ctx.max_output_bytes {
            output = truncate_middle(&output, ctx.max_output_bytes);
        }
        Ok(ToolOutput::text(output))
    }
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> &'static str {
        "Apply a unified diff to one or more workspace files. Supports updates, new files \
         (--- /dev/null) and deletions (+++ /dev/null). Hunks are matched by their context, \
         so the patch must be based on the current file contents. Prefer this for multi-file edits."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "patch": {
                    "type": "string",
                    "description": "Unified diff text with '--- ' / '+++ ' headers and @@ hunks. Paths are relative to the workspace."
                }
            }),
            &["patch"],
        )
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let patch_text = required_str(&args, "patch")?;
        let files = parse_unified_patch(patch_text)?;

        enum Action {
            Create(String),
            Update(String),
            Delete,
        }

        struct Pending {
            path: PathBuf,
            display: String,
            hunks: usize,
            action: Action,
        }

        let mut pending = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for file in files {
            let (raw_target, action_kind) = match (&file.old_path, &file.new_path) {
                (None, Some(new)) => (new.as_str(), "create"),
                (Some(old), None) => (old.as_str(), "delete"),
                (Some(old), Some(new)) => {
                    if old != new {
                        bail!("renames are not supported by apply_patch ({old} -> {new})");
                    }
                    (new.as_str(), "update")
                }
                (None, None) => bail!("patch has neither an old nor a new path"),
            };

            let path = ctx.resolve(raw_target)?;
            if !seen.insert(path.clone()) {
                bail!("patch touches {} more than once", ctx.display(&path));
            }
            let display = ctx.display(&path);
            let hunks = file.hunks.len();

            match action_kind {
                "create" => {
                    if path.exists() {
                        bail!("cannot create {display}: it already exists");
                    }
                    let content = apply_hunks("", &file.hunks)?;
                    pending.push(Pending {
                        path,
                        display,
                        hunks,
                        action: Action::Create(content),
                    });
                }
                "delete" => {
                    let original = read_utf8(&path).await?;
                    let result = apply_hunks(&original, &file.hunks)
                        .with_context(|| format!("cannot apply deletion patch to {display}"))?;
                    if !result.trim().is_empty() {
                        bail!(
                            "deletion patch for {display} did not remove the whole file; refusing to delete"
                        );
                    }
                    pending.push(Pending {
                        path,
                        display,
                        hunks,
                        action: Action::Delete,
                    });
                }
                _ => {
                    let original = read_utf8(&path).await?;
                    let content = apply_hunks(&original, &file.hunks)
                        .with_context(|| format!("cannot apply patch to {display}"))?;
                    pending.push(Pending {
                        path,
                        display,
                        hunks,
                        action: Action::Update(content),
                    });
                }
            }
        }

        let mut report = String::from("Applied patch:\n");
        for item in pending {
            match item.action {
                Action::Create(content) => {
                    if let Some(parent) = item.path.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .with_context(|| format!("cannot create {}", ctx.display(parent)))?;
                    }
                    tokio::fs::write(&item.path, content.as_bytes())
                        .await
                        .with_context(|| format!("cannot write {}", item.display))?;
                    writeln!(
                        &mut report,
                        "A {} ({} hunk{})",
                        item.display,
                        item.hunks,
                        plural(item.hunks)
                    )?;
                }
                Action::Update(content) => {
                    tokio::fs::write(&item.path, content.as_bytes())
                        .await
                        .with_context(|| format!("cannot write {}", item.display))?;
                    writeln!(
                        &mut report,
                        "M {} ({} hunk{})",
                        item.display,
                        item.hunks,
                        plural(item.hunks)
                    )?;
                }
                Action::Delete => {
                    tokio::fs::remove_file(&item.path)
                        .await
                        .with_context(|| format!("cannot delete {}", item.display))?;
                    writeln!(&mut report, "D {}", item.display)?;
                }
            }
        }

        Ok(ToolOutput::text(report))
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

async fn read_utf8(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    if looks_binary(&bytes) {
        bail!("{} appears to be a binary file", path.display());
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("{} is not valid UTF-8", path.display()))
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
}

// ---------------------------------------------------------------------------
// list_dir
// ---------------------------------------------------------------------------

pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> &'static str {
        "List files and directories under a workspace path. Respects depth and entry limits, \
         skips .git and hidden entries by default."
    }

    fn schema(&self) -> Value {
        schema_object(
            json!({
                "path": {"type": "string", "description": "Directory path. Defaults to the workspace root."},
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5,
                    "description": "Recursion depth. Default 1."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "description": "Maximum entries to return. Default 200."
                },
                "hidden": {"type": "boolean", "description": "Include dotfiles. Default false."}
            }),
            &[],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let base = match optional_str(&args, "path").filter(|path| !path.trim().is_empty()) {
            Some(path) => ctx.resolve(path)?,
            None => ctx.cwd.clone(),
        };
        let metadata = tokio::fs::metadata(&base)
            .await
            .with_context(|| format!("cannot stat {}", ctx.display(&base)))?;
        if metadata.is_file() {
            return Ok(ToolOutput::text(format!(
                "f {} ({} bytes)",
                ctx.display(&base),
                metadata.len()
            )));
        }
        if !metadata.is_dir() {
            bail!("{} is not a directory", ctx.display(&base));
        }

        let depth = optional_usize(&args, "depth")?.unwrap_or(1).clamp(1, 5);
        let limit = optional_usize(&args, "limit")?
            .unwrap_or(200)
            .clamp(1, 2_000);
        let hidden = optional_bool(&args, "hidden", false)?;

        let mut rows: Vec<(String, String, bool)> = Vec::new();
        let mut truncated = false;

        for entry in WalkDir::new(&base)
            .max_depth(depth)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name().to_string_lossy();
                if name == ".git" {
                    return false;
                }
                hidden || !name.starts_with('.')
            })
        {
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue;
            }
            if rows.len() >= limit {
                truncated = true;
                break;
            }
            let relative = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            let indent = "  ".repeat(entry.depth().saturating_sub(1));
            let name = entry.file_name().to_string_lossy();
            let is_dir = entry.file_type().is_dir();
            let line = if is_dir {
                format!("{indent}{name}/")
            } else {
                format!("{indent}{name}  ({})", human_size(size))
            };
            rows.push((relative.display().to_string(), line, is_dir));
        }

        rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.2.cmp(&a.2)));

        let mut output = String::new();
        for (_, line, _) in &rows {
            writeln!(&mut output, "{line}")?;
        }
        if rows.is_empty() {
            output.push_str("(empty directory)");
        } else if truncated {
            writeln!(
                &mut output,
                "... (entry limit {limit} reached; narrow the path or depth)"
            )?;
        }

        Ok(ToolOutput::text(output))
    }
}

fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= GB {
        format!("{:.1}G", bytes_f / GB)
    } else if bytes_f >= MB {
        format!("{:.1}M", bytes_f / MB)
    } else if bytes_f >= KB {
        format!("{:.1}K", bytes_f / KB)
    } else {
        format!("{bytes}B")
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
    async fn writes_reads_and_edits() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());

        WriteFile
            .execute(json!({"path": "src/a.txt", "content": "one\ntwo\n"}), &ctx)
            .await
            .unwrap();
        let read = ReadFile
            .execute(json!({"path": "src/a.txt"}), &ctx)
            .await
            .unwrap();
        assert!(read.content.contains("1\tone"));
        assert!(read.content.contains("2\ttwo"));

        let edited = EditFile
            .execute(
                json!({"path": "src/a.txt", "old_string": "two", "new_string": "TWO"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(edited.content.contains("+1 -1"));
        assert!(edited.content.contains("+TWO"));
    }

    #[tokio::test]
    async fn edit_requires_unique_match() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();
        let err = EditFile
            .execute(
                json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2 places"));
    }

    #[tokio::test]
    async fn applies_update_create_and_delete_in_one_patch() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();

        let patch = r#"--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,1 @@
+hi
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
"#;
        ApplyPatch
            .execute(json!({"patch": patch}), &ctx)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\nTWO\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "hi\n"
        );
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[tokio::test]
    async fn apply_patch_refuses_to_overwrite_on_create() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        std::fs::write(dir.path().join("exists.txt"), "hello\n").unwrap();
        let patch = "--- /dev/null\n+++ b/exists.txt\n@@ -0,0 +1 @@\n+new\n";
        let err = ApplyPatch
            .execute(json!({"patch": patch}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn lists_directories_with_depth() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        let ctx = context(dir.path());

        let listing = ListDir.execute(json!({"depth": 2}), &ctx).await.unwrap();
        assert!(listing.content.contains("README.md"));
        assert!(listing.content.contains("src/"));
        assert!(listing.content.contains("main.rs"));
        assert!(listing.content.contains("nested/"));

        let shallow = ListDir.execute(json!({"depth": 1}), &ctx).await.unwrap();
        assert!(shallow.content.contains("src/"));
        assert!(!shallow.content.contains("main.rs"));
    }

    #[tokio::test]
    async fn read_file_offset_and_limit() {
        let dir = tempdir().unwrap();
        let ctx = context(dir.path());
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n4\n").unwrap();
        let output = ReadFile
            .execute(json!({"path": "a.txt", "offset": 2, "limit": 2}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("2\t2"));
        assert!(output.content.contains("3\t3"));
        assert!(!output.content.contains("1\t1"));
        assert!(output.content.contains("more lines"));
    }
}
