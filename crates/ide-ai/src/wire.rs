//! Request building and response decoding for the three wire protocols.

use serde_json::{json, Value};

use crate::config::AiConfig;
use crate::error::AiError;
use crate::provider::{ProviderKind, WireProtocol};
use crate::sse::SseEvent;
use crate::types::{ChatDelta, ChatMessage, ChatResponse, FinishReason, Role, Usage};

/// Anthropic API version header value (still the current version in 2026).
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A fully built HTTP request.
pub(crate) struct WireRequest {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Value,
}

/// Effective generation parameters after applying per-request options.
pub(crate) struct GenParams {
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
}

/// Authentication headers for `config` (shared by chat and model listing).
pub(crate) fn auth_headers(config: &AiConfig) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    let key = config.api_key.as_deref();
    match config.wire() {
        WireProtocol::OpenAiChat => {
            if let Some(key) = key {
                headers.push(("authorization", format!("Bearer {key}")));
            }
            if config.provider == ProviderKind::OpenRouter {
                headers.push(("x-title", "IDE Levity".to_string()));
            }
        }
        WireProtocol::AnthropicMessages => {
            if let Some(key) = key {
                headers.push(("x-api-key", key.to_string()));
            }
            headers.push(("anthropic-version", ANTHROPIC_VERSION.to_string()));
        }
        WireProtocol::GeminiGenerateContent => {
            if let Some(key) = key {
                headers.push(("x-goog-api-key", key.to_string()));
            }
        }
    }
    headers
}

fn gemini_model_path(model: &str) -> &str {
    model.strip_prefix("models/").unwrap_or(model)
}

/// Builds the chat request for `config`'s protocol.
pub(crate) fn build_chat_request(
    config: &AiConfig,
    messages: &[ChatMessage],
    params: &GenParams,
    stream: bool,
) -> WireRequest {
    let base = &config.base_url;
    let headers = auth_headers(config);
    match config.wire() {
        WireProtocol::OpenAiChat => {
            let msgs: Vec<Value> = messages
                .iter()
                .map(|m| {
                    let role = match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                    };
                    json!({ "role": role, "content": m.content })
                })
                .collect();
            let mut body = json!({ "model": config.model, "messages": msgs, "stream": stream });
            // OpenAI deprecated `max_tokens` in favour of `max_completion_tokens`
            // (required by reasoning models); compatible servers mostly only
            // understand `max_tokens`.
            let token_field = if config.provider == ProviderKind::OpenAi {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            body[token_field] = json!(params.max_output_tokens);
            if let Some(t) = params.temperature {
                body["temperature"] = json!(t);
            }
            if stream && config.provider == ProviderKind::OpenAi {
                body["stream_options"] = json!({ "include_usage": true });
            }
            WireRequest {
                url: format!("{base}/chat/completions"),
                headers,
                body,
            }
        }
        WireProtocol::AnthropicMessages => {
            let system: Vec<&str> = messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.content.as_str())
                .collect();
            let msgs: Vec<Value> = messages
                .iter()
                .filter(|m| m.role != Role::System)
                .map(|m| {
                    let role = if m.role == Role::Assistant {
                        "assistant"
                    } else {
                        "user"
                    };
                    json!({ "role": role, "content": m.content })
                })
                .collect();
            let mut body = json!({
                "model": config.model,
                "max_tokens": params.max_output_tokens,
                "messages": msgs,
            });
            if !system.is_empty() {
                body["system"] = json!(system.join("\n\n"));
            }
            if let Some(t) = params.temperature {
                body["temperature"] = json!(t);
            }
            if stream {
                body["stream"] = json!(true);
            }
            WireRequest {
                url: format!("{base}/messages"),
                headers,
                body,
            }
        }
        WireProtocol::GeminiGenerateContent => {
            let system: Vec<Value> = messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| json!({ "text": m.content }))
                .collect();
            let contents: Vec<Value> = messages
                .iter()
                .filter(|m| m.role != Role::System)
                .map(|m| {
                    let role = if m.role == Role::Assistant {
                        "model"
                    } else {
                        "user"
                    };
                    json!({ "role": role, "parts": [{ "text": m.content }] })
                })
                .collect();
            let mut generation = json!({ "maxOutputTokens": params.max_output_tokens });
            if let Some(t) = params.temperature {
                generation["temperature"] = json!(t);
            }
            let mut body = json!({ "contents": contents, "generationConfig": generation });
            if !system.is_empty() {
                body["systemInstruction"] = json!({ "parts": system });
            }
            let model = gemini_model_path(&config.model);
            let url = if stream {
                format!("{base}/models/{model}:streamGenerateContent?alt=sse")
            } else {
                format!("{base}/models/{model}:generateContent")
            };
            WireRequest { url, headers, body }
        }
    }
}

