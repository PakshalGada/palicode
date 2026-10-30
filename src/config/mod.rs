//! Configuration loading and environment overlays.
//!
//! The config lives at `~/.config/palicode/config.toml` (or
//! `$PALICODE_CONFIG`). Missing files are fine: built-in defaults are used.

use anyhow::{Context, Result, bail};
use globset::GlobBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "palicode";
pub const CONFIG_FILE: &str = "config.toml";
pub const EXAMPLE_CONFIG: &str = include_str!("../../assets/config.example.toml");

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub providers: ProvidersConfig,
    pub tools: ToolsConfig,
    pub permissions: PermissionsConfig,
    pub memory: MemoryConfig,
    pub tui: TuiConfig,
    /// False when `.palicode/` exists but the workspace has not been trusted.
    /// Project config, rules, and imports are ignored while this is false.
    #[serde(skip)]
    pub project_trusted: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            providers: ProvidersConfig::default(),
            tools: ToolsConfig::default(),
            permissions: PermissionsConfig::default(),
            memory: MemoryConfig::default(),
            tui: TuiConfig::default(),
            project_trusted: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub theme: String,
    pub approval_mode: ApprovalMode,
    pub log_level: String,
    /// Provider used when `--provider` / `$PALICODE_PROVIDER` are absent.
    pub default_provider: Option<String>,
    /// Model override used when `--model` / `$PALICODE_MODEL` are absent.
    pub default_model: Option<String>,
    /// Maximum provider round-trips per user turn.
    pub max_steps: usize,
    /// Replace the built-in system prompt.
    pub system_prompt: Option<String>,
    /// Compact history once the approximate character count exceeds this.
    pub compact_after_chars: usize,
    /// Number of recent messages kept verbatim when compacting.
    pub compact_keep_messages: usize,
    /// Optional command run after plan execution (e.g. "cargo test").
    pub verify_command: Option<String>,
    /// Self-heal attempts when verification fails.
    pub verify_retries: usize,
    /// Write HANDOFF.md in the workspace when the TUI exits.
    pub handoff_on_exit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    Suggest,
    AutoEdit,
    FullAuto,
    /// Read-only analysis: every mutation, including shell commands that write,
    /// is denied.
    Plan,
    /// Allow read-only work and explicit allow rules; anything that would need
    /// a prompt is denied instead of blocking.
    DontAsk,
    /// Allow everything except built-in safety denials and protected paths.
    BypassPermissions,
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Suggest => "suggest",
            ApprovalMode::AutoEdit => "auto-edit",
            ApprovalMode::FullAuto => "full-auto",
            ApprovalMode::Plan => "plan",
            ApprovalMode::DontAsk => "dont-ask",
            ApprovalMode::BypassPermissions => "bypass-permissions",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "suggest" | "default" | "ask" => Some(ApprovalMode::Suggest),
            "auto-edit" | "autoedit" | "edit" | "accept-edits" => Some(ApprovalMode::AutoEdit),
            "full-auto" | "fullauto" | "yolo" => Some(ApprovalMode::FullAuto),
            "plan" | "read-only" | "readonly" => Some(ApprovalMode::Plan),
            "dont-ask" | "dontask" | "deny" => Some(ApprovalMode::DontAsk),
            "bypass-permissions" | "bypasspermissions" | "bypass" | "unsafe" => {
                Some(ApprovalMode::BypassPermissions)
            }
            _ => None,
        }
    }

    /// True when mutations are allowed without asking.
    pub fn is_permissive(self) -> bool {
        matches!(
            self,
            ApprovalMode::FullAuto | ApprovalMode::BypassPermissions
        )
    }
}

