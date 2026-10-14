//! `ide-lsp`: Asynchronous JSON-RPC 2.0 Language Server Protocol client supporting
//! `StdioTransport` (Desktop), `WebSocketTransport` (Mobile Remote LSP), and `MockTransport`.
//! Implements `textDocument/didOpen`, `didChange`, `completion`, `hover`, `definition`,
//! and `publishDiagnostics`.

use lsp_types::{
    CompletionItem, CompletionParams, CompletionResponse, Diagnostic, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    Position, PublishDiagnosticsParams, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

#[derive(Debug, Error)]
pub enum LspError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("LSP transport closed")]
    TransportClosed,
    #[error("JSON-RPC error ({code}): {message}")]
    RpcError { code: i64, message: String },
    #[error("Invalid URI: {0}")]
    InvalidUri(String),
    #[error("WebSocket handshake failed: {0}")]
    WebSocketHandshake(String),
}

/// JSON-RPC 2.0 Request object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: i64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 Error payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorPayload {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 Response object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorPayload>,
}

/// JSON-RPC 2.0 Notification object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Unified JSON-RPC 2.0 message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

/// Encodes a `JsonRpcMessage` using the standard LSP `Content-Length` HTTP-like header framing.
pub fn encode_lsp_frame(msg: &JsonRpcMessage) -> Result<Vec<u8>, LspError> {
    let body = serde_json::to_vec(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Reads a single `Content-Length`-framed `JsonRpcMessage` from an async buffered reader.
pub async fn read_lsp_frame<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<JsonRpcMessage>, LspError> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if content_length.is_some() {
                break;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = Some(len);
            }
        }
    }

    let len = content_length.ok_or(LspError::TransportClosed)?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let msg = serde_json::from_slice::<JsonRpcMessage>(&body)?;
    Ok(Some(msg))
}

/// Trait implemented by all asynchronous LSP transports (`StdioTransport`, `WebSocketTransport`, `MockTransport`).
pub struct TransportChannels {
    pub outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    pub incoming_rx: Mutex<mpsc::UnboundedReceiver<JsonRpcMessage>>,
}

/// Desktop transport that communicates over standard input/output (process stdio or duplex streams)
/// using `Content-Length` framed JSON-RPC 2.0 packets.
pub struct StdioTransport {
    channels: TransportChannels,
}

impl StdioTransport {
    /// Spawns a local Language Server process (e.g. `rust-analyzer`, `pyright-langserver`) and attaches to its stdio.
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self, LspError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().ok_or(LspError::TransportClosed)?;
        let stdout = child.stdout.take().ok_or(LspError::TransportClosed)?;

        Ok(Self::from_streams(stdout, stdin))
    }

    /// Creates a `StdioTransport` from any pair of `AsyncRead` and `AsyncWrite` streams.
    pub fn from_streams<R, W>(reader: R, mut writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Ok(frame) = encode_lsp_frame(&msg) {
                    if writer.write_all(&frame).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            }
        });

        tokio::spawn(async move {
            let mut buf_reader = BufReader::new(reader);
            while let Ok(Some(msg)) = read_lsp_frame(&mut buf_reader).await {
                if in_tx.send(msg).is_err() {
                    break;
                }
            }
        });

        Self {
            channels: TransportChannels {
                outgoing_tx: out_tx,
                incoming_rx: Mutex::new(in_rx),
            },
        }
    }

    pub fn into_channels(self) -> TransportChannels {
        self.channels
    }
}

/// Mobile / Remote transport that connects to a remote LSP bridge over WebSocket (RFC 6455).
pub struct WebSocketTransport {
    channels: TransportChannels,
}