fn malformed(what: &str, body: &str) -> AiError {
    let mut snippet: String = body.chars().take(300).collect();
    if body.chars().count() > 300 {
        snippet.push_str("...");
    }
    AiError::MalformedResponse(format!("{what}: {snippet}"))
}

fn u32_field(v: &Value, key: &str) -> u32 {
    v.get(key)
        .and_then(Value::as_u64)
        .map(|n| n.min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

fn openai_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage").filter(|u| u.is_object())?;
    Some(Usage {
        input_tokens: u32_field(u, "prompt_tokens"),
        output_tokens: u32_field(u, "completion_tokens"),
    })
}

fn gemini_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usageMetadata").filter(|u| u.is_object())?;
    Some(Usage {
        input_tokens: u32_field(u, "promptTokenCount"),
        output_tokens: u32_field(u, "candidatesTokenCount") + u32_field(u, "thoughtsTokenCount"),
    })
}

/// Extracts visible text, finish reason and blocking info from one Gemini
/// `GenerateContentResponse` object.
fn gemini_chunk(v: &Value) -> Result<(String, Option<FinishReason>), AiError> {
    if let Some(err) = v.get("error") {
        return Err(AiError::Provider(error_object_message(err)));
    }
    if let Some(reason) = v
        .get("promptFeedback")
        .and_then(|p| p.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Err(AiError::Blocked(format!("prompt blocked ({reason})")));
    }
    let Some(candidate) = v.get("candidates").and_then(|c| c.get(0)) else {
        return Ok((String::new(), None));
    };
    let mut text = String::new();
    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    let finish = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .filter(|r| *r != "FINISH_REASON_UNSPECIFIED")
        .map(FinishReason::from_provider);
    Ok((text, finish))
}