impl std::str::FromStr for ApprovalMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value).with_context(|| {
            format!(
                "invalid approval mode '{value}' (expected suggest, auto-edit, full-auto, plan, dont-ask, or bypass-permissions)"
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub list: Vec<ProviderConfig>,
}

/// An OpenAI-compatible endpoint (OpenCode Console, Ollama, LM Studio, ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(rename = "type", default = "default_provider_kind")]
    pub kind: String,
    pub base_url: String,
    /// Name of the environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: String,
    /// Literal API key. Prefer `api_key_env` or the Settings screen; this exists
    /// for local setups. Never serialized by `palicode config`.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub default_model: String,
    /// Optional override for the model-list endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Ask OpenAI to include token usage in the final stream chunk.
    #[serde(default)]
    pub stream_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub shell_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub max_read_lines: usize,
    pub path_sandbox: bool,
    /// Defaults to the process working directory.
    pub sandbox_root: Option<PathBuf>,
    /// Shell used by the `run_shell` tool.
    pub shell: String,
}

/// Tool permission rules. Rules are checked after the built-in safety denies;
/// a matching deny always wins, otherwise the first matching allow/ask applies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub rules: Vec<PermissionRule>,
    /// Claude-style shorthand lists. Each entry is a `Tool(specifier)` or a
    /// bare tool name.
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Glob matched against the tool name, e.g. "run_shell" or "write_*".
    pub tool: String,
    /// Optional glob matched against the tool subject (command, path, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    pub action: PermissionAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

/// Project-memory discovery (`PALICODE.md`, imports, `.palicode/rules`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Load instruction files at all. Useful for scripted runs in sensitive
    /// repositories.
    pub enabled: bool,
    /// Maximum number of `@import` hops (the root file is depth 0).
    pub max_import_depth: usize,
    /// Globs excluded from `@import` expansion, relative to the importing file
    /// or workspace.
    pub exclude: Vec<String>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_import_depth: 4,
            exclude: vec![
                ".git/**".to_string(),
                "node_modules/**".to_string(),
                "target/**".to_string(),
                ".venv/**".to_string(),
                "dist/**".to_string(),
                "build/**".to_string(),
                "*.lock".to_string(),
                "*.png".to_string(),
                "*.jpg".to_string(),
                "*.pdf".to_string(),
                "*.zip".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub mouse: bool,
    pub vim_nav: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            theme: "mono-dark".to_string(),
            approval_mode: ApprovalMode::Suggest,
            log_level: "warn".to_string(),
            default_provider: None,
            default_model: None,
            max_steps: 32,
            system_prompt: None,
            compact_after_chars: 120_000,
            compact_keep_messages: 12,
            verify_command: None,
            verify_retries: 1,
            handoff_on_exit: true,
        }
    }
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            list: default_providers(),
        }
    }
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            shell_timeout_secs: 120,
            max_output_bytes: 60_000,
            max_read_lines: 2_000,
            path_sandbox: true,
            sandbox_root: None,
            shell: "/bin/sh".to_string(),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            mouse: true,
            vim_nav: true,
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: default_provider_kind(),
            base_url: String::new(),
            api_key_env: String::new(),
            api_key: None,
            default_model: String::new(),
            models_url: None,
            enabled: true,
            stream_usage: false,
        }
    }
}

fn default_provider_kind() -> String {
    "openai_compatible".to_string()
}

fn default_true() -> bool {
    true
}

fn default_providers() -> Vec<ProviderConfig> {
    vec![
        ProviderConfig {
            name: "opencode".to_string(),
            base_url: "https://opencode.ai/inference/openai/v1".to_string(),
            api_key_env: "OPENCODE_API_KEY".to_string(),
            default_model: "deepseek-v4.1-flash".to_string(),
            models_url: Some("https://opencode.ai/inference/v1/models".to_string()),
            stream_usage: true,
            ..ProviderConfig::default()
        },
        ProviderConfig {
            name: "ollama".to_string(),
            base_url: default_ollama_base_url(),
            default_model: "qwen3.5:9b".to_string(),
            ..ProviderConfig::default()
        },
    ]
}

fn default_ollama_base_url() -> String {
    std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|host| !host.trim().is_empty())
        .map(|host| normalize_ollama_host(&host))
        .unwrap_or_else(|| "http://localhost:11434/v1".to_string())
}

impl ProviderConfig {
    /// Resolve the API key, preferring the environment over a literal value.
    pub fn api_key(&self) -> Option<String> {
        if !self.api_key_env.is_empty()
            && let Ok(value) = std::env::var(&self.api_key_env)
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_string());
        }
        if let Some(key) = self
            .api_key
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            return Some(key);
        }
        // Settings stores keys outside config.toml (user-only permissions).
        crate::credentials::key_for(&self.name)
    }

    /// True when no credential is required, or one is configured.
    pub fn has_credentials(&self) -> bool {
        self.api_key_env.is_empty() && self.api_key.is_none() || self.api_key().is_some()
    }

    pub fn models_url(&self) -> String {
        match &self.models_url {
            Some(url) if !url.trim().is_empty() => url.trim().to_string(),
            _ => format!("{}/models", self.base_url.trim_end_matches('/')),
        }
    }
}