impl WebSocketTransport {
    /// Connects to a `ws://host:port/path` endpoint and performs the HTTP Upgrade handshake.
    pub async fn connect(ws_url: &str) -> Result<Self, LspError> {
        let stripped = ws_url
            .strip_prefix("ws://")
            .ok_or_else(|| LspError::WebSocketHandshake("URL must start with ws://".into()))?;
        let (host_port, path) = match stripped.find('/') {
            Some(idx) => (&stripped[..idx], &stripped[idx..]),
            None => (stripped, "/"),
        };

        let mut stream = TcpStream::connect(host_port).await?;
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host_port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;

        let (read_half, write_half) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(read_half);
        let mut status_line = String::new();
        buf_reader.read_line(&mut status_line).await?;
        if !status_line.contains("101") {
            return Err(LspError::WebSocketHandshake(format!(
                "Expected HTTP 101 Switching Protocols, got: {}",
                status_line.trim()
            )));
        }
        loop {
            let mut header_line = String::new();
            let n = buf_reader.read_line(&mut header_line).await?;
            if n == 0 || header_line.trim().is_empty() {
                break;
            }
        }

        Ok(Self::from_framed_streams(buf_reader, write_half))
    }

    /// Creates a `WebSocketTransport` over an already-upgraded byte stream using RFC 6455 frames.
    pub fn from_framed_streams<R, W>(mut reader: R, mut writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Ok(payload) = serde_json::to_vec(&msg) {
                    let frame = encode_ws_text_frame(&payload);
                    if writer.write_all(&frame).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
            }
        });

        tokio::spawn(async move {
            while let Ok(Some(payload)) = read_ws_frame(&mut reader).await {
                if let Ok(msg) = serde_json::from_slice::<JsonRpcMessage>(&payload) {
                    if in_tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        });

        Self {
            channels: TransportChannels {
                outgoing_tx: out_tx,
                incoming_rx: Mutex::new(in_rx),
            },
        }
    }

    pub fn into_channels(self) -> TransportChannels {
        self.channels
    }
}

/// Encodes a WebSocket text frame (FIN=1, opcode=1).
pub fn encode_ws_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x81); // FIN + Text opcode
    let len = payload.len();
    if len <= 125 {
        frame.push(len as u8);
    } else if len <= 65535 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// Reads a single WebSocket frame payload.
pub async fn read_ws_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, LspError> {
    let mut header = [0u8; 2];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(LspError::Io(e)),
    }
    let opcode = header[0] & 0x0F;
    if opcode == 0x08 {
        // Close frame
        return Ok(None);
    }
    let masked = (header[1] & 0x80) != 0;
    let mut len = (header[1] & 0x7F) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        reader.read_exact(&mut ext).await?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        reader.read_exact(&mut ext).await?;
        len = u64::from_be_bytes(ext) as usize;
    }
    let mut mask_key = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask_key).await?;
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask_key[i % 4];
        }
    }
    Ok(Some(payload))
}

/// Mock transport for deterministic testing of all LSP requests and notifications.
pub struct MockTransport {
    channels: TransportChannels,
}

