//! Low-level asynchronous LSP client: request/response correlation, timeouts,
//! `$/cancelRequest` on drop, automatic answers to server-to-client requests,
//! notification routing and graceful shutdown.

use crate::jsonrpc::{
    error_codes, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
};
use crate::transport::{
    MockTransport, ServerProcess, StderrTail, StdioTransport, TransportChannels,
    WebSocketTransport,
};
use crate::LspError;
use lsp_types::{
    ClientCapabilities, CompletionItem, CompletionParams, CompletionResponse, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, FormattingOptions, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, InitializeParams, InitializeResult, Location,
    Position, PublishDiagnosticsParams, Range, ReferenceContext, ReferenceParams, Registration,
    ServerCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextEdit, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceFolder,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::ExitStatus;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// Default timeout applied by [`LspClient::send_request`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for the `initialize` handshake (some servers, e.g. kotlin-language-server,
/// take a long time to boot).
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);

/// Options controlling an [`LspClient`].
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Timeout used by [`LspClient::send_request`] and the convenience request methods.
    pub request_timeout: Duration,
    /// Settings object used to answer `workspace/configuration` (looked up by dotted section).
    pub settings: Value,
    /// Folders returned for `workspace/workspaceFolders`.
    pub workspace_folders: Vec<WorkspaceFolder>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            settings: Value::Null,
            workspace_folders: Vec::new(),
        }
    }
}

/// Events emitted by an [`LspClient`] to its owner.
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// A server notification (`textDocument/publishDiagnostics`, `window/showMessage`,
    /// `window/logMessage`, `$/progress`, ...).
    Notification(JsonRpcNotification),
    /// A server-to-client request. It has already been answered automatically; the
    /// event is informational (e.g. to surface `window/showMessageRequest` text).
    ServerRequest {
        /// Request method.
        method: String,
        /// Request parameters.
        params: Option<Value>,
    },
    /// The connection closed (server exited or the stream failed).
    Closed,
}

struct Shared {
    outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<JsonRpcResponse>>>,
    diagnostics: RwLock<HashMap<String, Vec<Diagnostic>>>,
    settings: RwLock<Value>,
    workspace_folders: RwLock<Vec<WorkspaceFolder>>,
    registrations: RwLock<HashMap<String, Registration>>,
    closed: AtomicBool,
    events_tx: mpsc::UnboundedSender<ClientEvent>,
}

impl Shared {
    fn send(&self, msg: JsonRpcMessage) -> Result<(), LspError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(LspError::TransportClosed);
        }
        self.outgoing_tx
            .send(msg)
            .map_err(|_| LspError::TransportClosed)
    }

    fn answer_server_request(&self, req: &JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone();
        let params = req.params.clone().unwrap_or(Value::Null);
        match req.method.as_str() {
            "workspace/configuration" => {
                let settings = self.settings.read();
                let items = params
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let answers: Vec<Value> = items
                    .iter()
                    .map(|item| match item.get("section").and_then(Value::as_str) {
                        Some(section) => lookup_section(&settings, section),
                        None => settings.clone(),
                    })
                    .collect();
                JsonRpcResponse::success(id, Value::Array(answers))
            }
            "workspace/workspaceFolders" => {
                let folders = self.workspace_folders.read();
                if folders.is_empty() {
                    JsonRpcResponse::success(id, Value::Null)
                } else {
                    JsonRpcResponse::success(id, json!(*folders))
                }
            }
            "client/registerCapability" => {
                if let Some(list) = params.get("registrations").and_then(Value::as_array) {
                    let mut regs = self.registrations.write();
                    for reg in list {
                        if let Ok(reg) = serde_json::from_value::<Registration>(reg.clone()) {
                            regs.insert(reg.id.clone(), reg);
                        }
                    }
                }
                JsonRpcResponse::success(id, Value::Null)
            }
            "client/unregisterCapability" => {
                // The specification spells the field `unregisterations`.
                let list = params
                    .get("unregisterations")
                    .or_else(|| params.get("unregistrations"))
                    .and_then(Value::as_array);
                if let Some(list) = list {
                    let mut regs = self.registrations.write();
                    for unreg in list {
                        if let Some(reg_id) = unreg.get("id").and_then(Value::as_str) {
                            regs.remove(reg_id);
                        }
                    }
                }
                JsonRpcResponse::success(id, Value::Null)
            }
            // No interactive prompt is available at this layer: answer "no selection".
            "window/showMessageRequest"
            | "window/workDoneProgress/create"
            | "workspace/semanticTokens/refresh"
            | "workspace/inlayHint/refresh"
            | "workspace/inlineValue/refresh"
            | "workspace/codeLens/refresh"
            | "workspace/diagnostic/refresh"
            | "workspace/foldingRange/refresh" => JsonRpcResponse::success(id, Value::Null),
            "window/showDocument" => JsonRpcResponse::success(id, json!({ "success": false })),
            "workspace/applyEdit" => JsonRpcResponse::success(
                id,
                json!({
                    "applied": false,
                    "failureReason": "workspace/applyEdit is not supported by this client"
                }),
            ),
            other => JsonRpcResponse::failure(
                id,
                error_codes::METHOD_NOT_FOUND,
                format!("Unhandled server request: {other}"),
            ),
        }
    }
}

