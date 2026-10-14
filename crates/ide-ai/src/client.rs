//! HTTP client for chat completions across all supported providers.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use serde_json::Value;

use crate::config::AiConfig;
use crate::error::AiError;
use crate::provider::WireProtocol;
use crate::sse::SseParser;
use crate::types::{ChatDelta, ChatMessage, ChatOptions, ChatResponse, FinishReason};
use crate::wire::{self, GenParams, StreamDecoder};

/// Boxed stream of answer deltas returned by [`AiClient::chat_stream`].
/// It ends after a [`ChatDelta::Done`] item or after the first error.
pub type ChatStream = Pin<Box<dyn Stream<Item = Result<ChatDelta, AiError>> + Send>>;

/// Longest provider-requested wait that automatic retries will honour.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// A cheap-to-clone, thread-safe client bound to one [`AiConfig`].
#[derive(Clone)]
pub struct AiClient {
    inner: Arc<Inner>,
}

struct Inner {
    config: AiConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for AiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiClient")
            .field("config", &self.inner.config)
            .finish()
    }
}

impl AiClient {
    /// Creates a client. Fails with [`AiError::NotConfigured`] when the
    /// provider needs an API key and none is set, or the base URL is empty.
    pub fn new(config: AiConfig) -> Result<Self, AiError> {
        if config.base_url.trim().is_empty() {
            return Err(AiError::NotConfigured(format!(
                "{} needs a base URL",
                config.provider.display_name()
            )));
        }
        if config.provider.requires_api_key() && config.api_key.is_none() {
            return Err(AiError::NotConfigured(format!(
                "{} needs an API key ({})",
                config.provider.display_name(),
                config.provider.api_key_env_vars().join(" / ")
            )));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10).min(config.timeout))
            .read_timeout(config.timeout)
            .user_agent(concat!("ide-levity/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AiError::Network(e.to_string()))?;
        Ok(AiClient {
            inner: Arc::new(Inner { config, http }),
        })
    }

    /// The configuration this client was built with.
    pub fn config(&self) -> &AiConfig {
        &self.inner.config
    }

    fn scrub(&self, text: String) -> String {
        match self.inner.config.api_key.as_deref() {
            Some(key) if key.len() >= 4 && text.contains(key) => text.replace(key, "<redacted>"),
            _ => text,
        }
    }

    fn scrub_error(&self, err: AiError) -> AiError {
        match err {
            AiError::Auth { status, message } => AiError::Auth {
                status,
                message: self.scrub(message),
            },
            AiError::RateLimited {
                retry_after,
                message,
            } => AiError::RateLimited {
                retry_after,
                message: self.scrub(message),
            },
            AiError::Http { status, message } => AiError::Http {
                status,
                message: self.scrub(message),
            },
            AiError::Provider(m) => AiError::Provider(self.scrub(m)),
            AiError::Blocked(m) => AiError::Blocked(self.scrub(m)),
            AiError::Network(m) => AiError::Network(self.scrub(m)),
            AiError::MalformedResponse(m) => AiError::MalformedResponse(self.scrub(m)),
            other => other,
        }
    }

    fn params(&self, options: &ChatOptions) -> GenParams {
        GenParams {
            max_output_tokens: options
                .max_output_tokens
                .unwrap_or(self.inner.config.max_output_tokens),
            temperature: options.temperature.or(self.inner.config.temperature),
        }
    }

    /// Sends the request once and maps non-success statuses to errors.
    async fn send_once(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        stream: bool,
    ) -> Result<reqwest::Response, AiError> {
        let config = &self.inner.config;
        let request = wire::build_chat_request(config, messages, &self.params(options), stream);
        let mut builder = self.inner.http.post(&request.url).json(&request.body);
        for (name, value) in &request.headers {
            builder = builder.header(*name, value);
        }
        if stream {
            builder = builder.header("accept", "text/event-stream");
        } else {
            builder = builder.timeout(config.timeout);
        }
        let response = builder.send().await.map_err(map_reqwest_error)?;
        check_status(response).await
    }

    /// Sends with automatic retries for transient failures.
    async fn send(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        stream: bool,
    ) -> Result<reqwest::Response, AiError> {
        let mut attempt = 0;
        loop {
            match self.send_once(messages, options, stream).await {
                Ok(response) => return Ok(response),
                Err(err) => {
                    let err = self.scrub_error(err);
                    let retryable = err.is_retryable() && !matches!(err, AiError::Timeout);
                    if !retryable || attempt >= options.max_retries {
                        return Err(err);
                    }
                    let backoff = Duration::from_millis(500 * (1 << attempt.min(5)));
                    let wait = match err.retry_after() {
                        Some(wait) if wait > MAX_RETRY_WAIT => return Err(err),
                        Some(wait) => wait,
                        None => backoff,
                    };
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Sends a conversation and waits for the complete answer.
    ///
    /// Returns [`AiError::Blocked`] when the provider produced no text because
    /// of a safety filter or refusal.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResponse, AiError> {
        let response = self.send(messages, options, false).await?;
        let body = response.text().await.map_err(map_reqwest_error)?;
        let parsed = wire::parse_chat_response(self.inner.config.wire(), &body)
            .map_err(|e| self.scrub_error(e))?;
        if parsed.text.is_empty() && parsed.finish_reason == Some(FinishReason::ContentFilter) {
            return Err(AiError::Blocked(
                "the provider's safety filter stopped the answer".to_string(),
            ));
        }
        Ok(parsed)
    }

    /// Streams the answer as it is generated.
    ///
    /// The request is sent lazily when the stream is first polled; request
    /// errors arrive as the first (and last) item. Safe to drive from any
    /// tokio-based executor, e.g. `iced::Task::run`.
    pub fn chat_stream(&self, messages: Vec<ChatMessage>, options: ChatOptions) -> ChatStream {
        let state = StreamState::Init {
            client: self.clone(),
            messages,
            options,
        };
        Box::pin(futures_util::stream::unfold(state, stream_step))
    }

    /// Lists model IDs available to this account / server (first page only).
    pub async fn list_models(&self) -> Result<Vec<String>, AiError> {
        let config = &self.inner.config;
        let url = match config.wire() {
            WireProtocol::OpenAiChat => format!("{}/models", config.base_url),
            WireProtocol::AnthropicMessages => format!("{}/models?limit=1000", config.base_url),
            WireProtocol::GeminiGenerateContent => {
                format!("{}/models?pageSize=1000", config.base_url)
            }
        };
        let mut builder = self.inner.http.get(&url).timeout(config.timeout);
        for (name, value) in wire::auth_headers(config) {
            builder = builder.header(name, value);
        }
        let result = async {
            let response = builder.send().await.map_err(map_reqwest_error)?;
            let response = check_status(response).await?;
            let body = response.text().await.map_err(map_reqwest_error)?;
            let v: Value = serde_json::from_str(&body)
                .map_err(|_| AiError::MalformedResponse("model list is not JSON".into()))?;
            Ok(match config.wire() {
                WireProtocol::GeminiGenerateContent => v
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|models| {
                        models
                            .iter()
                            .filter(|m| {
                                m.get("supportedGenerationMethods")
                                    .and_then(Value::as_array)
                                    .is_none_or(|methods| {
                                        methods.iter().any(|x| x == "generateContent")
                                    })
                            })
                            .filter_map(|m| m.get("name").and_then(Value::as_str))
                            .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => model_ids(&v),
            })
        }
        .await;
        result.map_err(|e| self.scrub_error(e))
    }
}

fn model_ids(v: &Value) -> Vec<String> {
    v.get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Probes an OpenAI-compatible `/models` endpoint without proxies.
/// Returns `None` when the server is unreachable or answers with an error.
pub(crate) async fn probe_models(base_url: &str, timeout: Duration) -> Option<Vec<String>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .ok()?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let v: Value = response.json().await.ok()?;
    Some(model_ids(&v))
}

fn map_reqwest_error(err: reqwest::Error) -> AiError {
    if err.is_timeout() {
        return AiError::Timeout;
    }
    // Strip the URL: it never holds keys here, but keeps messages short.
    let err = err.without_url();
    let mut message = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(inner) = source {
        let text = inner.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = inner.source();
    }
    if message.to_ascii_lowercase().contains("timed out") {
        return AiError::Timeout;
    }
    AiError::Network(message)
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        if ms.is_finite() && ms >= 0.0 {
            return Some(Duration::from_secs_f64(ms / 1000.0));
        }
    }
    let secs = get("retry-after")?.trim().parse::<f64>().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, AiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = parse_retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    let message = wire::error_message_from_body(&body);
    let code = status.as_u16();
    Err(match code {
        401 | 403 => AiError::Auth {
            status: code,
            message,
        },
        429 => AiError::RateLimited {
            retry_after: retry_after.or_else(|| wire::retry_delay_from_body(&body)),
            message,
        },
        _ => AiError::Http {
            status: code,
            message,
        },
    })
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Vec<u8>>> + Send>>;

enum StreamState {
    Init {
        client: AiClient,
        messages: Vec<ChatMessage>,
        options: ChatOptions,
    },
    Active {
        client: AiClient,
        body: ByteStream,
        parser: SseParser,
        decoder: StreamDecoder,
        pending: VecDeque<Result<ChatDelta, AiError>>,
        eof: bool,
    },
    Finished,
}

async fn stream_step(
    mut state: StreamState,
) -> Option<(Result<ChatDelta, AiError>, StreamState)> {
    loop {
        match state {
            StreamState::Finished => return None,
            StreamState::Init {
                client,
                messages,
                options,
            } => match client.send(&messages, &options, true).await {
                Ok(response) => {
                    let body: ByteStream =
                        Box::pin(response.bytes_stream().map(|r| r.map(|b| b.to_vec())));
                    let wire = client.config().wire();
                    state = StreamState::Active {
                        client,
                        body,
                        parser: SseParser::new(),
                        decoder: StreamDecoder::new(wire),
                        pending: VecDeque::new(),
                        eof: false,
                    };
                }
                Err(err) => return Some((Err(err), StreamState::Finished)),
            },
            StreamState::Active {
                client,
                mut body,
                mut parser,
                mut decoder,
                mut pending,
                eof,
            } => {
                if let Some(item) = pending.pop_front() {
                    let terminal = !matches!(item, Ok(ChatDelta::Text(_)));
                    let item = item.map_err(|e| client.scrub_error(e));
                    let next = if terminal {
                        StreamState::Finished
                    } else {
                        StreamState::Active {
                            client,
                            body,
                            parser,
                            decoder,
                            pending,
                            eof,
                        }
                    };
                    return Some((item, next));
                }
                if eof {
                    return None;
                }
                let mut eof = false;
                match body.next().await {
                    Some(Ok(chunk)) => {
                        for event in parser.push(&chunk) {
                            match decoder.on_event(event) {
                                Ok(deltas) => pending.extend(deltas.into_iter().map(Ok)),
                                Err(e) => pending.push_back(Err(e)),
                            }
                        }
                    }
                    Some(Err(err)) => {
                        return Some((
                            Err(client.scrub_error(map_reqwest_error(err))),
                            StreamState::Finished,
                        ))
                    }
                    None => {
                        eof = true;
                        if let Some(event) = parser.finish() {
                            match decoder.on_event(event) {
                                Ok(deltas) => pending.extend(deltas.into_iter().map(Ok)),
                                Err(e) => pending.push_back(Err(e)),
                            }
                        }
                        match decoder.on_eof() {
                            Ok(Some(done)) => pending.push_back(Ok(done)),
                            Ok(None) => {}
                            Err(e) => pending.push_back(Err(e)),
                        }
                    }
                }
                state = StreamState::Active {
                    client,
                    body,
                    parser,
                    decoder,
                    pending,
                    eof,
                };
            }
        }
    }
}

/// Collects a [`ChatStream`] into a single [`ChatResponse`].
pub async fn collect_stream(mut stream: ChatStream) -> Result<ChatResponse, AiError> {
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item? {
            ChatDelta::Text(t) => text.push_str(&t),
            ChatDelta::Done {
                finish_reason,
                usage,
            } => {
                return Ok(ChatResponse {
                    text,
                    finish_reason,
                    usage,
                    model: None,
                })
            }
        }
    }
    Err(AiError::Network(
        "the stream ended before the provider signalled completion".to_string(),
    ))
}