impl MockTransport {
    /// Creates a `MockTransport` paired with a server-side handler function and a notification sender.
    pub fn new<F>(
        mut handler: F,
    ) -> (
        Self,
        mpsc::UnboundedSender<JsonRpcNotification>,
        Arc<RwLock<Vec<JsonRpcNotification>>>,
    )
    where
        F: FnMut(&JsonRpcRequest) -> Result<Value, JsonRpcErrorPayload> + Send + 'static,
    {
        let (client_out_tx, mut server_in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (server_out_tx, client_in_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<JsonRpcNotification>();

        let recorded_notifications = Arc::new(RwLock::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded_notifications);
        let server_out_tx_clone = server_out_tx.clone();

        // Forward server-originated notifications (such as publishDiagnostics)
        tokio::spawn(async move {
            while let Some(notif) = notify_rx.recv().await {
                if server_out_tx_clone
                    .send(JsonRpcMessage::Notification(notif))
                    .is_err()
                {
                    break;
                }
            }
        });

        // Process incoming client requests & notifications
        tokio::spawn(async move {
            while let Some(msg) = server_in_rx.recv().await {
                match msg {
                    JsonRpcMessage::Request(req) => {
                        let response = match handler(&req) {
                            Ok(val) => JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id: req.id,
                                result: Some(val),
                                error: None,
                            },
                            Err(err) => JsonRpcResponse {
                                jsonrpc: "2.0".to_string(),
                                id: req.id,
                                result: None,
                                error: Some(err),
                            },
                        };
                        let _ = server_out_tx.send(JsonRpcMessage::Response(response));
                    }
                    JsonRpcMessage::Notification(notif) => {
                        recorded_clone.write().push(notif);
                    }
                    JsonRpcMessage::Response(_) => {}
                }
            }
        });

        (
            Self {
                channels: TransportChannels {
                    outgoing_tx: client_out_tx,
                    incoming_rx: Mutex::new(client_in_rx),
                },
            },
            notify_tx,
            recorded_notifications,
        )
    }

    pub fn into_channels(self) -> TransportChannels {
        self.channels
    }
}

/// Asynchronous JSON-RPC 2.0 LSP client.
pub struct LspClient {
    next_id: AtomicI64,
    outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    pending: Arc<RwLock<HashMap<i64, oneshot::Sender<JsonRpcResponse>>>>,
    diagnostics: Arc<RwLock<HashMap<String, Vec<Diagnostic>>>>,
}

impl LspClient {
    /// Creates a new `LspClient` from `TransportChannels` and starts its background message router.
    pub fn new(channels: TransportChannels) -> Self {
        let pending: Arc<RwLock<HashMap<i64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let diagnostics: Arc<RwLock<HashMap<String, Vec<Diagnostic>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let pending_clone = Arc::clone(&pending);
        let diagnostics_clone = Arc::clone(&diagnostics);

        tokio::spawn(async move {
            let mut rx = channels.incoming_rx.lock().await;
            while let Some(msg) = rx.recv().await {
                match msg {
                    JsonRpcMessage::Response(resp) => {
                        if let Some(tx) = pending_clone.write().remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    JsonRpcMessage::Notification(notif) => {
                        if notif.method == "textDocument/publishDiagnostics" {
                            if let Some(params_val) = notif.params {
                                if let Ok(params) =
                                    serde_json::from_value::<PublishDiagnosticsParams>(params_val)
                                {
                                    diagnostics_clone
                                        .write()
                                        .insert(params.uri.to_string(), params.diagnostics);
                                }
                            }
                        }
                    }
                    JsonRpcMessage::Request(_) => {}
                }
            }
        });

        Self {
            next_id: AtomicI64::new(1),
            outgoing_tx: channels.outgoing_tx,
            pending,
            diagnostics,
        }
    }

    pub fn from_stdio(transport: StdioTransport) -> Self {
        Self::new(transport.into_channels())
    }

    pub fn from_websocket(transport: WebSocketTransport) -> Self {
        Self::new(transport.into_channels())
    }

    pub fn from_mock(transport: MockTransport) -> Self {
        Self::new(transport.into_channels())
    }

    /// Sends a JSON-RPC 2.0 request and awaits the typed response.
    pub async fn send_request<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params_val = serde_json::to_value(params)?;
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params: Some(params_val),
        };

        let (tx, rx) = oneshot::channel();
        self.pending.write().insert(id, tx);

        self.outgoing_tx
            .send(JsonRpcMessage::Request(req))
            .map_err(|_| LspError::TransportClosed)?;

        let resp = rx.await.map_err(|_| LspError::TransportClosed)?;
        if let Some(err) = resp.error {
            return Err(LspError::RpcError {
                code: err.code,
                message: err.message,
            });
        }
        let result_val = resp.result.unwrap_or(Value::Null);
        Ok(serde_json::from_value(result_val)?)
    }

