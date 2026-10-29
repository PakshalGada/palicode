//! Provider abstraction: OpenAI-compatible chat completions with streaming.

pub mod openai;
pub mod types;

pub use types::{
    ChatRequest, ChatResponse, Message, Provider, StreamEvent, ToolCall, ToolSpec, Usage,
};

use crate::config::{Config, ProviderConfig};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, warn};

/// A provider that has been selected and is ready to chat.
#[derive(Clone)]
pub struct ResolvedProvider {
    pub provider: Arc<dyn Provider>,
    pub model: String,
}

/// Result of probing one configured provider (used by `palicode models`).
#[derive(Debug, Clone)]
pub struct ProviderProbe {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
    pub has_credentials: bool,
    /// `Ok(models)` when reachable, `Err(reason)` otherwise.
    pub result: std::result::Result<Vec<String>, String>,
}

impl ProviderProbe {
    pub fn model_count(&self) -> Option<usize> {
        self.result.as_ref().ok().map(Vec::len)
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("palicode/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("cannot build HTTP client")
}

/// Build a provider from config, without touching the network.
pub fn build(config: &ProviderConfig, client: &reqwest::Client) -> Result<Arc<dyn Provider>> {
    match config.kind.as_str() {
        "openai_compatible" | "openai" => {
            let provider = openai::OpenAiCompatible::new(config, client.clone())?;
            Ok(Arc::new(provider))
        }
        other => bail!("unsupported provider type '{other}'"),
    }
}

/// Probe every configured provider concurrently-ish (sequential, short timeout).
pub async fn probe_all(config: &Config) -> Vec<ProviderProbe> {
    let client = match build_client() {
        Ok(client) => client,
        Err(err) => {
            return config
                .providers
                .list
                .iter()
                .map(|cfg| ProviderProbe {
                    name: cfg.name.clone(),
                    base_url: cfg.base_url.clone(),
                    enabled: cfg.enabled,
                    has_credentials: cfg.has_credentials(),
                    result: Err(format!("{err:#}")),
                })
                .collect();
        }
    };

    let mut probes = Vec::new();
    for cfg in &config.providers.list {
        let has_credentials = cfg.has_credentials();
        let result = if !cfg.enabled {
            Err("disabled in config".to_string())
        } else {
            // Some gateways (OpenCode Console) expose their model catalog
            // publicly, so list even when no key is configured.
            match build(cfg, &client) {
                Ok(provider) => match timeout(PROBE_TIMEOUT, provider.list_models()).await {
                    Ok(Ok(models)) => Ok(models),
                    Ok(Err(err)) if !has_credentials => Err(format!(
                        "missing API key (set ${}): {err:#}",
                        cfg.api_key_env
                    )),
                    Ok(Err(err)) => Err(format!("{err:#}")),
                    Err(_) if !has_credentials => {
                        Err(format!("missing API key (set ${})", cfg.api_key_env))
                    }
                    Err(_) => Err(format!("timed out after {}s", PROBE_TIMEOUT.as_secs())),
                },
                Err(err) => Err(format!("{err:#}")),
            }
        };
        probes.push(ProviderProbe {
            name: cfg.name.clone(),
            base_url: cfg.base_url.clone(),
            enabled: cfg.enabled,
            has_credentials,
            result,
        });
    }
    probes
}

/// Resolve which provider/model to use.
///
/// Priority: explicit argument -> config default -> provider with credentials
/// (OpenCode first) -> any reachable provider.
pub async fn resolve(
    config: &Config,
    requested_provider: Option<&str>,
    requested_model: Option<&str>,
) -> Result<ResolvedProvider> {
    let client = build_client()?;
    let requested_provider = requested_provider
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| config.general.default_provider.clone());

    let candidates: Vec<&ProviderConfig> = if let Some(name) = &requested_provider {
        let provider = config
            .provider(name)
            .with_context(|| format!("unknown provider '{name}'"))?;
        if !provider.enabled {
            bail!("provider '{name}' is disabled in config");
        }
        vec![provider]
    } else {
        let mut list: Vec<&ProviderConfig> = config.enabled_providers().collect();
        // Prefer providers that have credentials, stable within that ordering.
        list.sort_by_key(|provider| !provider.has_credentials());
        list
    };

    let mut failures = Vec::new();
    for cfg in candidates {
        if !cfg.has_credentials() {
            failures.push(format!(
                "{}: missing API key (set ${})",
                cfg.name, cfg.api_key_env
            ));
            continue;
        }
        let provider = build(cfg, &client)
            .with_context(|| format!("cannot initialize provider '{}'", cfg.name))?;

        let models = match timeout(PROBE_TIMEOUT, provider.list_models()).await {
            Ok(Ok(models)) => models,
            Ok(Err(err)) => {
                let reason = format!("{}: {err:#}", cfg.name);
                if requested_provider.is_some() {
                    bail!("{reason}");
                }
                failures.push(reason);
                continue;
            }
            Err(_) => {
                let reason = format!("{}: timed out after {}s", cfg.name, PROBE_TIMEOUT.as_secs());
                if requested_provider.is_some() {
                    bail!("{reason}");
                }
                failures.push(reason);
                continue;
            }
        };

        let default_model = config
            .general
            .default_model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| Some(cfg.default_model.clone()).filter(|m| !m.trim().is_empty()));

        let model = choose_model(requested_model, default_model.as_deref(), &models)?;
        debug!(
            provider = %cfg.name,
            model = %model,
            available = models.len(),
            "provider resolved"
        );
        return Ok(ResolvedProvider { provider, model });
    }

