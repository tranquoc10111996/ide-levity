//! JSON-RPC 2.0 message model and the LSP `Content-Length` base-protocol framing.
//!
//! Messages are classified structurally (not via `#[serde(untagged)]` guessing):
//! an object with `method` and `id` is a request, `method` without `id` is a
//! notification, and anything else carrying an `id` is a response. Request ids may
//! be numbers or strings, as allowed by the specification.

use crate::LspError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Largest message body accepted from a server (256 MiB). Anything larger is treated
/// as a corrupted stream rather than allocated blindly.
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// JSON-RPC 2.0 request id (`number | string`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id (what this client always generates).
    Number(i64),
    /// String id (used by some servers for server-to-client requests).
    String(String),
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        RequestId::Number(value)
    }
}

impl From<&str> for RequestId {
    fn from(value: &str) -> Self {
        RequestId::String(value.to_string())
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::String(s) => write!(f, "{s:?}"),
        }
    }
}

/// JSON-RPC 2.0 Request object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Correlation id.
    pub id: RequestId,
    /// Method name, e.g. `textDocument/hover`.
    pub method: String,
    /// Optional parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 Error payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorPayload {
    /// Error code (see [`error_codes`]).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 Response object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Id of the request being answered. `None` only for errors about unparsable requests.
    pub id: Option<RequestId>,
    /// Result value (`null` is represented as `None` or `Some(Value::Null)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error, if the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorPayload>,
}

impl JsonRpcResponse {
    /// Builds a successful response. A `null` result is serialized explicitly, as the
    /// specification requires `result` to be present on success.
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    /// Builds an error response.
    pub fn failure(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            result: None,
            error: Some(JsonRpcErrorPayload {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// JSON-RPC 2.0 Notification object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Method name, e.g. `textDocument/publishDiagnostics`.
    pub method: String,
    /// Optional parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// Convenience constructor.
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.into(),
            params,
        }
    }
}

/// Unified JSON-RPC 2.0 message envelope.
#[derive(Debug, Clone)]
pub enum JsonRpcMessage {
    /// A request expecting a response.
    Request(JsonRpcRequest),
    /// A response to an earlier request.
    Response(JsonRpcResponse),
    /// A one-way notification.
    Notification(JsonRpcNotification),
}

impl JsonRpcMessage {
    /// Classifies and parses a JSON value into a message.
    pub fn from_value(value: Value) -> Result<Self, LspError> {
        let obj = value
            .as_object()
            .ok_or_else(|| LspError::Protocol("JSON-RPC message is not an object".into()))?;
        let has_method = obj.contains_key("method");
        let has_id = obj.get("id").is_some_and(|v| !v.is_null());
        let msg = if has_method && has_id {
            JsonRpcMessage::Request(serde_json::from_value(value)?)
        } else if has_method {
            JsonRpcMessage::Notification(serde_json::from_value(value)?)
        } else if obj.contains_key("id") {
            let has_null_result = obj.get("result") == Some(&Value::Null);
            let mut resp: JsonRpcResponse = serde_json::from_value(value)?;
            if has_null_result && resp.result.is_none() {
                resp.result = Some(Value::Null);
            }
            JsonRpcMessage::Response(resp)
        } else {
            return Err(LspError::Protocol(
                "JSON-RPC message has neither `method` nor `id`".into(),
            ));
        };
        Ok(msg)
    }

    /// Parses a message from raw JSON bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, LspError> {
        let value: Value = serde_json::from_slice(bytes)?;
        Self::from_value(value)
    }
}

impl Serialize for JsonRpcMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            JsonRpcMessage::Request(r) => r.serialize(serializer),
            JsonRpcMessage::Response(r) => r.serialize(serializer),
            JsonRpcMessage::Notification(n) => n.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        JsonRpcMessage::from_value(value).map_err(serde::de::Error::custom)
    }
}

/// Standard JSON-RPC and LSP error codes.
pub mod error_codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON sent is not a valid request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;
    /// The server received a request before `initialize`.
    pub const SERVER_NOT_INITIALIZED: i64 = -32002;
    /// The server cancelled the request.
    pub const SERVER_CANCELLED: i64 = -32802;
    /// The document changed while the request was computed.
    pub const CONTENT_MODIFIED: i64 = -32801;
    /// The client cancelled the request.
    pub const REQUEST_CANCELLED: i64 = -32800;
}