/// Looks up a dotted `section` (e.g. `rust-analyzer.cargo`) in a settings object.
/// Returns `null` when absent, which servers interpret as "use defaults".
pub fn lookup_section(settings: &Value, section: &str) -> Value {
    if section.is_empty() {
        return settings.clone();
    }
    if let Some(found) = settings.get(section) {
        return found.clone();
    }
    let mut current = settings;
    for part in section.split('.') {
        match current.get(part) {
            Some(next) => current = next,
            None => return Value::Null,
        }
    }
    current.clone()
}

/// An in-flight request. Await it with [`PendingRequest::wait`] or
/// [`PendingRequest::wait_timeout`]. Dropping it before the response arrives removes
/// it from the correlation table and sends `$/cancelRequest` to the server.
pub struct PendingRequest {
    id: RequestId,
    method: String,
    rx: Option<oneshot::Receiver<JsonRpcResponse>>,
    shared: Arc<Shared>,
}

impl PendingRequest {
    /// The JSON-RPC id of this request.
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// The method of this request.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// Waits for the response without a timeout.
    pub async fn wait(mut self) -> Result<Value, LspError> {
        let rx = self.rx.as_mut().ok_or(LspError::Cancelled)?;
        let outcome = rx.await;
        self.rx = None;
        let resp = outcome.map_err(|_| LspError::TransportClosed)?;
        response_to_result(resp)
    }

    /// Waits for the response; on timeout the request is cancelled on the server.
    pub async fn wait_timeout(self, timeout: Duration) -> Result<Value, LspError> {
        let method = self.method.clone();
        match tokio::time::timeout(timeout, self.wait()).await {
            Ok(result) => result,
            Err(_) => Err(LspError::Timeout { method, timeout }),
        }
    }

    /// Cancels the request (equivalent to dropping it).
    pub fn cancel(self) {}
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if self.rx.take().is_some() {
            let removed = self.shared.pending.lock().remove(&self.id).is_some();
            if removed {
                let _ = self.shared.send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                    "$/cancelRequest",
                    Some(json!({ "id": self.id })),
                )));
            }
        }
    }
}

fn response_to_result(resp: JsonRpcResponse) -> Result<Value, LspError> {
    if let Some(err) = resp.error {
        return Err(LspError::RpcError {
            code: err.code,
            message: err.message,
        });
    }
    Ok(resp.result.unwrap_or(Value::Null))
}