    let detail = if failures.is_empty() {
        "no providers are configured".to_string()
    } else {
        failures.join("; ")
    };
    bail!("no usable provider found: {detail}")
}

fn choose_model(
    requested: Option<&str>,
    provider_default: Option<&str>,
    discovered: &[String],
) -> Result<String> {
    if let Some(model) = requested.map(str::trim).filter(|m| !m.is_empty()) {
        return Ok(model.to_string());
    }
    if let Some(model) = provider_default.filter(|m| !m.is_empty()) {
        if discovered.is_empty() || discovered.iter().any(|m| m == model) {
            return Ok(model.to_string());
        }
        warn!(
            model = %model,
            "configured default model is not advertised by the provider; picking another"
        );
    }

    // Prefer coding models, then any non-embedding model.
    if let Some(model) = discovered
        .iter()
        .find(|model| model.to_ascii_lowercase().contains("coder"))
    {
        return Ok(model.clone());
    }
    if let Some(model) = discovered.iter().find(|model| !is_embedding_model(model)) {
        return Ok(model.clone());
    }
    discovered
        .first()
        .cloned()
        .context("provider is reachable but advertises no models")
}

fn is_embedding_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    ["embed", "bge", "nomic", "e5-", "gte-", "minilm"]
        .iter()
        .any(|marker| model.contains(marker))
}

/// Return model IDs from a `/models` JSON response, tolerating common shapes.
pub(crate) fn parse_models(value: &Value) -> Vec<String> {
    let array = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .or_else(|| value.as_array());

    let mut models: Vec<String> = array
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                return Some(id.to_string());
            }
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                return Some(name.to_string());
            }
            if let Some(name) = item.get("model").and_then(Value::as_str) {
                return Some(name.to_string());
            }
            item.as_str().map(str::to_string)
        })
        .filter(|model| !model.trim().is_empty())
        .collect();
    models.sort();
    models.dedup();
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_models_shape() {
        let value = serde_json::json!({
            "object": "list",
            "data": [{"id": "b"}, {"id": "a"}, {"id": "a"}]
        });
        assert_eq!(parse_models(&value), vec!["a", "b"]);
    }

    #[test]
    fn parses_ollama_tags_shape() {
        let value = serde_json::json!({
            "models": [{"name": "qwen2.5-coder:7b"}, {"name": "qwen3.5:9b"}]
        });
        assert_eq!(parse_models(&value), vec!["qwen2.5-coder:7b", "qwen3.5:9b"]);
    }

    #[test]
    fn chooses_explicit_model() {
        let model = choose_model(Some("custom"), Some("default"), &["a".to_string()]).unwrap();
        assert_eq!(model, "custom");
    }

    #[test]
    fn falls_back_when_default_missing() {
        let model = choose_model(None, Some("missing"), &["a".to_string()]).unwrap();
        assert_eq!(model, "a");
    }
}
