//! Workspace trust records.
//!
//! Project config, `.palicode/rules`, `@import`s from outside the workspace,
//! and Claude-compatible settings are inert until the workspace has been
//! explicitly trusted. Trust is stored per canonical path in the user config
//! directory, outside the repository, so a repository cannot trust itself.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const TRUST_FILE: &str = "trusted.toml";

pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PALICODE_TRUST_FILE")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::config_dir().context("cannot determine config directory")?;
    Ok(base.join(crate::config::APP_NAME).join(TRUST_FILE))
}

#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    path: PathBuf,
    entries: BTreeSet<PathBuf>,
}

impl TrustStore {
    pub fn load() -> Result<Self> {
        Self::load_at(&default_path()?)
    }

    pub fn load_at(path: &Path) -> Result<Self> {
        let entries = if path.is_file() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read trust file {}", path.display()))?;
            let value: toml::Value = text
                .parse()
                .with_context(|| format!("invalid trust file {}", path.display()))?;
            value
                .get("trusted")
                .and_then(toml::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            entries,
        })
    }

    pub fn is_trusted(&self, cwd: &Path) -> bool {
        let Ok(cwd) = cwd.canonicalize() else {
            return false;
        };
        self.entries.iter().any(|entry| {
            entry.canonicalize().unwrap_or_else(|_| entry.clone()) == cwd
        })
    }

    pub fn trust(&mut self, cwd: &Path) -> Result<()> {
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", cwd.display()))?;
        self.entries.insert(cwd);
        self.save()
    }

    pub fn revoke(&mut self, cwd: &Path) -> Result<bool> {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let removed = self.entries.remove(&cwd);
        self.save()?;
        Ok(removed)
    }

    pub fn entries(&self) -> impl Iterator<Item = &PathBuf> {
        self.entries.iter()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut root = toml::value::Table::new();
        root.insert(
            "trusted".to_string(),
            toml::Value::Array(
                self.entries
                    .iter()
                    .map(|path| toml::Value::String(path.display().to_string()))
                    .collect(),
            ),
        );
        let text = toml::to_string_pretty(&toml::Value::Table(root))
            .context("cannot serialize trust file")?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// True when the workspace has a trust record.
pub fn is_trusted(cwd: &Path) -> bool {
    TrustStore::load()
        .map(|store| store.is_trusted(cwd))
        .unwrap_or(false)
}

/// Record trust for `cwd`.
pub fn trust(cwd: &Path) -> Result<PathBuf> {
    let path = default_path()?;
    let mut store = TrustStore::load_at(&path)?;
    store.trust(cwd)?;
    Ok(path)
}

pub fn revoke(cwd: &Path) -> Result<bool> {
    let path = default_path()?;
    let mut store = TrustStore::load_at(&path)?;
    store.revoke(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_revokes_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("trusted.toml");
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut store = TrustStore::load_at(&file).unwrap();
        assert!(!store.is_trusted(&workspace));
        store.trust(&workspace).unwrap();
        let reloaded = TrustStore::load_at(&file).unwrap();
        assert!(reloaded.is_trusted(&workspace));
        assert!(reloaded.entries().any(|entry| entry == &workspace));
    }
}
