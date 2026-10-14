//! Error types for AI requests and agent edits.

use std::time::Duration;

/// Errors produced by [`crate::AiClient`] requests.
///
/// Messages never contain the configured API key: provider error bodies are
/// scrubbed before they are stored in an error value.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AiError {
    /// No usable provider configuration (for example a hosted provider
    /// without an API key).
    #[error("AI is not configured: {0}")]
    NotConfigured(String),
    /// The provider rejected the credentials (HTTP 401 or 403).
    #[error("authentication failed (HTTP {status}): {message}")]
    Auth {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Provider supplied error message.
        message: String,
    },
    /// The provider rate limited the request (HTTP 429).
    #[error("rate limited{}: {message}", format_retry(.retry_after))]
    RateLimited {
        /// How long the provider asked us to wait, when it said so.
        retry_after: Option<Duration>,
        /// Provider supplied error message.
        message: String,
    },
    /// Any other non-success HTTP status.
    #[error("provider returned HTTP {status}: {message}")]
    Http {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Provider supplied error message (or a truncated raw body).
        message: String,
    },
    /// The provider reported an error inside an otherwise successful
    /// response, for example an `error` event in the middle of a stream.
    #[error("provider error: {0}")]
    Provider(String),
    /// The provider refused to produce content (safety filter, blocked
    /// prompt, refusal).
    #[error("the provider blocked the request: {0}")]
    Blocked(String),
    /// The request did not complete within the configured timeout.
    #[error("the request timed out")]
    Timeout,
    /// Connection level failure (DNS, refused connection, TLS, reset).
    #[error("network error: {0}")]
    Network(String),
    /// The provider answered with a body we could not understand.
    #[error("malformed response: {0}")]
    MalformedResponse(String),
}

fn format_retry(retry_after: &Option<Duration>) -> String {
    match retry_after {
        Some(d) => format!(" (retry after {:.1}s)", d.as_secs_f32()),
        None => String::new(),
    }
}

impl AiError {
    /// Returns `true` for errors that may succeed when retried later
    /// (rate limits, overload, 5xx, timeouts and connection failures).
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::RateLimited { .. } | AiError::Timeout | AiError::Network(_) => true,
            AiError::Http { status, .. } => *status >= 500 || *status == 408,
            _ => false,
        }
    }

    /// The provider requested back-off delay, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            AiError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// Errors produced while turning a model response into an edited file.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EditError {
    /// A SEARCH block did not match the file, even with whitespace tolerance.
    #[error(
        "SEARCH block {} of {total} does not match {file_path}{}:\n{search_preview}",
        .block_index + 1,
        closest_hint(.closest_line)
    )]
    SearchNotFound {
        /// Path of the file being edited.
        file_path: String,
        /// Zero based index of the failing block.
        block_index: usize,
        /// Total number of blocks for this file.
        total: usize,
        /// The first lines of the SEARCH text, for display.
        search_preview: String,
        /// 1-based line of the most similar region in the file, if any.
        closest_line: Option<usize>,
    },
    /// The response contained neither SEARCH/REPLACE blocks nor a usable
    /// full-file code block.
    #[error("the model response contains no SEARCH/REPLACE block or full file")]
    NoEditFound,
    /// The fallback code block looks like a partial snippet, not a full file.
    #[error("the model returned a partial snippet instead of the full file ({0})")]
    PartialFile(String),
    /// A SEARCH/REPLACE block was opened but never terminated correctly.
    #[error("malformed SEARCH/REPLACE block starting at response line {line}: {reason}")]
    MalformedBlock {
        /// 1-based line in the model response.
        line: usize,
        /// What was wrong.
        reason: String,
    },
    /// Applying the edits produced exactly the original text.
    #[error("the proposed edit does not change {0}")]
    NoChanges(String),
    /// The model stopped early because it hit the output token limit.
    #[error("the model response was cut off at the output token limit; raise max_output_tokens or narrow the request")]
    Truncated,
}

fn closest_hint(line: &Option<usize>) -> String {
    match line {
        Some(l) => format!(" (closest similar text near line {l})"),
        None => String::new(),
    }
}

/// Error returned by [`crate::propose_edit`]: either the request failed or
/// the response could not be applied.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProposeError {
    /// The model request failed.
    #[error(transparent)]
    Ai(#[from] AiError),
    /// The model answered but the edit could not be applied.
    #[error("{error}")]
    Edit {
        /// What went wrong while applying.
        error: EditError,
        /// The raw model response, so the UI can show it to the user.
        raw_response: String,
    },
}
