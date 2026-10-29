//! OpenAI-compatible chat completions client (OpenCode Console, Ollama, ...).

use super::types::{
    ChatRequest, ChatResponse, Message, Provider, ProviderStream, Role, StreamEvent, ToolCall,
    ToolSpec, Usage,
};
use crate::config::ProviderConfig;
use crate::util::text::{truncate_middle, truncate_tail};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const MODELS_TIMEOUT: Duration = Duration::from_secs(10);
const ERROR_BODY_LIMIT: usize = 4_000;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);

pub struct OpenAiCompatible {
    name: String,
    base_url: String,
    api_key: Option<String>,
    models_url: String,
    stream_usage: bool,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    pub fn new(config: &ProviderConfig, client: reqwest::Client) -> Result<Self> {
        if config.base_url.trim().is_empty() {
            bail!("provider '{}' has an empty base_url", config.name);
        }
        Ok(Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key(),
            models_url: config.models_url(),
            stream_usage: config.stream_usage,
            client,
        })
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    async fn send_with_retry<F>(&self, build: F, label: &str) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempt = 0usize;
        loop {
            let response = build()
                .send()
                .await
                .with_context(|| format!("cannot reach provider '{}'", self.name))?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }

            let wait = retry_delay(
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()),
                attempt,
            );
            let body = response.text().await.unwrap_or_default();

            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RATE_LIMIT_RETRIES
            {
                debug!(
                    provider = %self.name,
                    attempt = attempt + 1,
                    wait_ms = wait.as_millis(),
                    "rate limited; retrying"
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
                continue;
            }

            let hint = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                "\nhint: provider rate limit or quota reached. Wait and retry, switch model/provider, \
                 or check OpenCode Console credits. Free models are throttled more aggressively."
            } else {
                ""
            };
            bail!(
                "provider '{}' {} returned HTTP {}: {}{hint}",
                self.name,
                label,
                status,
                truncate_middle(body.trim(), ERROR_BODY_LIMIT)
            );
        }
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }
}

#[async_trait]
impl Provider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn chat_stream(&self, request: ChatRequest) -> Result<ProviderStream> {
        let body = WireRequest {
            model: request.model.clone(),
            messages: request.messages.iter().map(to_wire_message).collect(),
            tools: (!request.tools.is_empty()).then_some(request.tools.as_slice()),
            stream: true,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            stream_options: self.stream_usage.then_some(StreamOptions {
                include_usage: true,
            }),
        };

        debug!(
            provider = %self.name,
            model = %request.model,
            messages = request.messages.len(),
            tools = request.tools.len(),
            "starting chat stream"
        );

        let url = self.chat_url();
        let response = self
            .send_with_retry(
                || self.auth(self.client.post(&url)).json(&body),
                "chat request",
            )
            .await?;

        let mut byte_stream = response.bytes_stream();
        let (tx, stream) = ProviderStream::channel(256);
        let provider_name = self.name.clone();

        tokio::spawn(async move {
            let mut decoder = SseDecoder::default();
            let mut assembler = StreamAssembler::default();

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        let _ = tx
                            .send(Err(anyhow!("stream error from '{provider_name}': {err}")))
                            .await;
                        return;
                    }
                };
                for data in decoder.push(&chunk) {
                    if !pump(&mut assembler, data, &tx).await {
                        return;
                    }
                }
                if assembler.finished {
                    return;
                }
            }

            for data in decoder.finish() {
                if !pump(&mut assembler, data, &tx).await {
                    return;
                }
                if assembler.finished {
                    return;
                }
            }

            if let Some(response) = assembler.finish() {
                let _ = tx.send(Ok(StreamEvent::Finished(response))).await;
            } else {
                let _ = tx
                    .send(Err(anyhow!(
                        "provider '{provider_name}' closed the stream without a completion"
                    )))
                    .await;
            }
        });

        Ok(stream)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .send_with_retry(
                || {
                    self.auth(self.client.get(&self.models_url))
                        .timeout(MODELS_TIMEOUT)
                },
                "model list",
            )
            .await?;

        let value: Value = response
            .json()
            .await
            .with_context(|| format!("invalid model list JSON from '{}'", self.name))?;
        let models = super::parse_models(&value);
        if models.is_empty() {
            warn!(provider = %self.name, "provider advertised zero models");
        }
        Ok(models)
    }
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolSpec]>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunction,
}

#[derive(Serialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