/// Encodes a `JsonRpcMessage` using the standard LSP `Content-Length` header framing.
pub fn encode_lsp_frame(msg: &JsonRpcMessage) -> Result<Vec<u8>, LspError> {
    let body = serde_json::to_vec(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Reads the raw body of one `Content-Length`-framed message.
///
/// Returns `Ok(None)` on a clean EOF between messages. Header names are matched
/// case-insensitively and unknown headers (such as `Content-Type`) are ignored.
/// Partial reads are handled by the buffered reader (`read_line` / `read_exact`).
pub async fn read_lsp_frame_bytes<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, LspError> {
    let mut content_length: Option<usize> = None;
    let mut line = Vec::new();
    let mut saw_any_header = false;

    loop {
        line.clear();
        let bytes_read = reader.read_until(b'\n', &mut line).await?;
        if bytes_read == 0 {
            if saw_any_header {
                return Err(LspError::Protocol(
                    "unexpected EOF inside message header".into(),
                ));
            }
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            }
            // Stray blank line between messages: tolerate and keep scanning.
            continue;
        }
        saw_any_header = true;
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                let len = value.trim().parse::<usize>().map_err(|_| {
                    LspError::Protocol(format!("invalid Content-Length header: {trimmed}"))
                })?;
                content_length = Some(len);
            }
        }
    }

    let len = content_length.unwrap_or(0);
    if len > MAX_FRAME_BYTES {
        return Err(LspError::Protocol(format!(
            "message of {len} bytes exceeds the {MAX_FRAME_BYTES} byte limit"
        )));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Reads a single `Content-Length`-framed `JsonRpcMessage` from an async buffered reader.
///
/// A body that is not valid JSON-RPC yields `Err(LspError::Json | Protocol)` but leaves
/// the stream positioned at the next frame, so callers may log and continue.
pub async fn read_lsp_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<JsonRpcMessage>, LspError> {
    match read_lsp_frame_bytes(reader).await? {
        Some(body) => Ok(Some(JsonRpcMessage::from_slice(&body)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[test]
    fn classifies_messages_structurally() {
        let req = JsonRpcMessage::from_slice(br#"{"jsonrpc":"2.0","id":"abc","method":"m"}"#)
            .unwrap();
        assert!(matches!(req, JsonRpcMessage::Request(r) if r.id == RequestId::from("abc")));

        let notif = JsonRpcMessage::from_slice(br#"{"jsonrpc":"2.0","method":"n"}"#).unwrap();
        assert!(matches!(notif, JsonRpcMessage::Notification(_)));

        let resp = JsonRpcMessage::from_slice(br#"{"jsonrpc":"2.0","id":4,"result":null}"#)
            .unwrap();
        assert!(
            matches!(resp, JsonRpcMessage::Response(r) if r.id == Some(RequestId::Number(4)))
        );

        let err_resp = JsonRpcMessage::from_slice(
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"bad"}}"#,
        )
        .unwrap();
        assert!(matches!(err_resp, JsonRpcMessage::Response(r) if r.id.is_none()));

        assert!(JsonRpcMessage::from_slice(br#"{"jsonrpc":"2.0"}"#).is_err());
    }

    #[test]
    fn success_response_serializes_null_result() {
        let resp = JsonRpcMessage::Response(JsonRpcResponse::success(1.into(), Value::Null));
        let text = serde_json::to_string(&resp).unwrap();
        assert!(text.contains("\"result\":null"), "{text}");
    }

    #[tokio::test]
    async fn frames_survive_byte_by_byte_delivery_and_extra_headers() {
        let (mut tx, rx) = tokio::io::duplex(8);
        let body1 = r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"héllo ✓"}}"#;
        let body2 = r#"{"jsonrpc":"2.0","id":7,"result":[1,2,3]}"#;
        let mut wire = Vec::new();
        wire.extend_from_slice(
            format!(
                "content-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
                body1.len()
            )
            .as_bytes(),
        );
        wire.extend_from_slice(body1.as_bytes());
        wire.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body2.len()).as_bytes());
        wire.extend_from_slice(body2.as_bytes());

        tokio::spawn(async move {
            for byte in wire {
                tx.write_all(&[byte]).await.unwrap();
            }
        });

        let mut reader = BufReader::new(rx);
        let first = read_lsp_frame(&mut reader).await.unwrap().unwrap();
        match first {
            JsonRpcMessage::Notification(n) => {
                assert_eq!(n.method, "window/logMessage");
                assert_eq!(n.params.unwrap()["message"], "héllo ✓");
            }
            other => panic!("unexpected {other:?}"),
        }
        let second = read_lsp_frame(&mut reader).await.unwrap().unwrap();
        assert!(matches!(second, JsonRpcMessage::Response(_)));
        assert!(read_lsp_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn malformed_body_does_not_desynchronize_stream() {
        let good = r#"{"jsonrpc":"2.0","method":"ok"}"#;
        let bad = "{not json}";
        let wire = format!(
            "Content-Length: {}\r\n\r\n{bad}Content-Length: {}\r\n\r\n{good}",
            bad.len(),
            good.len()
        );
        let mut reader = BufReader::new(wire.as_bytes());
        assert!(read_lsp_frame(&mut reader).await.is_err());
        let next = read_lsp_frame(&mut reader).await.unwrap().unwrap();
        assert!(matches!(next, JsonRpcMessage::Notification(n) if n.method == "ok"));
    }

    #[tokio::test]
    async fn eof_inside_header_is_an_error() {
        let mut reader = BufReader::new(&b"Content-Length: 10\r\n"[..]);
        assert!(read_lsp_frame(&mut reader).await.is_err());
    }
}
