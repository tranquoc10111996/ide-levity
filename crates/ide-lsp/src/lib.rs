//! `ide-lsp`: Language Server Protocol support for IDE Levity.
//!
//! Two layers:
//!
//! * [`LspClient`] - a hardened asynchronous JSON-RPC 2.0 client over
//!   [`StdioTransport`] (desktop), [`WebSocketTransport`] (remote/mobile) or
//!   [`MockTransport`] (tests). It correlates responses, applies timeouts, sends
//!   `$/cancelRequest` for abandoned requests, answers server-to-client requests,
//!   routes notifications and shuts servers down cleanly.
//! * [`LanguageServerManager`] - what the IDE uses: detects installed servers per
//!   language, starts one per project root on demand, synchronizes documents and
//!   exposes cancellable requests (completion, hover, definition, references,
//!   formatting) plus an [`LspEvent`] channel (diagnostics, status, messages). It is
//!   safe to drive from a GUI thread outside any Tokio runtime.

mod client;
mod convert;
mod jsonrpc;
mod manager;
mod registry;
mod transport;

pub use client::{
    default_client_capabilities, lookup_section, ClientEvent, ClientOptions, LspClient,
    PendingRequest, DEFAULT_REQUEST_TIMEOUT, INITIALIZE_TIMEOUT,
};
pub use convert::{
    completion_entries, completion_entry, definition_to_locations, hover_to_text, lsp_location,
    path_to_uri, snippet_to_plain_text, text_end_position, uri_to_path, CompletionEntry,
    CompletionKind, DiagnosticLevel, Location, LspDiagnostic, LspPosition, LspRange,
    LspTextEdit,
};
pub use jsonrpc::{
    encode_lsp_frame, error_codes, read_lsp_frame, read_lsp_frame_bytes, JsonRpcErrorPayload,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
    MAX_FRAME_BYTES,
};
pub use manager::{
    LanguageServerManager, LspEvent, LspEventReceiver, LspRequest, ManagerOptions, MessageLevel,
    MissingServer, OpenOutcome, ServerInfo, ServerKey, ServerState,
};
pub use registry::{
    find_executable, find_root, language_id_for_path, normalize_language_id, LanguageOverride,
    LspConfig, ServerAvailability, ServerRegistry, ServerSpec,
};
pub use transport::{
    encode_ws_frame, encode_ws_text_frame, read_ws_frame, read_ws_raw_frame, websocket_accept_key,
    MockTransport, ServerProcess, SpawnOptions, StderrTail, StdioTransport, TransportChannels,
    WebSocketTransport, WsFrame, STDERR_TAIL_LINES,
};

/// Raw protocol types, re-exported for callers that need them.
pub use lsp_types;

use std::time::Duration;
use thiserror::Error;

