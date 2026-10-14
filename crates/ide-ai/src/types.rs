//! Provider-neutral chat request and response types.

/// Author of a chat message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// System / developer instructions.
    System,
    /// The human user.
    User,
    /// The model.
    Assistant,
}

/// One message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// Who wrote the message.
    pub role: Role,
    /// Plain text content.
    pub content: String,
}

impl ChatMessage {
    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::System,
            content: content.into(),
        }
    }

    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::User,
            content: content.into(),
        }
    }

    /// An assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        ChatMessage {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Per-request overrides of [`crate::AiConfig`] settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatOptions {
    /// Overrides `AiConfig::max_output_tokens`.
    pub max_output_tokens: Option<u32>,
    /// Overrides `AiConfig::temperature`.
    pub temperature: Option<f32>,
    /// How many times to retry rate limits (waits of at most 30 s), 5xx
    /// responses and connection failures before any output was received.
    pub max_retries: u32,
}

impl Default for ChatOptions {
    fn default() -> Self {
        ChatOptions {
            max_output_tokens: None,
            temperature: None,
            max_retries: 2,
        }
    }
}

/// Normalized reason why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural end of the answer (or a stop sequence).
    Stop,
    /// The output token limit was reached; the answer is truncated.
    Length,
    /// A safety filter or refusal ended the answer.
    ContentFilter,
    /// Any other provider specific reason (raw value).
    Other(String),
}

impl FinishReason {
    /// Maps raw provider values (OpenAI, Anthropic and Gemini spellings).
    pub fn from_provider(raw: &str) -> Self {
        match raw {
            "stop" | "end_turn" | "stop_sequence" | "STOP" | "eos" => FinishReason::Stop,
            "length" | "max_tokens" | "MAX_TOKENS" | "model_context_window_exceeded" => {
                FinishReason::Length
            }
            "content_filter" | "refusal" | "SAFETY" | "RECITATION" | "BLOCKLIST"
            | "PROHIBITED_CONTENT" | "SPII" | "IMAGE_SAFETY" => FinishReason::ContentFilter,
            other => FinishReason::Other(other.to_string()),
        }
    }
}

/// Token accounting reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u32,
    /// Generated tokens (including reasoning tokens where reported).
    pub output_tokens: u32,
}

/// A complete (non-streaming) model answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatResponse {
    /// The visible answer text (reasoning/thinking content is excluded).
    pub text: String,
    /// Why generation stopped, when reported.
    pub finish_reason: Option<FinishReason>,
    /// Token usage, when reported.
    pub usage: Option<Usage>,
    /// Model that actually served the request, when reported.
    pub model: Option<String>,
}

/// One item of a streaming answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatDelta {
    /// More answer text.
    Text(String),
    /// The answer is complete; always the last successful item.
    Done {
        /// Why generation stopped, when reported.
        finish_reason: Option<FinishReason>,
        /// Token usage, when reported.
        usage: Option<Usage>,
    },
}
