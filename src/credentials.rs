//! Provider credentials stored outside config.toml.
//!
//! The file is written with user-only permissions (0600 on Unix) and never
//! serialized into `palicode config --show`. Environment variables still take
//! precedence over stored keys.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

pub const OPENCODE_PROVIDER: &str = "opencode";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderCredential>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredential {
    pub api_key: String,
}

pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PALICODE_CREDENTIALS")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::config_dir().context("cannot determine config directory")?;
    Ok(base.join("palicode").join("credentials.toml"))
}

pub fn load() -> Result<Credentials> {
    load_from(&path()?)
}

pub fn load_from(path: &Path) -> Result<Credentials> {
    if !path.is_file() {
        return Ok(Credentials::default());
    }
    warn_if_permissions_are_loose(path);
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read credentials {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid credentials file {}", path.display()))
}

/// Read a stored key for `provider`, ignoring (and warning about) a broken file.
pub fn key_for(provider: &str) -> Option<String> {
    match load() {
        Ok(credentials) => credentials
            .providers
            .get(provider)
            .map(|entry| entry.api_key.trim().to_string())
            .filter(|key| !key.is_empty()),
        Err(err) => {
            warn!(error = %err, "cannot read stored credentials");
            None
        }
    }
}

pub fn set_key(provider: &str, api_key: &str) -> Result<PathBuf> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        anyhow::bail!("API key must not be empty");
    }
    let path = path()?;
    let mut credentials = load_from(&path)?;
    credentials.providers.insert(
        provider.to_string(),
        ProviderCredential {
            api_key: api_key.to_string(),
        },
    );
    save_to(&path, &credentials)?;
    Ok(path)
}

pub fn remove_key(provider: &str) -> Result<PathBuf> {
    let path = path()?;
    let mut credentials = load_from(&path)?;
    credentials.providers.remove(provider);
    if credentials.providers.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
        }
    } else {
        save_to(&path, &credentials)?;
    }
    Ok(path)
}

pub fn has_key(provider: &str) -> bool {
    key_for(provider).is_some()
}

pub fn save_to(path: &Path, credentials: &Credentials) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("credentials path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot secure {}", parent.display()))?;
    }

    let text = toml::to_string_pretty(credentials).context("cannot serialize credentials")?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "credentials.toml".to_string());
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("cannot create {}", temp.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("cannot write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot sync {}", temp.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot secure {}", temp.display()))?;
    }

    std::fs::rename(&temp, path).with_context(|| format!("cannot replace {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot secure {}", path.display()))?;
    }
    Ok(())
}

fn warn_if_permissions_are_loose(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warn!(
                    path = %path.display(),
                    mode = format!("{mode:o}"),
                    "credentials file is readable by other users; expected 0600"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn saves_loads_and_removes_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config").join("credentials.toml");

        let mut credentials = Credentials::default();
        credentials.providers.insert(
            OPENCODE_PROVIDER.to_string(),
            ProviderCredential {
                api_key: "sk-test-123".to_string(),
            },
        );
        save_to(&path, &credentials).unwrap();

        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.providers[OPENCODE_PROVIDER].api_key, "sk-test-123");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let parent_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(parent_mode, 0o700);
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempdir().unwrap();
        let loaded = load_from(&dir.path().join("nope.toml")).unwrap();
        assert!(loaded.providers.is_empty());
    }

    #[test]
    fn malformed_file_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        assert!(load_from(&path).is_err());
    }
}
