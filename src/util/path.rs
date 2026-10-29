//! Path resolution with an optional sandbox root.
//!
//! Tool paths are resolved relative to the agent's working directory. When a
//! sandbox root is configured, the resolved path (including symlinks in any
//! existing ancestor) must live inside that root.

use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};

/// Resolve a user/model supplied path.
///
/// * `cwd` - base directory for relative paths.
/// * `sandbox_root` - when `Some`, the result must stay inside this directory.
/// * `raw` - path from a tool call; `~` is expanded.
pub fn resolve_path(cwd: &Path, sandbox_root: Option<&Path>, raw: &str) -> Result<PathBuf> {
    if raw.contains('\0') {
        bail!("path contains a NUL byte");
    }
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("path must not be empty");
    }

    let expanded = expand_home(raw)?;
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let normalized = normalize_lexical(&joined);

    if let Some(root) = sandbox_root {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot resolve sandbox root {}", root.display()))?;
        let resolved = canonicalize_allow_missing(&normalized)?;
        if !resolved.starts_with(&root) {
            bail!(
                "path {} is outside the sandbox root {}",
                normalized.display(),
                root.display()
            );
        }
    }

    Ok(normalized)
}

fn expand_home(raw: &str) -> Result<PathBuf> {
    if raw == "~" || raw.starts_with("~/") {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        if raw == "~" {
            return Ok(home);
        }
        return Ok(home.join(&raw[2..]));
    }
    Ok(PathBuf::from(raw))
}

/// Remove `.` and resolve `..` lexically without touching the filesystem.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop above a root/prefix.
                if !out.pop() {
                    // Relative path that escapes its own start: keep it explicit.
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Canonicalize `path`, tolerating a missing tail (e.g. a new file).
///
/// Existing ancestors are canonicalized so symlinks cannot be used to escape
/// the sandbox.
fn canonicalize_allow_missing(path: &Path) -> Result<PathBuf> {
    let mut probe = path.to_path_buf();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();

    loop {
        match probe.canonicalize() {
            Ok(mut canonical) => {
                for part in missing.iter().rev() {
                    canonical.push(part);
                }
                return Ok(canonical);
            }
            Err(err) => {
                let Some(name) = probe.file_name().map(|n| n.to_owned()) else {
                    return Err(err).with_context(|| {
                        format!("cannot resolve any existing ancestor of {}", path.display())
                    });
                };
                missing.push(name);
                let Some(parent) = probe.parent() else {
                    return Err(err).with_context(|| {
                        format!("cannot resolve any existing ancestor of {}", path.display())
                    });
                };
                probe = parent.to_path_buf();
            }
        }
    }
}

/// Render `path` relative to `cwd` when possible, otherwise absolute.
pub fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
        Ok(rest) => rest.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dotdot() {
        assert_eq!(
            normalize_lexical(Path::new("/tmp/a/../b/./c")),
            PathBuf::from("/tmp/b/c")
        );
    }

    #[test]
    fn relative_path_is_joined_to_cwd() {
        let p = resolve_path(Path::new("/tmp/work"), None, "src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/work/src/main.rs"));
    }

    #[test]
    fn sandbox_rejects_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let err = resolve_path(&root, Some(&root), "../etc/passwd").unwrap_err();
        assert!(err.to_string().contains("outside the sandbox"));
    }

    #[test]
    fn sandbox_allows_missing_child() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let p = resolve_path(&root, Some(&root), "new/dir/file.txt").unwrap();
        assert_eq!(p, root.join("new/dir/file.txt"));
    }
}
