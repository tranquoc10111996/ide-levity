//! Multi-provider AI client and Aider-style SEARCH/REPLACE code editing protocol for IDE Levity.
//!
//! Supports OpenAI Chat Completions, Anthropic Messages, Google Gemini `generateContent`,
//! OpenRouter, Groq, DeepSeek, Mistral, xAI, Together AI, and local Ollama / LM Studio servers.

pub mod client;
pub mod config;
pub mod edit;
pub mod error;
pub mod provider;
mod sse;
pub mod types;
mod wire;

pub use client::{collect_stream, AiClient, ChatStream};
pub use config::{
    default_config_paths, guess_provider_from_key, ollama_host_to_base_url, pick_local_model,
    resolve_config, setup_instructions, AiConfig, AiFileConfig, AiSetup, ConfigSource, EnvSnapshot,
    ResolveOptions, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_PROBE_TIMEOUT, DEFAULT_TIMEOUT, ENV_API_KEY,
    ENV_BASE_URL, ENV_CONFIG, ENV_MODEL, ENV_PROVIDER,
};
pub use edit::{
    apply_edit_blocks, explanation_text, extract_full_file, parse_edit_blocks, path_matches,
    EditBlock,
};
pub use error::{AiError, EditError, ProposeError};
pub use provider::{ProviderKind, WireProtocol};
pub use types::{ChatDelta, ChatMessage, ChatOptions, ChatResponse, FinishReason, Role, Usage};

/// Additional read-only file context attached to an edit or chat request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    /// Workspace-relative or absolute path of the context file.
    pub path: String,
    /// UTF-8 file text.
    pub content: String,
}

/// Request to edit a file using an AI model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
    /// Natural-language instruction from the user.
    pub instruction: String,
    /// Path of the target file being edited.
    pub file_path: String,
    /// Optional language identifier (e.g. `"rust"`, `"typescript"`).
    pub language: String,
    /// Current content of the target file.
    pub file_text: String,
    /// Optional 0-based inclusive `(start_line, end_line)` selection hint.
    pub selection: Option<(u32, u32)>,
    /// Additional context files to include in the prompt.
    pub extra_context: Vec<ContextFile>,
}

/// Proposed file edit produced by [`propose_edit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditProposal {
    /// Complete updated file text after applying SEARCH/REPLACE blocks (or full-file fallback).
    pub new_text: String,
    /// Human-readable explanation extracted from the model response.
    pub explanation: String,
    /// Raw model response text.
    pub raw_response: String,
    /// Number of SEARCH/REPLACE blocks applied (`0` when full-file fallback was used).
    pub blocks_applied: usize,
}

const EDIT_SYSTEM_PROMPT: &str = "\
You are an expert software engineering assistant integrated into IDE Levity.
When asked to modify code, respond with a short explanation followed by one or more SEARCH/REPLACE blocks in this exact format:

<file_path>
<<<<<<< SEARCH
<exact existing lines from the file>
=======
<replacement lines>
>>>>>>> REPLACE

Rules for SEARCH/REPLACE blocks:
1. Every SEARCH section must match the existing lines in the file, including indentation.
2. Keep each SEARCH block concise but unique enough to identify the exact location.
3. Do not use placeholders like `// ... existing code ...` inside SEARCH or REPLACE sections.
4. To insert into an empty file or append to the end, leave the SEARCH section empty.
5. You may return multiple SEARCH/REPLACE blocks in top-to-bottom order.";

/// Builds the system and user messages for an [`EditRequest`].
pub fn build_edit_messages(req: &EditRequest) -> Vec<ChatMessage> {
    let mut user = String::new();
    for ctx in &req.extra_context {
        user.push_str(&format!(
            "Context file `{}`:\n```\n{}\n```\n\n",
            ctx.path, ctx.content
        ));
    }
    user.push_str(&format!(
        "Target file `{}`:\n```{}\n{}\n```\n\n",
        req.file_path, req.language, req.file_text
    ));
    if let Some((start, end)) = req.selection {
        user.push_str(&format!(
            "Selected lines (1-based): {}..={}\n\n",
            start + 1,
            end + 1
        ));
    }
    user.push_str("Instruction:\n");
    user.push_str(req.instruction.trim());
    vec![
        ChatMessage::system(EDIT_SYSTEM_PROMPT),
        ChatMessage::user(user),
    ]
}