/// Parses a non-streaming success body.
pub(crate) fn parse_chat_response(wire: WireProtocol, body: &str) -> Result<ChatResponse, AiError> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| malformed("response is not JSON", body))?;
    match wire {
        WireProtocol::OpenAiChat => {
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                return Err(AiError::Provider(error_object_message(err)));
            }
            let choice = v
                .get("choices")
                .and_then(|c| c.get(0))
                .ok_or_else(|| malformed("missing choices[0]", body))?;
            let message = choice
                .get("message")
                .ok_or_else(|| malformed("missing choices[0].message", body))?;
            let text = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
                    return Err(AiError::Blocked(refusal.to_string()));
                }
            }
            Ok(ChatResponse {
                text,
                finish_reason: choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .map(FinishReason::from_provider),
                usage: openai_usage(&v),
                model: v.get("model").and_then(Value::as_str).map(str::to_string),
            })
        }
        WireProtocol::AnthropicMessages => {
            if v.get("type").and_then(Value::as_str) == Some("error") {
                return Err(anthropic_stream_error(&v));
            }
            let blocks = v
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| malformed("missing content array", body))?;
            let text: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            let usage = v.get("usage").map(|u| Usage {
                input_tokens: u32_field(u, "input_tokens"),
                output_tokens: u32_field(u, "output_tokens"),
            });
            Ok(ChatResponse {
                text,
                finish_reason: v
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(FinishReason::from_provider),
                usage,
                model: v.get("model").and_then(Value::as_str).map(str::to_string),
            })
        }
        WireProtocol::GeminiGenerateContent => {
            if v.get("candidates").is_none()
                && v.get("promptFeedback").is_none()
                && v.get("error").is_none()
            {
                return Err(malformed("missing candidates", body));
            }
            let (text, finish_reason) = gemini_chunk(&v)?;
            Ok(ChatResponse {
                text,
                finish_reason,
                usage: gemini_usage(&v),
                model: v
                    .get("modelVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
    }
}

fn anthropic_stream_error(v: &Value) -> AiError {
    let err = v.get("error").cloned().unwrap_or(Value::Null);
    let kind = err.get("type").and_then(Value::as_str).unwrap_or("error");
    let message = error_object_message(&err);
    match kind {
        "overloaded_error" => AiError::Http {
            status: 529,
            message,
        },
        "rate_limit_error" => AiError::RateLimited {
            retry_after: None,
            message,
        },
        "authentication_error" | "permission_error" => AiError::Auth {
            status: 401,
            message,
        },
        _ => AiError::Provider(format!("{kind}: {message}")),
    }
}

/// Turns SSE events into [`ChatDelta`]s for one protocol.
pub(crate) struct StreamDecoder {
    wire: WireProtocol,
    finish: Option<FinishReason>,
    usage: Option<Usage>,
    done: bool,
}

impl StreamDecoder {
    pub fn new(wire: WireProtocol) -> Self {
        StreamDecoder {
            wire,
            finish: None,
            usage: None,
            done: false,
        }
    }

    fn done_delta(&mut self) -> ChatDelta {
        self.done = true;
        ChatDelta::Done {
            finish_reason: self.finish.clone(),
            usage: self.usage,
        }
    }

    /// Handles one event. Returns the deltas it produced.
    pub fn on_event(&mut self, event: SseEvent) -> Result<Vec<ChatDelta>, AiError> {
        if self.done {
            return Ok(Vec::new());
        }
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        match self.wire {
            WireProtocol::OpenAiChat => {
                if data == "[DONE]" {
                    out.push(self.done_delta());
                    return Ok(out);
                }
                let v: Value = serde_json::from_str(data)
                    .map_err(|_| malformed("stream chunk is not JSON", data))?;
                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                    return Err(AiError::Provider(error_object_message(err)));
                }
                if let Some(u) = openai_usage(&v) {
                    self.usage = Some(u);
                }
                if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
                    if let Some(text) = choice
                        .get("delta")
                        .and_then(|d| d.get("content"))
                        .and_then(Value::as_str)
                    {
                        if !text.is_empty() {
                            out.push(ChatDelta::Text(text.to_string()));
                        }
                    }
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        self.finish = Some(FinishReason::from_provider(reason));
                    }
                }
            }
            WireProtocol::AnthropicMessages => {
                let v: Value = serde_json::from_str(data)
                    .map_err(|_| malformed("stream event is not JSON", data))?;
                let kind = v
                    .get("type")
                    .and_then(Value::as_str)
                    .or(event.event.as_deref())
                    .unwrap_or_default();
                match kind {
                    "message_start" => {
                        if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                            self.usage = Some(Usage {
                                input_tokens: u32_field(u, "input_tokens"),
                                output_tokens: u32_field(u, "output_tokens"),
                            });
                        }
                    }
                    "content_block_delta" => {
                        let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                        if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    out.push(ChatDelta::Text(text.to_string()));
                                }
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(reason) = v
                            .get("delta")
                            .and_then(|d| d.get("stop_reason"))
                            .and_then(Value::as_str)
                        {
                            self.finish = Some(FinishReason::from_provider(reason));
                        }
                        if let Some(u) = v.get("usage") {
                            let usage = self.usage.get_or_insert_with(Usage::default);
                            usage.output_tokens = u32_field(u, "output_tokens");
                            if u.get("input_tokens").is_some() {
                                usage.input_tokens = u32_field(u, "input_tokens");
                            }
                        }
                    }
                    "message_stop" => out.push(self.done_delta()),
                    "error" => return Err(anthropic_stream_error(&v)),
                    // ping, content_block_start/stop and future event types.
                    _ => {}
                }
            }
            WireProtocol::GeminiGenerateContent => {
                let v: Value = serde_json::from_str(data)
                    .map_err(|_| malformed("stream chunk is not JSON", data))?;
                let (text, finish) = gemini_chunk(&v)?;
                if let Some(u) = gemini_usage(&v) {
                    self.usage = Some(u);
                }
                if !text.is_empty() {
                    out.push(ChatDelta::Text(text));
                }
                if finish.is_some() {
                    self.finish = finish;
                }
            }
        }
        Ok(out)
    }

    /// Called when the HTTP body ends.
    pub fn on_eof(&mut self) -> Result<Option<ChatDelta>, AiError> {
        if self.done {
            return Ok(None);
        }
        // Gemini signals completion only through `finishReason`; OpenAI
        // compatible servers occasionally omit `[DONE]` after a finish reason.
        if self.finish.is_some() {
            return Ok(Some(self.done_delta()));
        }
        Err(AiError::Network(
            "the stream ended before the provider signalled completion".to_string(),
        ))
    }
}

/// Best-effort message from a JSON error object (`{"message": ..}`) or string.
fn error_object_message(err: &Value) -> String {
    if let Some(s) = err.as_str() {
        return s.to_string();
    }
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = err
        .get("status")
        .or_else(|| err.get("type"))
        .or_else(|| err.get("code"))
        .and_then(|c| {
            c.as_str()
                .map(str::to_string)
                .or_else(|| c.as_i64().map(|n| n.to_string()))
        });
    match (message.is_empty(), code) {
        (false, Some(code)) => format!("{message} ({code})"),
        (false, None) => message.to_string(),
        (true, Some(code)) => code,
        (true, None) => err.to_string(),
    }
}