/// Returns the client capabilities advertised by this crate: UTF-16 positions,
/// push diagnostics, plain-text completion (no snippets), markdown/plaintext hover,
/// definition links, didSave, references, formatting and work-done progress.
pub fn default_client_capabilities() -> ClientCapabilities {
    let kinds: Vec<u32> = (1..=25).collect();
    let caps = json!({
        "general": { "positionEncodings": ["utf-16"] },
        "workspace": {
            "applyEdit": false,
            "workspaceFolders": true,
            "configuration": true,
            "didChangeConfiguration": { "dynamicRegistration": false },
            "didChangeWatchedFiles": { "dynamicRegistration": false },
            "workspaceEdit": { "documentChanges": false }
        },
        "window": {
            "workDoneProgress": true,
            "showMessage": { "messageActionItem": { "additionalPropertiesSupport": false } },
            "showDocument": { "support": false }
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "didSave": true,
                "willSave": false,
                "willSaveWaitUntil": false
            },
            "completion": {
                "dynamicRegistration": false,
                "contextSupport": true,
                "completionItem": {
                    "snippetSupport": false,
                    "commitCharactersSupport": false,
                    "documentationFormat": ["markdown", "plaintext"],
                    "deprecatedSupport": true,
                    "preselectSupport": true,
                    "insertReplaceSupport": false,
                    "labelDetailsSupport": true
                },
                "completionItemKind": { "valueSet": kinds }
            },
            "hover": { "dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"] },
            "definition": { "dynamicRegistration": false, "linkSupport": true },
            "references": { "dynamicRegistration": false },
            "formatting": { "dynamicRegistration": false },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "tagSupport": { "valueSet": [1, 2] },
                "codeDescriptionSupport": true,
                "dataSupport": false
            }
        },
        "experimental": { "serverStatusNotification": true }
    });
    serde_json::from_value(caps).unwrap_or_default()
}

/// Asynchronous JSON-RPC 2.0 LSP client.
pub struct LspClient {
    next_id: AtomicI64,
    shared: Arc<Shared>,
    request_timeout: Duration,
    process: tokio::sync::Mutex<Option<ServerProcess>>,
    stderr_tail: Option<StderrTail>,
    process_id: Option<u32>,
    capabilities: RwLock<Option<ServerCapabilities>>,
}

impl LspClient {
    /// Creates a new `LspClient` from `TransportChannels` and starts its background message router.
    /// Events are discarded; use [`LspClient::with_options`] to receive them.
    /// Must be called inside a Tokio runtime.
    pub fn new(channels: TransportChannels) -> Self {
        Self::with_options(channels, ClientOptions::default()).0
    }