impl Config {
    /// Default config path, honouring `$PALICODE_CONFIG`.
    pub fn path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("PALICODE_CONFIG")
            && !path.is_empty()
        {
            return Ok(PathBuf::from(path));
        }
        let base = dirs::config_dir().context("cannot determine config directory")?;
        Ok(base.join(APP_NAME).join(CONFIG_FILE))
    }

    pub fn load_from_optional(path: &Path, required: bool) -> Result<Self> {
        let mut config = if path.is_file() {
            Self::read_file(path)?
        } else if required {
            bail!("config file {} does not exist", path.display());
        } else {
            Config::default()
        };

        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    fn read_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    fn read_value(path: &Path) -> Result<toml::Value> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        text.parse::<toml::Value>()
            .with_context(|| format!("invalid config {}", path.display()))
    }

    /// Load the user config, layer the trusted project config on top, then
    /// apply environment variables. CLI flags are applied by the caller and
    /// therefore win over everything here.
    pub fn load_hierarchy(cwd: &Path) -> Result<Self> {
        Self::load_hierarchy_from(&Self::path()?, cwd)
    }

    pub fn load_hierarchy_from(user_path: &Path, cwd: &Path) -> Result<Self> {
        let mut value = if user_path.is_file() {
            Self::read_value(user_path)?
        } else {
            toml::Value::Table(Default::default())
        };

        let project_path = cwd.join(".palicode").join(CONFIG_FILE);
        let has_project = project_path.is_file() || cwd.join(".palicode").join("rules").is_dir();
        let trusted = crate::trust::is_trusted(cwd);
        if project_path.is_file() {
            if trusted {
                let project = Self::read_value(&project_path)?;
                merge_values(&mut value, project, "");
            } else {
                tracing::warn!(
                    path = %project_path.display(),
                    "project config ignored: workspace is not trusted (run `palicode trust`)"
                );
            }
        }

        // Optional Claude-style settings compatibility. These are only read
        // after trust, exactly like project config/rules.
        if trusted {
            for settings in [
                cwd.join(".claude").join("settings.json"),
                cwd.join(".claude").join("settings.local.json"),
            ] {
                merge_claude_settings(&mut value, &settings);
            }
        }

        let mut config: Config =
            value.try_into().context("invalid merged configuration")?;
        config.project_trusted = trusted || !has_project;
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Insert a rule into the project config, creating `.palicode/config.toml`
    /// and `.palicode/` as needed. Used by "don't ask again" approvals.
    pub fn add_project_rule(cwd: &Path, rule: &PermissionRule) -> Result<PathBuf> {
        let dir = cwd.join(".palicode");
        let path = dir.join(CONFIG_FILE);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;

        let mut root = if path.is_file() {
            Self::read_value(&path)?
        } else {
            toml::Value::Table(Default::default())
        };
        let table = root
            .as_table_mut()
            .context("project config root must be a table")?;
        let permissions = table
            .entry("permissions")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .context("permissions must be a table")?;
        let rules = permissions
            .entry("rules")
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .context("permissions.rules must be an array")?;
        let mut entry = toml::value::Table::new();
        entry.insert("tool".to_string(), toml::Value::String(rule.tool.clone()));
        if let Some(pattern) = &rule.pattern {
            entry.insert("pattern".to_string(), toml::Value::String(pattern.clone()));
        }
        entry.insert(
            "action".to_string(),
            toml::Value::String(match rule.action {
                PermissionAction::Allow => "allow".to_string(),
                PermissionAction::Ask => "ask".to_string(),
                PermissionAction::Deny => "deny".to_string(),
            }),
        );
        let value = toml::Value::Table(entry);
        if !rules.iter().any(|existing| existing == &value) {
            rules.push(value);
        }
        let text = toml::to_string_pretty(&root).context("cannot serialize project config")?;
        std::fs::write(&path, text)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(path)
    }

    /// Environment variables override file values.
    pub fn apply_env(&mut self) {
        if let Ok(host) = std::env::var("OLLAMA_HOST")
            && !host.trim().is_empty()
        {
            let url = normalize_ollama_host(&host);
            match self.providers.list.iter_mut().find(|p| p.name == "ollama") {
                Some(provider) => provider.base_url = url,
                None => self.providers.list.push(ProviderConfig {
                    name: "ollama".to_string(),
                    base_url: url,
                    ..ProviderConfig::default()
                }),
            }
        }

        if let Ok(value) = std::env::var("PALICODE_PROVIDER")
            && !value.trim().is_empty()
        {
            self.general.default_provider = Some(value.trim().to_string());
        }
        if let Ok(value) = std::env::var("PALICODE_MODEL")
            && !value.trim().is_empty()
        {
            self.general.default_model = Some(value.trim().to_string());
        }
        if let Ok(value) = std::env::var("PALICODE_LOG")
            && !value.trim().is_empty()
        {
            self.general.log_level = value.trim().to_string();
        }
        if let Ok(value) = std::env::var("PALICODE_SANDBOX")
            && let Some(enabled) = parse_bool(&value)
        {
            self.tools.path_sandbox = enabled;
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.general.max_steps == 0 {
            bail!("general.max_steps must be greater than zero");
        }
        if self.general.compact_after_chars < 1_000 {
            bail!("general.compact_after_chars must be at least 1000");
        }
        if self.general.compact_keep_messages < 2 {
            bail!("general.compact_keep_messages must be at least 2");
        }
        if self.general.verify_retries > 10 {
            bail!("general.verify_retries must be at most 10");
        }
        if self.memory.max_import_depth > 16 {
            bail!("memory.max_import_depth must be at most 16");
        }
        if self.tools.shell_timeout_secs == 0 {
            bail!("tools.shell_timeout_secs must be greater than zero");
        }
        if self.tools.max_output_bytes < 256 {
            bail!("tools.max_output_bytes must be at least 256");
        }
        if self.tools.max_read_lines == 0 {
            bail!("tools.max_read_lines must be greater than zero");
        }
        if self.tools.shell.trim().is_empty() {
            bail!("tools.shell must not be empty");
        }
        if self.providers.list.is_empty() {
            bail!("at least one provider must be configured");
        }

        for (index, rule) in self.permissions.rules.iter().enumerate() {
            if rule.tool.trim().is_empty() {
                bail!("permissions.rules[{index}].tool must not be empty");
            }
            validate_glob(&rule.tool)
                .with_context(|| format!("permissions.rules[{index}].tool is not a valid glob"))?;
            if let Some(pattern) = &rule.pattern {
                if pattern.trim().is_empty() {
                    bail!("permissions.rules[{index}].pattern must not be empty");
                }
                validate_glob(pattern).with_context(|| {
                    format!("permissions.rules[{index}].pattern is not a valid glob")
                })?;
            }
        }

        let mut seen = HashSet::new();
        for provider in &self.providers.list {
            if provider.name.trim().is_empty() {
                bail!("provider name must not be empty");
            }
            if !seen.insert(provider.name.as_str()) {
                bail!("duplicate provider name '{}'", provider.name);
            }
            if provider.kind != "openai_compatible" && provider.kind != "openai" {
                bail!(
                    "provider '{}' has unsupported type '{}' (expected openai_compatible)",
                    provider.name,
                    provider.kind
                );
            }
            if !provider.base_url.starts_with("http://")
                && !provider.base_url.starts_with("https://")
            {
                bail!(
                    "provider '{}' base_url must start with http:// or https://",
                    provider.name
                );
            }
        }
        Ok(())
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.list.iter().find(|p| p.name == name)
    }

    pub fn enabled_providers(&self) -> impl Iterator<Item = &ProviderConfig> {
        self.providers.list.iter().filter(|p| p.enabled)
    }

    /// Write the commented example config. Returns the path written.
    pub fn init(path: &Path, force: bool) -> Result<PathBuf> {
        if path.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite)",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(path, EXAMPLE_CONFIG)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(path.to_path_buf())
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("cannot serialize config")
    }
}