/// Extracts a readable message from an error response body of any of the
/// supported providers, falling back to a truncated raw body.
pub(crate) fn error_message_from_body(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        // Gemini wraps errors in an array when streaming.
        let v = v.as_array().and_then(|a| a.first().cloned()).unwrap_or(v);
        if let Some(err) = v.get("error") {
            return error_object_message(err);
        }
        for key in ["message", "detail", "msg"] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                return s.to_string();
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(empty response body)".to_string();
    }
    let mut snippet: String = trimmed.chars().take(500).collect();
    if trimmed.chars().count() > 500 {
        snippet.push_str("...");
    }
    snippet
}

/// Gemini reports quota back-off as `error.details[].retryDelay` ("37s").
pub(crate) fn retry_delay_from_body(body: &str) -> Option<std::time::Duration> {
    let v: Value = serde_json::from_str(body).ok()?;
    let v = v.as_array().and_then(|a| a.first().cloned()).unwrap_or(v);
    let details = v.get("error")?.get("details")?.as_array()?;
    details.iter().find_map(|d| {
        let raw = d.get("retryDelay")?.as_str()?;
        let secs: f64 = raw.trim().trim_end_matches('s').parse().ok()?;
        (secs.is_finite() && secs >= 0.0).then(|| std::time::Duration::from_secs_f64(secs))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent {
            event: event.map(str::to_string),
            data: data.to_string(),
        }
    }

    #[test]
    fn anthropic_skips_thinking_blocks() {
        let body = r#"{"type":"message","model":"claude-sonnet-5-5","content":[
            {"type":"thinking","thinking":"hmm","signature":"x"},
            {"type":"text","text":"Hello"},{"type":"text","text":" world"}],
            "stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":5}}"#;
        let r = parse_chat_response(WireProtocol::AnthropicMessages, body).unwrap();
        assert_eq!(r.text, "Hello world");
        assert_eq!(r.finish_reason, Some(FinishReason::Stop));
        assert_eq!(r.usage.unwrap().output_tokens, 5);
    }

    #[test]
    fn gemini_skips_thought_parts_and_reports_blocking() {
        let body = r#"{"candidates":[{"content":{"role":"model","parts":[
            {"text":"thinking...","thought":true},{"text":"Answer"}]},"finishReason":"MAX_TOKENS"}],
            "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"thoughtsTokenCount":7}}"#;
        let r = parse_chat_response(WireProtocol::GeminiGenerateContent, body).unwrap();
        assert_eq!(r.text, "Answer");
        assert_eq!(r.finish_reason, Some(FinishReason::Length));
        assert_eq!(r.usage.unwrap().output_tokens, 9);
        let blocked = r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#;
        assert!(matches!(
            parse_chat_response(WireProtocol::GeminiGenerateContent, blocked),
            Err(AiError::Blocked(_))
        ));
    }

    #[test]
    fn openai_stream_decoding_and_eof_rules() {
        let mut d = StreamDecoder::new(WireProtocol::OpenAiChat);
        let out = d
            .on_event(ev(None, r#"{"choices":[{"delta":{"role":"assistant","content":"Hi"}}]}"#))
            .unwrap();
        assert_eq!(out, vec![ChatDelta::Text("Hi".into())]);
        assert!(d.on_eof().is_err(), "no finish reason yet");
        d.on_event(ev(None, r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#))
            .unwrap();
        assert_eq!(
            d.on_event(ev(None, "[DONE]")).unwrap(),
            vec![ChatDelta::Done {
                finish_reason: Some(FinishReason::Length),
                usage: None
            }]
        );
        assert_eq!(d.on_eof().unwrap(), None);
    }

    #[test]
    fn anthropic_stream_error_event_maps_to_overload() {
        let mut d = StreamDecoder::new(WireProtocol::AnthropicMessages);
        let err = d
            .on_event(ev(
                Some("error"),
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ))
            .unwrap_err();
        assert!(err.is_retryable());
        assert!(matches!(err, AiError::Http { status: 529, .. }));
    }

    #[test]
    fn error_bodies_of_all_shapes() {
        assert_eq!(
            error_message_from_body(r#"{"error":{"message":"Invalid key","type":"invalid_request_error"}}"#),
            "Invalid key (invalid_request_error)"
        );
        assert_eq!(error_message_from_body(r#"{"error":"model not found"}"#), "model not found");
        assert_eq!(
            error_message_from_body(r#"[{"error":{"code":429,"message":"Quota","status":"RESOURCE_EXHAUSTED"}}]"#),
            "Quota (RESOURCE_EXHAUSTED)"
        );
        assert_eq!(error_message_from_body("<html>bad gateway</html>"), "<html>bad gateway</html>");
        assert_eq!(
            retry_delay_from_body(
                r#"{"error":{"code":429,"details":[{"@type":"x.RetryInfo","retryDelay":"37s"}]}}"#
            ),
            Some(std::time::Duration::from_secs(37))
        );
    }
}