/// Errors produced by this crate.
#[derive(Debug, Error)]
pub enum LspError {
    /// Underlying I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// (De)serialization failure.
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    /// The connection to the server is closed.
    #[error("LSP transport closed")]
    TransportClosed,
    /// The server answered with a JSON-RPC error.
    #[error("JSON-RPC error ({code}): {message}")]
    RpcError {
        /// Error code (see [`error_codes`]).
        code: i64,
        /// Error message.
        message: String,
    },
    /// A string could not be parsed as a URI / converted from a path.
    #[error("Invalid URI: {0}")]
    InvalidUri(String),
    /// WebSocket upgrade failed.
    #[error("WebSocket handshake failed: {0}")]
    WebSocketHandshake(String),
    /// The peer violated the base protocol (framing, message shape).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server process could not be started.
    #[error("failed to start `{program}`: {source}")]
    Spawn {
        /// Program that failed to start.
        program: String,
        /// OS error.
        #[source]
        source: std::io::Error,
    },
    /// A request did not complete in time (it was cancelled on the server).
    #[error("request `{method}` timed out after {timeout:?}")]
    Timeout {
        /// Request method.
        method: String,
        /// Timeout that elapsed.
        timeout: Duration,
    },
    /// The request was cancelled by the caller.
    #[error("request cancelled")]
    Cancelled,
    /// Invalid `lsp.json` configuration.
    #[error("invalid LSP configuration: {0}")]
    Config(String),
    /// The document was never opened with the manager.
    #[error("document is not open: {0}")]
    DocumentNotOpen(String),
    /// The server for the document is not running (failed, exited or missing).
    #[error("language server `{0}` is not running")]
    ServerNotRunning(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        CompletionItem, CompletionItemKind, CompletionResponse, Diagnostic, DiagnosticSeverity,
        GotoDefinitionResponse, Hover, HoverContents, Location as LspLocation, MarkupContent,
        MarkupKind, Position, PublishDiagnosticsParams, Range, Uri,
    };
    use serde_json::{json, Value};
    use std::str::FromStr;
    use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

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
                    let loc = LspLocation {
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

        // Wait for the background router task to process the notification.
        let mut diags = Vec::new();
        for _ in 0..200 {
            diags = client.diagnostics_for_uri(uri);
            if !diags.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
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
                let resp = JsonRpcMessage::Response(JsonRpcResponse::success(req.id, Value::Null));
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
                    let resp = JsonRpcMessage::Response(JsonRpcResponse::success(req.id, json!([])));
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

    /// In-process fake server speaking the real `Content-Length` framing.
    struct FakeServer {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl FakeServer {
        fn connect(options: ClientOptions) -> (LspClient, tokio::sync::mpsc::UnboundedReceiver<ClientEvent>, Self) {
            let (client_side, server_side) = tokio::io::duplex(1 << 16);
            let (cr, cw) = tokio::io::split(client_side);
            let (sr, sw) = tokio::io::split(server_side);
            let transport = StdioTransport::from_streams(cr, cw);
            let (client, events) = LspClient::with_options(transport.into_channels(), options);
            (
                client,
                events,
                Self {
                    reader: BufReader::new(sr),
                    writer: sw,
                },
            )
        }

        async fn recv(&mut self) -> JsonRpcMessage {
            tokio::time::timeout(std::time::Duration::from_secs(5), read_lsp_frame(&mut self.reader))
                .await
                .expect("fake server timed out waiting for a message")
                .unwrap()
                .expect("stream closed")
        }

        async fn send(&mut self, msg: JsonRpcMessage) {
            let bytes = encode_lsp_frame(&msg).unwrap();
            self.writer.write_all(&bytes).await.unwrap();
            self.writer.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn server_to_client_requests_are_answered() {
        let options = ClientOptions {
            settings: json!({ "python": { "analysis": { "typeCheckingMode": "strict" } } }),
            workspace_folders: vec![lsp_types::WorkspaceFolder {
                uri: Uri::from_str("file:///proj").unwrap(),
                name: "proj".into(),
            }],
            ..Default::default()
        };
        let (client, mut events, mut server) = FakeServer::connect(options);

        let request = |id: &str, method: &str, params: Value| {
            JsonRpcMessage::Request(JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: RequestId::from(id),
                method: method.into(),
                params: Some(params),
            })
        };

        server
            .send(request(
                "c1",
                "workspace/configuration",
                json!({ "items": [{ "section": "python.analysis" }, { "section": "missing" }] }),
            ))
            .await;
        server
            .send(request("c2", "window/workDoneProgress/create", json!({ "token": "t" })))
            .await;
        server
            .send(request(
                "c3",
                "client/registerCapability",
                json!({ "registrations": [{ "id": "r1", "method": "workspace/didChangeWatchedFiles" }] }),
            ))
            .await;
        server
            .send(request("c4", "workspace/workspaceFolders", Value::Null))
            .await;
        server
            .send(request(
                "c5",
                "window/showMessageRequest",
                json!({ "type": 2, "message": "Pick one", "actions": [{ "title": "A" }] }),
            ))
            .await;
        server
            .send(request("c6", "custom/unknown", json!({})))
            .await;

        let mut answers = HashMap::new();
        for _ in 0..6 {
            match server.recv().await {
                JsonRpcMessage::Response(resp) => {
                    answers.insert(resp.id.clone().unwrap(), resp);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let get = |id: &str| answers.get(&RequestId::from(id)).unwrap().clone();
        assert_eq!(
            get("c1").result.unwrap(),
            json!([{ "typeCheckingMode": "strict" }, null])
        );
        let c2 = get("c2");
        assert!(c2.error.is_none());
        assert_eq!(c2.result, Some(Value::Null));
        assert_eq!(client.registrations().len(), 1);
        assert_eq!(get("c4").result.unwrap()[0]["name"], "proj");
        assert_eq!(get("c5").result, Some(Value::Null));
        assert_eq!(
            get("c6").error.unwrap().code,
            error_codes::METHOD_NOT_FOUND
        );

        server
            .send(request(
                "c7",
                "client/unregisterCapability",
                json!({ "unregisterations": [{ "id": "r1", "method": "workspace/didChangeWatchedFiles" }] }),
            ))
            .await;
        let _ = server.recv().await;
        assert!(client.registrations().is_empty());

        // Informational events were emitted for each request.
        let mut seen = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let ClientEvent::ServerRequest { method, .. } = ev {
                seen.push(method);
            }
        }
        assert!(seen.contains(&"window/showMessageRequest".to_string()));
    }

    use std::collections::HashMap;

    #[tokio::test]
    async fn timeout_and_drop_send_cancel_request() {
        let (client, _events, mut server) = FakeServer::connect(ClientOptions::default());

        let err = client
            .send_request_with_timeout::<_, Value>(
                "textDocument/hover",
                json!({}),
                std::time::Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::Timeout { .. }), "{err}");
        let first = server.recv().await;
        let JsonRpcMessage::Request(req) = first else {
            panic!("expected request")
        };
        match server.recv().await {
            JsonRpcMessage::Notification(n) => {
                assert_eq!(n.method, "$/cancelRequest");
                assert_eq!(n.params.unwrap()["id"], json!(req.id));
            }
            other => panic!("expected cancel, got {other:?}"),
        }

        // A late response for the cancelled id is ignored and does not disturb the next request.
        server
            .send(JsonRpcMessage::Response(JsonRpcResponse::success(
                req.id.clone(),
                json!("late"),
            )))
            .await;

        let pending = client.start_request("textDocument/definition", json!({})).unwrap();
        let JsonRpcMessage::Request(req2) = server.recv().await else {
            panic!("expected request")
        };
        assert_eq!(pending.id(), &req2.id);
        drop(pending);
        match server.recv().await {
            JsonRpcMessage::Notification(n) => assert_eq!(n.method, "$/cancelRequest"),
            other => panic!("expected cancel, got {other:?}"),
        }

        // Out-of-order responses are correlated by id.
        let a = client.start_request("a", json!({})).unwrap();
        let b = client.start_request("b", json!({})).unwrap();
        let JsonRpcMessage::Request(ra) = server.recv().await else { panic!() };
        let JsonRpcMessage::Request(rb) = server.recv().await else { panic!() };
        server
            .send(JsonRpcMessage::Response(JsonRpcResponse::success(rb.id, json!("B"))))
            .await;
        server
            .send(JsonRpcMessage::Response(JsonRpcResponse::failure(
                ra.id,
                error_codes::CONTENT_MODIFIED,
                "content modified",
            )))
            .await;
        assert_eq!(b.wait().await.unwrap(), json!("B"));
        assert!(matches!(
            a.wait().await,
            Err(LspError::RpcError { code, .. }) if code == error_codes::CONTENT_MODIFIED
        ));
    }

    #[tokio::test]
    async fn closed_transport_fails_pending_requests_and_emits_closed() {
        let (client, mut events, server) = FakeServer::connect(ClientOptions::default());
        let pending = client.start_request("slow", json!({})).unwrap();
        drop(server);
        let err = pending.wait().await.unwrap_err();
        assert!(matches!(err, LspError::TransportClosed), "{err}");
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .unwrap()
            {
                Some(ClientEvent::Closed) => break,
                Some(_) => continue,
                None => panic!("event channel closed without Closed event"),
            }
        }
        assert!(client.is_closed());
        assert!(matches!(
            client.send_notification("x", json!({})),
            Err(LspError::TransportClosed)
        ));
    }

    #[tokio::test]
    async fn shutdown_kills_unresponsive_process() {
        // `sleep` never answers `shutdown`; the client must kill it after the timeout.
        let transport = StdioTransport::spawn("sleep", &["30"]).unwrap();
        let client = LspClient::new(transport.into_channels());
        assert!(client.process_id().is_some());
        let started = std::time::Instant::now();
        let status = client
            .shutdown(std::time::Duration::from_millis(300))
            .await
            .expect("exit status");
        assert!(!status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn client_capabilities_advertise_utf16_and_no_snippets() {
        let caps = default_client_capabilities();
        let value = serde_json::to_value(&caps).unwrap();
        assert_eq!(value["general"]["positionEncodings"], json!(["utf-16"]));
        assert_eq!(
            value["textDocument"]["completion"]["completionItem"]["snippetSupport"],
            json!(false)
        );
        assert_eq!(value["textDocument"]["definition"]["linkSupport"], json!(true));
        assert_eq!(value["window"]["workDoneProgress"], json!(true));
        assert_eq!(
            lookup_section(&json!({"a": {"b": {"c": 1}}}), "a.b"),
            json!({"c": 1})
        );
    }
}