/// Parses and applies a [`ChatResponse`] to an [`EditRequest`].
pub fn apply_response_to_file(
    req: &EditRequest,
    response: &ChatResponse,
) -> Result<EditProposal, ProposeError> {
    let raw = response.text.clone();
    let fail = |error: EditError| ProposeError::Edit {
        error,
        raw_response: raw.clone(),
    };
    let blocks = match parse_edit_blocks(&raw) {
        Ok(b) => b,
        Err(err) => {
            if response.finish_reason == Some(FinishReason::Length) {
                return Err(fail(EditError::Truncated));
            }
            return Err(fail(err));
        }
    };

    let matching_blocks: Vec<EditBlock> = blocks
        .iter()
        .filter(|b| {
            b.path
                .as_deref()
                .is_none_or(|p| path_matches(p, &req.file_path))
        })
        .cloned()
        .collect();
    let active_blocks = if matching_blocks.is_empty() {
        blocks
    } else {
        matching_blocks
    };

    let hint_line = req.selection.map(|(s, _)| s as usize);
    let (new_text, blocks_applied) = if !active_blocks.is_empty() {
        let count = active_blocks.len();
        let applied =
            apply_edit_blocks(&req.file_path, &req.file_text, &active_blocks, hint_line)
                .map_err(fail)?;
        (applied, count)
    } else {
        if response.finish_reason == Some(FinishReason::Length) {
            return Err(fail(EditError::Truncated));
        }
        let full = extract_full_file(&raw, &req.file_text).map_err(fail)?;
        (full, 0)
    };

    if new_text == req.file_text {
        return Err(fail(EditError::NoChanges(req.file_path.clone())));
    }

    let mut explanation = explanation_text(&raw);
    if explanation.is_empty() {
        explanation = format!("Applied edit to {}", req.file_path);
    }

    Ok(EditProposal {
        new_text,
        explanation,
        raw_response: raw,
        blocks_applied,
    })
}

/// Requests a code edit from `client` and applies the resulting SEARCH/REPLACE blocks.
pub async fn propose_edit(
    client: &AiClient,
    req: &EditRequest,
) -> Result<EditProposal, ProposeError> {
    let messages = build_edit_messages(req);
    let response = client.chat(&messages, &ChatOptions::default()).await?;
    apply_response_to_file(req, &response)
}