/// Accept `host:port`, `http://host`, or a full base URL and return `.../v1`.
pub fn normalize_ollama_host(input: &str) -> String {
    let mut host = input.trim().to_string();
    if !host.starts_with("http://") && !host.starts_with("https://") {
        host = format!("http://{host}");
    }
    while host.ends_with('/') {
        host.pop();
    }
    if host.ends_with("/v1") || host.contains("/v1/") {
        host
    } else {
        format!("{host}/v1")
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn merge_values(base: &mut toml::Value, overlay: toml::Value, prefix: &str) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                match base_table.get_mut(&key) {
                    Some(existing) => {
                        let concatenate = matches!(
                            path.as_str(),
                            "permissions.rules"
                                | "permissions.allow"
                                | "permissions.ask"
                                | "permissions.deny"
                        );
                        if concatenate {
                            if let (toml::Value::Array(base_items), toml::Value::Array(items)) =
                                (&mut *existing, value)
                            {
                                base_items.extend(items);
                            }
                        } else {
                            merge_values(existing, value, &path);
                        }
                    }
                    None => {
                        base_table.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Read the `permissions.allow/ask/deny` arrays from a Claude settings file.
fn merge_claude_settings(target: &mut toml::Value, path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::warn!(path = %path.display(), "ignoring invalid Claude settings file");
        return;
    };
    let Some(permissions) = json.get("permissions") else {
        return;
    };
    let Some(table) = target.as_table_mut() else {
        return;
    };
    let permissions_table = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let Some(permissions_table) = permissions_table.as_table_mut() else {
        return;
    };
    for key in ["allow", "ask", "deny"] {
        let Some(values) = permissions.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        let entry = permissions_table
            .entry(key)
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let Some(items) = entry.as_array_mut() else {
            continue;
        };
        for value in values {
            if let Some(text) = value.as_str() {
                let text = toml::Value::String(text.to_string());
                if !items.contains(&text) {
                    items.push(text);
                }
            }
        }
    }
}

fn validate_glob(pattern: &str) -> Result<()> {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(false)
        .build()
        .with_context(|| format!("invalid glob '{pattern}'"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contain_opencode_and_ollama() {
        let config = Config::default();
        assert!(config.provider("opencode").is_some());
        assert!(config.provider("ollama").is_some());
        assert!(config.tools.path_sandbox);
    }

    #[test]
    fn parses_partial_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[general]
approval_mode = "full-auto"
max_steps = 7

[[providers.list]]
name = "local"
type = "openai_compatible"
base_url = "http://localhost:11434/v1"
default_model = "qwen2.5-coder:7b"

[tools]
path_sandbox = false

[permissions]
[[permissions.rules]]
tool = "run_shell"
pattern = "git push*"
action = "ask"
"#,
        )
        .unwrap();

        let config = Config::load_from_optional(&path, true).unwrap();
        assert_eq!(config.general.approval_mode, ApprovalMode::FullAuto);
        assert_eq!(config.general.max_steps, 7);
        assert!(!config.tools.path_sandbox);
        assert_eq!(config.providers.list.len(), 1);
        assert_eq!(config.providers.list[0].name, "local");
        assert_eq!(config.permissions.rules.len(), 1);
        assert_eq!(config.permissions.rules[0].action, PermissionAction::Ask);
    }

    #[test]
    fn normalizes_ollama_hosts() {
        assert_eq!(
            normalize_ollama_host("localhost:11434"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_ollama_host("http://localhost:11434/v1/"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_ollama_host("https://ollama.example.com"),
            "https://ollama.example.com/v1"
        );
    }

    #[test]
    fn rejects_duplicate_provider_names() {
        let mut config = Config::default();
        config.providers.list.push(config.providers.list[0].clone());
        assert!(config.validate().is_err());
    }

    #[test]
    fn serialized_config_never_contains_secrets() {
        let mut config = Config::default();
        config.providers.list[0].api_key = Some("sk-super-secret".to_string());
        let toml = config.to_toml().unwrap();
        assert!(!toml.contains("sk-super-secret"));
    }

    #[test]
    fn parses_approval_modes() {
        assert_eq!(ApprovalMode::parse("suggest"), Some(ApprovalMode::Suggest));
        assert_eq!(
            ApprovalMode::parse("AUTO-EDIT"),
            Some(ApprovalMode::AutoEdit)
        );
        assert_eq!(ApprovalMode::parse("yolo"), Some(ApprovalMode::FullAuto));
        assert_eq!(ApprovalMode::parse("nope"), None);
    }

    #[test]
    fn validates_permission_rules() {
        let mut config = Config::default();
        config.permissions.rules.push(PermissionRule {
            tool: "run_shell".to_string(),
            pattern: Some("git push*".to_string()),
            action: PermissionAction::Ask,
        });
        config.validate().unwrap();

        config.permissions.rules.push(PermissionRule {
            tool: String::new(),
            pattern: None,
            action: PermissionAction::Deny,
        });
        assert!(config.validate().is_err());
    }
}