    /// Sends a JSON-RPC 2.0 notification (`textDocument/didOpen`, `textDocument/didChange`, etc.).
    pub fn send_notification<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspError> {
        let params_val = serde_json::to_value(params)?;
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params: Some(params_val),
        };
        self.outgoing_tx
            .send(JsonRpcMessage::Notification(notif))
            .map_err(|_| LspError::TransportClosed)
    }

    /// Helper to parse an LSP `Uri` from string.
    pub fn parse_uri(uri_str: &str) -> Result<Uri, LspError> {
        Uri::from_str(uri_str).map_err(|e| LspError::InvalidUri(format!("{uri_str}: {e}")))
    }

    /// Sends `textDocument/didOpen`.
    pub fn did_open(
        &self,
        uri_str: &str,
        language_id: &str,
        version: i32,
        text: String,
    ) -> Result<(), LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: language_id.to_string(),
                version,
                text,
            },
        };
        self.send_notification("textDocument/didOpen", params)
    }

    /// Sends `textDocument/didChange` with full document synchronization.
    pub fn did_change(
        &self,
        uri_str: &str,
        version: i32,
        new_text: String,
    ) -> Result<(), LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: new_text,
            }],
        };
        self.send_notification("textDocument/didChange", params)
    }

    /// Requests `textDocument/completion` at `(line, character)` and normalizes the response.
    pub async fn completion(
        &self,
        uri_str: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<CompletionItem>, LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        let resp: Option<CompletionResponse> =
            self.send_request("textDocument/completion", params).await?;
        let items = match resp {
            Some(CompletionResponse::Array(arr)) => arr,
            Some(CompletionResponse::List(list)) => list.items,
            None => Vec::new(),
        };
        Ok(items)
    }

    /// Requests `textDocument/hover` at `(line, character)`.
    pub async fn hover(
        &self,
        uri_str: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<Hover>, LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        self.send_request("textDocument/hover", params).await
    }

    /// Requests `textDocument/definition` at `(line, character)`.
    pub async fn definition(
        &self,
        uri_str: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<GotoDefinitionResponse>, LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
        };
        self.send_request("textDocument/definition", params).await
    }

    /// Returns cached diagnostics published by the server for `uri_str`.
    pub fn diagnostics_for_uri(&self, uri_str: &str) -> Vec<Diagnostic> {
        self.diagnostics
            .read()
            .get(uri_str)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        CompletionItemKind, DiagnosticSeverity, HoverContents, Location, MarkupContent, MarkupKind,
        Range,
    };

    #[tokio::test]
    async fn test_mock_transport_full_lsp_flow() {
        let (mock_transport, server_notify_tx, recorded_notifs) = MockTransport::new(|req| {
            match req.method.as_str() {
                "textDocument/completion" => {
                    let items = vec![
                        CompletionItem {
                            label: "println!".to_string(),
                            kind: Some(CompletionItemKind::FUNCTION),
                            detail: Some("macro println!".to_string()),
                            ..Default::default()
                        },
                        CompletionItem {
                            label: "push_str".to_string(),
                            kind: Some(CompletionItemKind::METHOD),
                            ..Default::default()
                        },
                    ];
                    Ok(serde_json::to_value(CompletionResponse::Array(items)).unwrap())
                }
                "textDocument/hover" => {
                    let hover = Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: "**fn main()** - Điểm khởi đầu chương trình".to_string(),
                        }),
                        range: None,
                    };
                    Ok(serde_json::to_value(Some(hover)).unwrap())
                }
                "textDocument/definition" => {
                    let loc = Location {
                        uri: Uri::from_str("file:///workspace/src/lib.rs").unwrap(),
                        range: Range {
                            start: Position {
                                line: 10,
                                character: 0,
                            },
                            end: Position {
                                line: 10,
                                character: 12,
                            },
                        },
                    };
                    Ok(serde_json::to_value(Some(GotoDefinitionResponse::Scalar(loc))).unwrap())
                }
                _ => Err(JsonRpcErrorPayload {
                    code: -32601,
                    message: "Method not found".to_string(),
                    data: None,
                }),
            }
        });

        let client = LspClient::from_mock(mock_transport);
        let uri = "file:///workspace/src/main.rs";

        // 1. didOpen & didChange
        client
            .did_open(uri, "rust", 1, "fn main() {}\n".to_string())
            .unwrap();
        client
            .did_change(uri, 2, "fn main() {\n    pr\n}\n".to_string())
            .unwrap();

        // 2. completion
        let completions = client.completion(uri, 1, 6).await.unwrap();
        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].label, "println!");

        // 3. hover
        let hover = client.hover(uri, 0, 3).await.unwrap().expect("hover");
        match hover.contents {
            HoverContents::Markup(m) => {
                assert!(m.value.contains("Điểm khởi đầu"));
            }
            _ => panic!("Expected Markup hover"),
        }

        // 4. definition
        let def = client
            .definition(uri, 0, 3)
            .await
            .unwrap()
            .expect("definition");
        match def {
            GotoDefinitionResponse::Scalar(loc) => {
                assert_eq!(loc.range.start.line, 10);
            }
            _ => panic!("Expected Scalar location"),
        }

        // 5. publishDiagnostics from server
        let diag_params = PublishDiagnosticsParams {
            uri: Uri::from_str(uri).unwrap(),
            diagnostics: vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 4,
                    },
                    end: Position {
                        line: 1,
                        character: 6,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: "cannot find value `pr` in this scope".to_string(),
                ..Default::default()
            }],
            version: Some(2),
        };
        server_notify_tx
            .send(JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "textDocument/publishDiagnostics".to_string(),
                params: Some(serde_json::to_value(diag_params).unwrap()),
            })
            .unwrap();

        // Yield briefly for the background router task to process the notification
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let diags = client.diagnostics_for_uri(uri);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "cannot find value `pr` in this scope");
        assert_eq!(recorded_notifs.read().len(), 2);
    }

    #[tokio::test]
    async fn test_stdio_transport_framing_roundtrip() {
        let (client_read, mut server_write) = tokio::io::duplex(4096);
        let (server_read, client_write) = tokio::io::duplex(4096);

        let transport = StdioTransport::from_streams(client_read, client_write);
        let client = LspClient::from_stdio(transport);

        tokio::spawn(async move {
            let mut reader = BufReader::new(server_read);
            if let Ok(Some(JsonRpcMessage::Request(req))) = read_lsp_frame(&mut reader).await {
                let resp = JsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: req.id,
                    result: Some(serde_json::json!(null)),
                    error: None,
                });
                let bytes = encode_lsp_frame(&resp).unwrap();
                let _ = server_write.write_all(&bytes).await;
                let _ = server_write.flush().await;
            }
        });

        let hover = client
            .hover("file:///test.rs", 0, 0)
            .await
            .expect("stdio hover should succeed");
        assert!(hover.is_none());
    }

    #[tokio::test]
    async fn test_websocket_transport_framing_roundtrip() {
        let (client_read, mut server_write) = tokio::io::duplex(4096);
        let (mut server_read, client_write) = tokio::io::duplex(4096);

        let transport = WebSocketTransport::from_framed_streams(client_read, client_write);
        let client = LspClient::from_websocket(transport);

        tokio::spawn(async move {
            if let Ok(Some(payload)) = read_ws_frame(&mut server_read).await {
                if let Ok(JsonRpcMessage::Request(req)) =
                    serde_json::from_slice::<JsonRpcMessage>(&payload)
                {
                    let resp = JsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: Some(serde_json::json!([])),
                        error: None,
                    });
                    let frame = encode_ws_text_frame(&serde_json::to_vec(&resp).unwrap());
                    let _ = server_write.write_all(&frame).await;
                    let _ = server_write.flush().await;
                }
            }
        });

        let items = client
            .completion("file:///mobile.kt", 2, 4)
            .await
            .expect("websocket completion should succeed");
        assert!(items.is_empty());
    }
}