    /// Creates a client with explicit options and returns the receiver of [`ClientEvent`]s.
    /// Must be called inside a Tokio runtime.
    pub fn with_options(
        channels: TransportChannels,
        options: ClientOptions,
    ) -> (Self, mpsc::UnboundedReceiver<ClientEvent>) {
        let TransportChannels {
            outgoing_tx,
            incoming_rx,
            process,
        } = channels;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            outgoing_tx,
            pending: Mutex::new(HashMap::new()),
            diagnostics: RwLock::new(HashMap::new()),
            settings: RwLock::new(options.settings),
            workspace_folders: RwLock::new(options.workspace_folders),
            registrations: RwLock::new(HashMap::new()),
            closed: AtomicBool::new(false),
            events_tx,
        });

        let router_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut rx = incoming_rx.into_inner();
            while let Some(msg) = rx.recv().await {
                match msg {
                    JsonRpcMessage::Response(resp) => {
                        let Some(id) = resp.id.clone() else { continue };
                        let waiter = router_shared.pending.lock().remove(&id);
                        if let Some(tx) = waiter {
                            let _ = tx.send(resp);
                        }
                    }
                    JsonRpcMessage::Notification(notif) => {
                        if notif.method == "textDocument/publishDiagnostics" {
                            if let Some(params_val) = notif.params.clone() {
                                if let Ok(params) =
                                    serde_json::from_value::<PublishDiagnosticsParams>(params_val)
                                {
                                    router_shared
                                        .diagnostics
                                        .write()
                                        .insert(params.uri.to_string(), params.diagnostics);
                                }
                            }
                        }
                        let _ = router_shared
                            .events_tx
                            .send(ClientEvent::Notification(notif));
                    }
                    JsonRpcMessage::Request(req) => {
                        let resp = router_shared.answer_server_request(&req);
                        let _ = router_shared.send(JsonRpcMessage::Response(resp));
                        let _ = router_shared.events_tx.send(ClientEvent::ServerRequest {
                            method: req.method,
                            params: req.params,
                        });
                    }
                }
            }
            router_shared.closed.store(true, Ordering::SeqCst);
            // Dropping the senders fails every outstanding request with TransportClosed.
            router_shared.pending.lock().clear();
            let _ = router_shared.events_tx.send(ClientEvent::Closed);
        });

        let stderr_tail = process.as_ref().map(ServerProcess::stderr_tail_handle);
        let process_id = process.as_ref().and_then(ServerProcess::pid);
        let client = Self {
            next_id: AtomicI64::new(1),
            shared,
            request_timeout: options.request_timeout,
            process: tokio::sync::Mutex::new(process),
            stderr_tail,
            process_id,
            capabilities: RwLock::new(None),
        };
        (client, events_rx)
    }

    /// Creates a client over a [`StdioTransport`].
    pub fn from_stdio(transport: StdioTransport) -> Self {
        Self::new(transport.into_channels())
    }

    /// Creates a client over a [`WebSocketTransport`].
    pub fn from_websocket(transport: WebSocketTransport) -> Self {
        Self::new(transport.into_channels())
    }

    /// Creates a client over a [`MockTransport`].
    pub fn from_mock(transport: MockTransport) -> Self {
        Self::new(transport.into_channels())
    }

    /// `true` once the connection to the server is gone.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// OS process id of the server, when this client owns a spawned process.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// Last lines the server wrote to stderr (empty for non-process transports).
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .as_ref()
            .map(|t| t.joined(20))
            .unwrap_or_default()
    }

    /// Server capabilities received from `initialize`, if the handshake completed.
    pub fn server_capabilities(&self) -> Option<ServerCapabilities> {
        self.capabilities.read().clone()
    }

    /// Currently active dynamic registrations (`client/registerCapability`).
    pub fn registrations(&self) -> Vec<Registration> {
        self.shared.registrations.read().values().cloned().collect()
    }

    /// Replaces the settings used to answer `workspace/configuration`.
    pub fn set_settings(&self, settings: Value) {
        *self.shared.settings.write() = settings;
    }

    /// Sends a request immediately and returns a handle to await (or cancel) it.
    /// Messages are written in call order, so a notification sent before this call is
    /// guaranteed to reach the server first.
    pub fn start_request<P: Serialize>(
        &self,
        method: &str,
        params: P,
    ) -> Result<PendingRequest, LspError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let params_val = serde_json::to_value(params)?;
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            method: method.to_string(),
            params: Some(params_val),
        };
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().insert(id.clone(), tx);
        if let Err(e) = self.shared.send(JsonRpcMessage::Request(req)) {
            self.shared.pending.lock().remove(&id);
            return Err(e);
        }
        Ok(PendingRequest {
            id,
            method: method.to_string(),
            rx: Some(rx),
            shared: Arc::clone(&self.shared),
        })
    }

    /// Sends a JSON-RPC 2.0 request and awaits the typed response (default timeout applies).
    pub async fn send_request<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, LspError> {
        self.send_request_with_timeout(method, params, self.request_timeout)
            .await
    }

    /// Sends a request and awaits the typed response, cancelling it after `timeout`.
    pub async fn send_request_with_timeout<P: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<R, LspError> {
        let value = self
            .start_request(method, params)?
            .wait_timeout(timeout)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Sends a JSON-RPC 2.0 notification (`textDocument/didOpen`, `textDocument/didChange`, etc.).
    pub fn send_notification<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspError> {
        let params_val = serde_json::to_value(params)?;
        self.shared
            .send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                method,
                Some(params_val),
            )))
    }

    /// Performs the `initialize` request, stores the server capabilities and sends
    /// the `initialized` notification.
    pub async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, LspError> {
        let result: InitializeResult = self
            .send_request_with_timeout("initialize", params, INITIALIZE_TIMEOUT)
            .await?;
        *self.capabilities.write() = Some(result.capabilities.clone());
        self.send_notification("initialized", json!({}))?;
        Ok(result)
    }

    /// Gracefully shuts the server down: `shutdown` request, `exit` notification, then
    /// waits for the process to exit and kills it if it does not within `timeout`.
    /// Returns the exit status when a process was owned.
    pub async fn shutdown(&self, timeout: Duration) -> Option<ExitStatus> {
        if !self.is_closed() {
            if let Ok(pending) = self.start_request("shutdown", Value::Null) {
                let _ = pending.wait_timeout(timeout).await;
            }
            let _ = self.shared.send(JsonRpcMessage::Notification(JsonRpcNotification::new(
                "exit", None,
            )));
        }
        let mut guard = self.process.lock().await;
        let child = guard.as_mut()?;
        match tokio::time::timeout(timeout, child.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => {
                let _ = child.child.kill().await;
                child.child.wait().await.ok()
            }
        }
    }

    /// Waits up to `timeout` for the owned server process to exit and returns its status.
    pub async fn wait_for_exit(&self, timeout: Duration) -> Option<ExitStatus> {
        let mut guard = self.process.lock().await;
        let child = guard.as_mut()?;
        match tokio::time::timeout(timeout, child.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            _ => None,
        }
    }

    /// Forcefully kills the owned server process.
    pub async fn kill(&self) {
        let mut guard = self.process.lock().await;
        if let Some(child) = guard.as_mut() {
            let _ = child.child.kill().await;
        }
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
        self.did_change_with_range(uri_str, version, None, new_text)
    }

    /// Sends `textDocument/didChange` with a single change event. `range: None` means a
    /// full-document replacement; `Some(range)` replaces that range.
    pub fn did_change_with_range(
        &self,
        uri_str: &str,
        version: i32,
        range: Option<Range>,
        text: String,
    ) -> Result<(), LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range,
                range_length: None,
                text,
            }],
        };
        self.send_notification("textDocument/didChange", params)
    }

    /// Sends `textDocument/didSave`, optionally including the saved text.
    pub fn did_save(&self, uri_str: &str, text: Option<String>) -> Result<(), LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
            text,
        };
        self.send_notification("textDocument/didSave", params)
    }

    /// Sends `textDocument/didClose`.
    pub fn did_close(&self, uri_str: &str) -> Result<(), LspError> {
        let uri = Self::parse_uri(uri_str)?;
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        };
        self.send_notification("textDocument/didClose", params)
    }

    fn position_params(uri_str: &str, line: u32, character: u32) -> Result<TextDocumentPositionParams, LspError> {
        Ok(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: Self::parse_uri(uri_str)?,
            },
            position: Position { line, character },
        })
    }

    /// Requests `textDocument/completion` at `(line, character)` and normalizes the response.
    pub async fn completion(
        &self,
        uri_str: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<CompletionItem>, LspError> {
        let params = CompletionParams {
            text_document_position: Self::position_params(uri_str, line, character)?,
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
        let params = HoverParams {
            text_document_position_params: Self::position_params(uri_str, line, character)?,
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
        let params = GotoDefinitionParams {
            text_document_position_params: Self::position_params(uri_str, line, character)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
        };
        self.send_request("textDocument/definition", params).await
    }

    /// Requests `textDocument/references` at `(line, character)`.
    pub async fn references(
        &self,
        uri_str: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>, LspError> {
        let params = ReferenceParams {
            text_document_position: Self::position_params(uri_str, line, character)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
            context: ReferenceContext {
                include_declaration,
            },
        };
        let resp: Option<Vec<Location>> =
            self.send_request("textDocument/references", params).await?;
        Ok(resp.unwrap_or_default())
    }

    /// Requests `textDocument/formatting` for the whole document.
    pub async fn formatting(
        &self,
        uri_str: &str,
        options: FormattingOptions,
    ) -> Result<Vec<TextEdit>, LspError> {
        let params = DocumentFormattingParams {
            text_document: TextDocumentIdentifier {
                uri: Self::parse_uri(uri_str)?,
            },
            options,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let resp: Option<Vec<TextEdit>> =
            self.send_request("textDocument/formatting", params).await?;
        Ok(resp.unwrap_or_default())
    }

    /// Returns cached diagnostics published by the server for `uri_str`.
    pub fn diagnostics_for_uri(&self, uri_str: &str) -> Vec<Diagnostic> {
        if let Some(found) = self.shared.diagnostics.read().get(uri_str) {
            return found.clone();
        }
        // Servers may normalize the URI (e.g. percent-encoding); compare parsed forms.
        let Ok(wanted) = Self::parse_uri(uri_str) else {
            return Vec::new();
        };
        self.shared
            .diagnostics
            .read()
            .iter()
            .find(|(k, _)| Self::parse_uri(k).is_ok_and(|u| u == wanted))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Fail outstanding requests promptly; the process is killed by kill_on_drop.
        self.shared.pending.lock().clear();
    }
}
