//! Token estimation and context-window accounting.
//!
//! Providers do not all report usage, and the ones that do only report it after
//! a request. Everything here is intentionally an estimate: it is good enough
//! for a context meter and for deciding when to compact, without ever being
//! presented as an exact billable count.

/// Rough token estimate for a piece of text (about four characters per token).
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4).max(1)
}

/// Estimated token count for a JSON value (used for tool schemas/arguments).
pub fn estimate_json_tokens(value: &serde_json::Value) -> u64 {
    estimate_tokens(&value.to_string())
}

/// Context window for a model id. Unknown ids get a conservative default.
///
/// The table is prefix/substring based because model ids carry provider
/// prefixes (`openai/`, `anthropic.`, `ollama/`, `:free`, ...).
pub fn context_window(model: &str) -> u64 {
    let model = model.to_ascii_lowercase();

    // Long-context Claude 5.5 family.
    if model.contains("claude") {
        if model.contains("5-5") || model.contains("5.5") || model.contains("fable") {
            return 1_000_000;
        }
        return 200_000;
    }

    if model.contains("deepseek") {
        return 128_000;
    }
    if model.contains("gemini") {
        return 1_000_000;
    }
    if model.contains("kimi") || model.contains("moonshot") {
        return 256_000;
    }
    if model.contains("qwen") {
        // qwen3.5 ships with a 128K window; older coder models often 32K.
        return if model.contains("qwen3") {
            131_072
        } else {
            32_768
        };
    }
    if model.contains("gpt-5")
        || model.contains("gpt-4.1")
        || model.contains("o3")
        || model.contains("o4")
    {
        return 1_000_000;
    }
    if model.contains("gpt-4o") || model.contains("gpt-4-turbo") {
        return 128_000;
    }
    if model.contains("llama") {
        return 131_072;
    }
    if model.contains("mistral") || model.contains("mixtral") {
        return 131_072;
    }
    128_000
}

/// Everything the context meter needs, in estimated tokens.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextUsage {
    pub window: u64,
    pub system: u64,
    pub memory: u64,
    pub tools: u64,
    pub conversation: u64,
}

impl ContextUsage {
    pub fn total(&self) -> u64 {
        self.system + self.memory + self.tools + self.conversation
    }

    /// Fraction in `0.0..=1.0`.
    pub fn fraction(&self) -> f64 {
        if self.window == 0 {
            return 0.0;
        }
        (self.total() as f64 / self.window as f64).clamp(0.0, 1.0)
    }

    pub fn percent(&self) -> u64 {
        (self.fraction() * 100.0).round() as u64
    }
}

/// A compact text meter, e.g. `42.1k / 128k (33%)`.
pub fn format_meter(usage: &ContextUsage) -> String {
    format!(
        "{} / {} ({}%)",
        format_tokens(usage.total()),
        format_tokens(usage.window),
        usage.percent()
    )
}

pub fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_about_four_chars_per_token() {
        assert_eq!(estimate_tokens("12345678"), 2);
        assert_eq!(estimate_tokens(""), 1);
    }

    #[test]
    fn knows_common_context_windows() {
        assert_eq!(context_window("anthropic/claude-opus-5-5"), 1_000_000);
        assert_eq!(context_window("deepseek-v4.1-flash"), 128_000);
        assert_eq!(context_window("qwen3.5:9b"), 131_072);
        assert_eq!(context_window("unknown-model"), 128_000);
    }

    #[test]
    fn meter_reports_total_and_percent() {
        let usage = ContextUsage {
            window: 100_000,
            system: 1_000,
            memory: 2_000,
            tools: 3_000,
            conversation: 4_000,
        };
        assert_eq!(usage.total(), 10_000);
        assert_eq!(usage.percent(), 10);
        assert!(format_meter(&usage).contains("10%"));
    }
}