fn to_wire_message(message: &Message) -> WireMessage {
    let mut content = message.content.clone();
    if message.role == Role::Assistant && !message.tool_calls.is_empty() && content.is_none() {
        content = Some(String::new());
    }
    WireMessage {
        role: message.role.as_str(),
        content,
        tool_calls: (!message.tool_calls.is_empty()).then(|| {
            message
                .tool_calls
                .iter()
                .map(|call| WireToolCall {
                    id: call.id.clone(),
                    kind: "function",
                    function: WireFunction {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                })
                .collect()
        }),
        tool_call_id: message.tool_call_id.clone(),
    }
}

fn retry_delay(retry_after: Option<&str>, attempt: usize) -> Duration {
    let from_header = retry_after
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    from_header
        .unwrap_or_else(|| Duration::from_secs(2_u64.saturating_pow(attempt as u32)))
        .min(MAX_RETRY_WAIT)
}

/// Feed one SSE event into the assembler and forward resulting events.
async fn pump(
    assembler: &mut StreamAssembler,
    data: String,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> bool {
    let mut events = Vec::new();
    if let Err(err) = assembler.ingest(&data, &mut events) {
        let _ = tx.send(Err(err)).await;
        return false;
    }
    for event in events {
        if tx.send(Ok(event)).await.is_err() {
            return false;
        }
    }
    true
}

/// Incremental server-sent-events decoder.
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((index, separator_len)) = find_event_boundary(&self.buffer) {
            let raw: Vec<u8> = self.buffer.drain(..index + separator_len).collect();
            if let Some(data) = parse_sse_event(&raw) {
                events.push(data);
            }
        }
        events
    }

    /// Flush a trailing event that was not followed by a blank line.
    fn finish(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        self.buffer.extend_from_slice(b"\n\n");
        self.push(&[])
    }
}

fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < buffer.len() {
        if buffer[index] == b'\n' {
            if buffer.get(index + 1) == Some(&b'\n') {
                return Some((index, 2));
            }
            if buffer.get(index + 1) == Some(&b'\r') && buffer.get(index + 2) == Some(&b'\n') {
                return Some((index, 3));
            }
        }
        index += 1;
    }
    None
}

fn parse_sse_event(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let mut data = String::new();
    let mut saw_data = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if saw_data {
                data.push('\n');
            }
            data.push_str(rest);
            saw_data = true;
        }
    }
    saw_data.then_some(data)
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamAssembler {
    content: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, PartialToolCall>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    finished: bool,
    saw_data: bool,
}