/// Sends a general coding question with optional file context and returns the answer text.
pub async fn ask(
    client: &AiClient,
    question: &str,
    context: &[ContextFile],
) -> Result<String, AiError> {
    let mut user = String::new();
    for ctx in context {
        user.push_str(&format!("File `{}`:\n```\n{}\n```\n\n", ctx.path, ctx.content));
    }
    user.push_str(question.trim());
    let messages = vec![
        ChatMessage::system(
            "You are a concise, accurate coding assistant inside IDE Levity.",
        ),
        ChatMessage::user(user),
    ];
    let resp = client.chat(&messages, &ChatOptions::default()).await?;
    Ok(resp.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_server<F>(handler: F) -> String
    where
        F: Fn(String) -> (u16, Vec<(&'static str, String)>, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = std::sync::Arc::new(handler);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut temp = [0u8; 4096];
                    let mut header_end = None;
                    let mut content_len = 0usize;
                    loop {
                        let Ok(n) = stream.read(&mut temp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&temp[..n]);
                        if header_end.is_none() {
                            if let Some(pos) =
                                buf.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                let end = pos + 4;
                                header_end = Some(end);
                                let headers_str = String::from_utf8_lossy(&buf[..end]);
                                for line in headers_str.lines() {
                                    if let Some((k, v)) = line.split_once(':') {
                                        if k.trim().eq_ignore_ascii_case("content-length") {
                                            content_len = v.trim().parse().unwrap_or(0);
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(end) = header_end {
                            if buf.len() >= end + content_len {
                                break;
                            }
                        }
                    }
                    let req_text = String::from_utf8_lossy(&buf).into_owned();
                    let (status, extra_headers, body) = handler(req_text);
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        429 => "Too Many Requests",
                        _ => "Error",
                    };
                    let mut resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in extra_headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");
                    resp.push_str(&body);
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn openai_chat_and_propose_edit_against_mock_server() {
        let base_url = spawn_mock_server(|req| {
            assert!(req.to_ascii_lowercase().contains("authorization: bearer sk-test-key"));
            assert!(req.contains("POST /v1/chat/completions"));
            let body = serde_json::json!({
                "id": "chatcmpl-1",
                "model": "gpt-6.1-sol",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Updated greeting.\n\nsrc/main.rs\n<<<<<<< SEARCH\n    println!(\"old\");\n=======\n    println!(\"new\");\n>>>>>>> REPLACE\n"
                    },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 20, "completion_tokens": 15 }
            })
            .to_string();
            (200, vec![("content-type", "application/json".into())], body)
        })
        .await;

        let cfg = AiConfig::new(ProviderKind::OpenAi, "gpt-6.1-sol")
            .with_base_url(base_url)
            .with_api_key("sk-test-key");
        let client = AiClient::new(cfg).unwrap();
        let req = EditRequest {
            instruction: "Change old to new".into(),
            file_path: "src/main.rs".into(),
            language: "rust".into(),
            file_text: "fn main() {\n    println!(\"old\");\n}\n".into(),
            selection: None,
            extra_context: Vec::new(),
        };
        let proposal = propose_edit(&client, &req).await.unwrap();
        assert_eq!(proposal.new_text, "fn main() {\n    println!(\"new\");\n}\n");
        assert_eq!(proposal.explanation, "Updated greeting.");
        assert_eq!(proposal.blocks_applied, 1);
    }

    #[tokio::test]
    async fn anthropic_and_gemini_streaming_against_mock_server() {
        let anthropic_url = spawn_mock_server(|req| {
            assert!(req.to_ascii_lowercase().contains("x-api-key: sk-ant-secret"));
            assert!(req.to_ascii_lowercase().contains("anthropic-version: 2023-06-01"));
            let sse = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Xin \"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"chào!\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            (200, vec![("content-type", "text/event-stream".into())], sse.into())
        })
        .await;

        let client = AiClient::new(
            AiConfig::new(ProviderKind::Anthropic, "claude-sonnet-5-5")
                .with_base_url(anthropic_url)
                .with_api_key("sk-ant-secret"),
        )
        .unwrap();
        let resp = collect_stream(
            client.chat_stream(vec![ChatMessage::user("Hi")], ChatOptions::default()),
        )
        .await
        .unwrap();
        assert_eq!(resp.text, "Xin chào!");
        assert_eq!(resp.finish_reason, Some(FinishReason::Stop));

        let gemini_url = spawn_mock_server(|req| {
            assert!(req.to_ascii_lowercase().contains("x-goog-api-key: aiza-test"));
            let sse = concat!(
                "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Gemini \"}]}}]}\n\n",
                "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"OK\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n\n"
            );
            (200, vec![("content-type", "text/event-stream".into())], sse.into())
        })
        .await;

        let gem_client = AiClient::new(
            AiConfig::new(ProviderKind::Gemini, "gemini-3.8-flash")
                .with_base_url(gemini_url)
                .with_api_key("AIza-test"),
        )
        .unwrap();
        let gem_resp = collect_stream(
            gem_client.chat_stream(vec![ChatMessage::user("Hi")], ChatOptions::default()),
        )
        .await
        .unwrap();
        assert_eq!(gem_resp.text, "Gemini OK");
        assert_eq!(gem_resp.finish_reason, Some(FinishReason::Stop));
    }

    #[tokio::test]
    async fn auth_and_rate_limit_errors_scrub_api_key() {
        let url_401 = spawn_mock_server(|_| {
            let body = r#"{"error":{"message":"Invalid key sk-top-secret-key"}}"#;
            (401, vec![], body.into())
        })
        .await;
        let client = AiClient::new(
            AiConfig::new(ProviderKind::OpenAi, "gpt-6.1-sol")
                .with_base_url(url_401)
                .with_api_key("sk-top-secret-key"),
        )
        .unwrap();
        let err = client
            .chat(
                &[ChatMessage::user("hi")],
                &ChatOptions {
                    max_retries: 0,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AiError::Auth { status: 401, .. }));
        assert!(!err.to_string().contains("sk-top-secret-key"));

        let url_429 = spawn_mock_server(|_| {
            let body = r#"{"error":{"message":"Rate limit reached"}}"#;
            (429, vec![("retry-after", "45".into())], body.into())
        })
        .await;
        let client_429 = AiClient::new(
            AiConfig::new(ProviderKind::OpenAi, "gpt-6.1-sol")
                .with_base_url(url_429)
                .with_api_key("sk-top-secret-key"),
        )
        .unwrap();
        let err = client_429
            .chat(
                &[ChatMessage::user("hi")],
                &ChatOptions {
                    max_retries: 0,
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.retry_after(), Some(std::time::Duration::from_secs(45)));
    }
}