impl StreamAssembler {
    fn ingest(&mut self, data: &str, out: &mut Vec<StreamEvent>) -> Result<()> {
        if self.finished || data.trim().is_empty() {
            return Ok(());
        }
        self.saw_data = true;

        if data.trim() == "[DONE]" {
            self.finished = true;
            out.push(StreamEvent::Finished(self.response()));
            return Ok(());
        }

        let value: Value = serde_json::from_str(data).with_context(|| {
            format!(
                "invalid streaming JSON from provider: {}",
                truncate_tail(data, 500)
            )
        })?;

        if let Some(error) = value.get("error").filter(|value| !value.is_null()) {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            bail!("provider error: {message}");
        }
        if value.get("type").and_then(Value::as_str) == Some("error") {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            bail!("provider error: {message}");
        }

        if let Some(usage) = value.get("usage").and_then(parse_usage) {
            self.usage = Some(usage);
        }

        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }

        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };

        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.content.push_str(text);
            out.push(StreamEvent::TextDelta(text.to_string()));
        }

        let reasoning = delta
            .get("reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| delta.get("reasoning").and_then(Value::as_str))
            .filter(|text| !text.is_empty());
        if let Some(reasoning) = reasoning {
            self.reasoning.push_str(reasoning);
            out.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in tool_calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(position);
                let entry = self.tool_calls.entry(index).or_default();

                if let Some(id) = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    entry.id = id.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                    {
                        entry.name = name.to_string();
                    }
                    match function.get("arguments") {
                        Some(Value::String(fragment)) => entry.arguments.push_str(fragment),
                        Some(other) if !other.is_null() => {
                            entry.arguments.push_str(&other.to_string())
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    fn response(&mut self) -> ChatResponse {
        let tool_calls: Vec<ToolCall> = std::mem::take(&mut self.tool_calls)
            .into_values()
            .filter(|call| !call.name.is_empty())
            .map(|call| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4().simple())
                } else {
                    call.id
                },
                name: call.name,
                arguments: if call.arguments.trim().is_empty() {
                    "{}".to_string()
                } else {
                    call.arguments
                },
            })
            .collect();

        let finish_reason = match self.finish_reason.take() {
            Some(reason) => Some(reason),
            None if !tool_calls.is_empty() => Some("tool_calls".to_string()),
            None if self.saw_data => Some("stop".to_string()),
            None => None,
        };

        if tool_calls.is_empty() && self.content.is_empty() && !self.saw_data {
            warn!("provider stream produced no content");
        }

        let mut content = std::mem::take(&mut self.content);
        // Some thinking models occasionally finish with the answer only in the
        // reasoning channel. Prefer that over returning nothing.
        if content.trim().is_empty() && tool_calls.is_empty() && !self.reasoning.trim().is_empty() {
            debug!("model returned reasoning without content; using it as the answer");
            content = std::mem::take(&mut self.reasoning);
        }

        ChatResponse {
            content,
            tool_calls,
            usage: self.usage.take(),
            finish_reason,
        }
    }

    /// Finalize when the byte stream ends without an explicit `[DONE]`.
    fn finish(&mut self) -> Option<ChatResponse> {
        if self.finished {
            return None;
        }
        if !self.saw_data {
            return None;
        }
        self.finished = true;
        Some(self.response())
    }
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let prompt_tokens = value.get("prompt_tokens").and_then(Value::as_u64)?;
    let completion_tokens = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_chunks(chunks: &[&str]) -> Vec<String> {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk.as_bytes()));
        }
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn retry_delay_honors_and_caps_retry_after() {
        assert_eq!(retry_delay(Some("3"), 0), Duration::from_secs(3));
        assert_eq!(retry_delay(Some("300"), 0), MAX_RETRY_WAIT);
        assert_eq!(retry_delay(None, 1), Duration::from_secs(2));
        assert_eq!(retry_delay(Some("bad"), 0), Duration::from_secs(1));
    }

    #[test]
    fn decodes_split_sse_chunks() {
        let events = collect_chunks(&["data: {\"a\":", "1}\n\n", "data: [DO", "NE]\n\n"]);
        assert_eq!(events, vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn decodes_crlf_events() {
        let events = collect_chunks(&["data: one\r\n\r\ndata: two\r\n\r\n"]);
        assert_eq!(events, vec!["one", "two"]);
    }

    #[test]
    fn assembles_text_and_usage() {
        let mut assembler = StreamAssembler::default();
        let mut events = Vec::new();
        assembler
            .ingest(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#, &mut events)
            .unwrap();
        assembler
            .ingest(
                r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
                &mut events,
            )
            .unwrap();
        assembler.ingest("[DONE]", &mut events).unwrap();

        let text: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::TextDelta(delta) => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");

        let finished = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Finished(response) => Some(response.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(finished.content, "Hello");
        assert_eq!(finished.finish_reason.as_deref(), Some("stop"));
        assert_eq!(finished.usage.unwrap().total_tokens, 5);
    }

    #[test]
    fn assembles_fragmented_tool_call() {
        let mut assembler = StreamAssembler::default();
        let mut events = Vec::new();
        assembler
            .ingest(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
                &mut events,
            )
            .unwrap();
        assembler
            .ingest(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.txt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                &mut events,
            )
            .unwrap();

        let response = assembler.finish().unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "call_1");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments, "{\"path\":\"a.txt\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn parses_reasoning_deltas() {
        let mut assembler = StreamAssembler::default();
        let mut events = Vec::new();
        assembler
            .ingest(
                r#"{"choices":[{"delta":{"reasoning_content":"thinking..."}}]}"#,
                &mut events,
            )
            .unwrap();
        assert!(matches!(
            events.first(),
            Some(StreamEvent::ReasoningDelta(text)) if text == "thinking..."
        ));
    }

    #[test]
    fn falls_back_to_reasoning_when_content_is_empty() {
        let mut assembler = StreamAssembler::default();
        let mut events = Vec::new();
        assembler
            .ingest(
                r#"{"choices":[{"delta":{"content":"","reasoning":"The answer is 42."}}]}"#,
                &mut events,
            )
            .unwrap();
        let response = assembler.finish().unwrap();
        assert_eq!(response.content, "The answer is 42.");
    }

    #[test]
    fn surfaces_provider_errors() {
        let mut assembler = StreamAssembler::default();
        let mut events = Vec::new();
        let err = assembler
            .ingest(r#"{"error":{"message":"bad key"}}"#, &mut events)
            .unwrap_err();
        assert!(err.to_string().contains("bad key"));
    }
}
