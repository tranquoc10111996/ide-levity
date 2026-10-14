//! [`LanguageServerManager`]: the high-level, GUI-friendly entry point.
//!
//! * Detects the right server for a language (built-in registry + `lsp.json` overrides).
//! * Lazily starts one server per `(server id, project root)` on first `did_open`.
//! * Keeps document state and sends `didOpen` / `didChange` / `didSave` / `didClose`.
//! * Issues cancellable, time-limited requests (completion, hover, definition, ...)
//!   returning [`LspRequest`] handles that are `Future`s *and* can be polled or waited
//!   on synchronously.
//! * Publishes [`LspEvent`]s (diagnostics, server status, messages) on a channel.
//!
//! All methods are non-blocking and may be called from any thread, including a GUI
//! thread that is not inside a Tokio runtime: the manager owns (or borrows) a runtime
//! and every slow operation runs there.

use crate::client::{default_client_capabilities, ClientEvent, ClientOptions, LspClient};
use crate::convert::{
    completion_entries, definition_to_locations, hover_to_text, lsp_location, path_to_uri,
    text_end_position, uri_to_path, CompletionEntry, Location, LspDiagnostic, LspTextEdit,
};
use crate::registry::{
    find_root, normalize_language_id, LspConfig, ServerAvailability, ServerRegistry, ServerSpec,
};
use crate::transport::{SpawnOptions, StdioTransport};
use crate::LspError;
use lsp_types::{
    ClientInfo, CompletionResponse, GotoDefinitionResponse, Hover, HoverProviderCapability,
    InitializeParams, OneOf, Position, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncSaveOptions, TextEdit, Uri,
    WorkspaceFolder,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

/// Receiver of [`LspEvent`]s. Use `recv().await` in async code (e.g. an iced
/// subscription stream), or `try_recv()` / `blocking_recv()` from plain threads.
pub type LspEventReceiver = mpsc::UnboundedReceiver<LspEvent>;

/// Identifies one running language server instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    /// Server id from the registry (e.g. `rust-analyzer`).
    pub server_id: String,
    /// Project root the server was started for.
    pub root: PathBuf,
}

/// Lifecycle state of a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    /// Process spawned, `initialize` in progress.
    Starting,
    /// Initialized and idle.
    Ready,
    /// Working in the background (indexing, loading the workspace, ...).
    Indexing(String),
    /// Could not start or crashed; the message includes stderr output when available.
    Failed(String),
    /// Exited normally (after shutdown).
    Exited,
}

/// Severity of `window/showMessage` / `window/logMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageLevel {
    /// Error.
    Error,
    /// Warning.
    Warning,
    /// Info.
    Info,
    /// Log / debug.
    Log,
}

impl MessageLevel {
    fn from_lsp(value: Option<&Value>) -> Self {
        match value.and_then(Value::as_i64) {
            Some(1) => MessageLevel::Error,
            Some(2) => MessageLevel::Warning,
            Some(3) => MessageLevel::Info,
            _ => MessageLevel::Log,
        }
    }
}

/// Events published by the manager.
#[derive(Debug, Clone, PartialEq)]
pub enum LspEvent {
    /// Full replacement of the diagnostics for one file.
    Diagnostics {
        /// File path.
        path: PathBuf,
        /// Document version the diagnostics refer to, if the server reports it.
        version: Option<i32>,
        /// Diagnostics (empty means "clear").
        items: Vec<LspDiagnostic>,
        /// Server that produced them.
        server_id: String,
    },
    /// A server changed state.
    ServerStatus {
        /// Server id.
        server_id: String,
        /// Language that triggered the server start.
        language_id: String,
        /// Project root.
        root: PathBuf,
        /// New state.
        state: ServerState,
    },
    /// A file was opened for a language whose server is not installed. Sent once per
    /// language until [`LanguageServerManager::rescan_servers`] is called.
    ServerMissing {
        /// Language id.
        language_id: String,
        /// Commands that were searched for.
        commands: Vec<String>,
        /// Installation instructions.
        install_hint: String,
    },
    /// `window/showMessage` (or the text of a `window/showMessageRequest`).
    ShowMessage {
        /// Server id.
        server_id: String,
        /// Severity.
        level: MessageLevel,
        /// Text.
        message: String,
    },
    /// `window/logMessage`.
    LogMessage {
        /// Server id.
        server_id: String,
        /// Severity.
        level: MessageLevel,
        /// Text.
        message: String,
    },
}

/// Outcome of [`LanguageServerManager::did_open`].
#[derive(Debug, Clone, PartialEq)]
pub enum OpenOutcome {
    /// The document is attached to a (starting or running) server.
    Attached(ServerKey),
    /// A server exists for this language but is not installed.
    ServerMissing {
        /// Installation instructions.
        install_hint: String,
    },
    /// Language server support is disabled for this language in `lsp.json`.
    Disabled,
    /// No language server is known for this language.
    Unsupported,
}

/// Snapshot of a known server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    /// Key (server id + root).
    pub key: ServerKey,
    /// Language that triggered the start.
    pub language_id: String,
    /// Executable path.
    pub executable: PathBuf,
    /// Current state.
    pub state: ServerState,
    /// Number of documents currently open on this server.
    pub open_documents: usize,
}

/// A language whose server is not installed.
#[derive(Debug, Clone, PartialEq)]
pub struct MissingServer {
    /// Language id.
    pub language_id: String,
    /// Commands searched for.
    pub commands: Vec<String>,
    /// Installation instructions.
    pub install_hint: String,
}

/// Timeouts and behaviour of the manager.
#[derive(Debug, Clone)]
pub struct ManagerOptions {
    /// User overrides (usually [`LspConfig::load_default`]).
    pub config: LspConfig,
    /// Timeout for completion requests.
    pub completion_timeout: Duration,
    /// Timeout for hover, definition and references requests.
    pub request_timeout: Duration,
    /// Timeout for formatting requests.
    pub formatting_timeout: Duration,
    /// Maximum automatic restarts after crashes within `restart_window`.
    pub max_restarts: usize,
    /// Window for counting automatic restarts.
    pub restart_window: Duration,
    /// Client name reported in `initialize`.
    pub client_name: String,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            config: LspConfig::default(),
            completion_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(15),
            formatting_timeout: Duration::from_secs(15),
            max_restarts: 3,
            restart_window: Duration::from_secs(180),
            client_name: "IDE Levity".to_string(),
        }
    }
}

/// Handle to an in-flight manager request.
///
/// * `.await` it (it is a `Send + 'static` future, usable with `iced::Task::perform`);
/// * or poll it without blocking via [`LspRequest::try_take`];
/// * or block with [`LspRequest::wait`] (never from inside an async context).
///
/// [`LspRequest::cancel`] or dropping the handle cancels the request, which sends
/// `$/cancelRequest` to the server if it was already dispatched.
pub struct LspRequest<T> {
    rx: Option<oneshot::Receiver<Result<T, LspError>>>,
    abort: Option<AbortHandle>,
}

impl<T> LspRequest<T> {
    fn ready(result: Result<T, LspError>) -> Self {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(result);
        Self {
            rx: Some(rx),
            abort: None,
        }
    }

    /// Cancels the request.
    pub fn cancel(&self) {
        if let Some(abort) = &self.abort {
            abort.abort();
        }
    }

    /// Returns the result if it is available, without blocking.
    pub fn try_take(&mut self) -> Option<Result<T, LspError>> {
        let rx = self.rx.as_mut()?;
        match rx.try_recv() {
            Ok(result) => {
                self.rx = None;
                Some(result)
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.rx = None;
                Some(Err(LspError::Cancelled))
            }
        }
    }

    /// Blocks the current thread until the result is available.
    ///
    /// # Panics
    /// Panics if called from within an asynchronous execution context.
    pub fn wait(mut self) -> Result<T, LspError> {
        match self.rx.take() {
            Some(rx) => rx.blocking_recv().unwrap_or(Err(LspError::Cancelled)),
            None => Err(LspError::Cancelled),
        }
    }
}

impl<T> Future for LspRequest<T> {
    type Output = Result<T, LspError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = self.rx.as_mut() else {
            return Poll::Ready(Err(LspError::Cancelled));
        };
        match Pin::new(rx).poll(cx) {
            Poll::Ready(result) => {
                self.rx = None;
                Poll::Ready(result.unwrap_or(Err(LspError::Cancelled)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for LspRequest<T> {
    fn drop(&mut self) {
        if self.rx.is_some() {
            self.cancel();
        }
    }
}

type CapabilityCheck = fn(&ServerCapabilities) -> bool;

enum ServerCommand {
    Open {
        uri: Uri,
        language_id: String,
        version: i32,
        text: String,
    },
    Change {
        uri: Uri,
        version: i32,
        text: String,
        previous_end: Position,
    },
    Save {
        uri: Uri,
        text: Option<String>,
    },
    Close {
        uri: Uri,
    },
    Request {
        method: &'static str,
        params: Value,
        capability: CapabilityCheck,
        reply: oneshot::Sender<Result<Value, LspError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct ServerEntry {
    language_id: String,
    spec: ServerSpec,
    executable: PathBuf,
    cmd_tx: mpsc::UnboundedSender<ServerCommand>,
    state: Arc<RwLock<ServerState>>,
    capabilities: Arc<RwLock<Option<ServerCapabilities>>>,
    generation: u64,
    restarts: Vec<Instant>,
    stopping: Arc<AtomicBool>,
}

struct DocumentState {
    key: ServerKey,
    uri: Uri,
    language_id: String,
    version: i32,
    text: String,
}

struct Inner {
    runtime: Mutex<Option<Runtime>>,
    handle: Handle,
    registry: ServerRegistry,
    options: ManagerOptions,
    servers: Mutex<HashMap<ServerKey, ServerEntry>>,
    documents: Mutex<HashMap<PathBuf, DocumentState>>,
    missing_reported: Mutex<HashSet<String>>,
    availability_cache: Mutex<HashMap<String, (Instant, ServerAvailability)>>,
    events_tx: mpsc::UnboundedSender<LspEvent>,
    next_generation: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Child processes are spawned with kill_on_drop; dropping the tasks kills them.
        if let Some(rt) = self.runtime.lock().take() {
            rt.shutdown_background();
        }
    }
}

const AVAILABILITY_TTL: Duration = Duration::from_secs(30);

/// High-level manager of language servers. Cheap to clone (shared state).
#[derive(Clone)]
pub struct LanguageServerManager {
    inner: Arc<Inner>,
}

impl LanguageServerManager {
    /// Creates a manager that owns a dedicated multi-threaded Tokio runtime.
    pub fn new(options: ManagerOptions) -> Result<(Self, LspEventReceiver), LspError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ide-lsp")
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        Ok(Self::build(Some(runtime), handle, options))
    }

    /// Creates a manager that runs its tasks on an existing runtime.
    pub fn with_handle(handle: Handle, options: ManagerOptions) -> (Self, LspEventReceiver) {
        Self::build(None, handle, options)
    }

    fn build(
        runtime: Option<Runtime>,
        handle: Handle,
        options: ManagerOptions,
    ) -> (Self, LspEventReceiver) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let registry = ServerRegistry::with_config(&options.config);
        let inner = Arc::new(Inner {
            runtime: Mutex::new(runtime),
            handle,
            registry,
            options,
            servers: Mutex::new(HashMap::new()),
            documents: Mutex::new(HashMap::new()),
            missing_reported: Mutex::new(HashSet::new()),
            availability_cache: Mutex::new(HashMap::new()),
            events_tx,
            next_generation: AtomicU64::new(1),
        });
        (Self { inner }, events_rx)
    }

    /// Handle of the runtime the manager uses.
    pub fn runtime_handle(&self) -> Handle {
        self.inner.handle.clone()
    }

    /// The registry (built-in servers plus overrides).
    pub fn registry(&self) -> &ServerRegistry {
        &self.inner.registry
    }

    /// Looks up which server would handle `language_id` (cached for 30 s).
    pub fn availability(&self, language_id: &str) -> ServerAvailability {
        let language = normalize_language_id(language_id);
        if let Some((at, cached)) = self.inner.availability_cache.lock().get(&language) {
            if at.elapsed() < AVAILABILITY_TTL {
                return cached.clone();
            }
        }
        let resolved = self.inner.registry.resolve(&language);
        self.inner
            .availability_cache
            .lock()
            .insert(language, (Instant::now(), resolved.clone()));
        resolved
    }

    /// Forgets cached server detection results (call after installing a server).
    pub fn rescan_servers(&self) {
        self.inner.availability_cache.lock().clear();
        self.inner.missing_reported.lock().clear();
    }

    /// Languages whose servers are known but not installed.
    pub fn missing_servers(&self) -> Vec<MissingServer> {
        self.inner
            .registry
            .languages()
            .into_iter()
            .filter_map(|language_id| match self.availability(&language_id) {
                ServerAvailability::Missing {
                    commands,
                    install_hint,
                } => Some(MissingServer {
                    language_id,
                    commands,
                    install_hint,
                }),
                _ => None,
            })
            .collect()
    }

    /// Snapshot of all servers started so far.
    pub fn servers(&self) -> Vec<ServerInfo> {
        let docs = self.inner.documents.lock();
        let servers = self.inner.servers.lock();
        let mut list: Vec<ServerInfo> = servers
            .iter()
            .map(|(key, entry)| ServerInfo {
                key: key.clone(),
                language_id: entry.language_id.clone(),
                executable: entry.executable.clone(),
                state: entry.state.read().clone(),
                open_documents: docs.values().filter(|d| &d.key == key).count(),
            })
            .collect();
        list.sort_by(|a, b| (&a.key.server_id, &a.key.root).cmp(&(&b.key.server_id, &b.key.root)));
        list
    }

    /// Server responsible for an open document.
    pub fn server_for(&self, path: &Path) -> Option<ServerKey> {
        let path = absolute(path);
        self.inner.documents.lock().get(&path).map(|d| d.key.clone())
    }

    /// Characters that should automatically trigger completion for `path`
    /// (from the server's capabilities; empty until the server is initialized).
    pub fn completion_trigger_characters(&self, path: &Path) -> Vec<String> {
        let Some(key) = self.server_for(path) else {
            return Vec::new();
        };
        let servers = self.inner.servers.lock();
        servers
            .get(&key)
            .and_then(|e| e.capabilities.read().clone())
            .and_then(|c| c.completion_provider)
            .and_then(|c| c.trigger_characters)
            .unwrap_or_default()
    }

    /// Opens a document, starting its language server if needed.
    pub fn did_open(&self, path: &Path, language_id: &str, version: i32, text: &str) -> OpenOutcome {
        let path = absolute(path);
        let language = normalize_language_id(language_id);
        let (spec, executable) = match self.availability(&language) {
            ServerAvailability::Available { spec, executable } => (spec, executable),
            ServerAvailability::Missing {
                commands,
                install_hint,
            } => {
                if self.inner.missing_reported.lock().insert(language.clone()) {
                    let _ = self.inner.events_tx.send(LspEvent::ServerMissing {
                        language_id: language.clone(),
                        commands,
                        install_hint: install_hint.clone(),
                    });
                }
                return OpenOutcome::ServerMissing { install_hint };
            }
            ServerAvailability::Disabled => return OpenOutcome::Disabled,
            ServerAvailability::Unsupported => return OpenOutcome::Unsupported,
        };
        let Ok(uri) = path_to_uri(&path) else {
            return OpenOutcome::Unsupported;
        };
        let root = find_root(&path, &spec.root_markers);
        let key = ServerKey {
            server_id: spec.id.clone(),
            root,
        };

        let mut docs = self.inner.documents.lock();
        if let Some(previous) = docs.remove(&path) {
            self.send_to(&previous.key, ServerCommand::Close { uri: previous.uri });
        }
        Inner::ensure_server(&self.inner, &key, &language, &spec, &executable);
        self.send_to(
            &key,
            ServerCommand::Open {
                uri: uri.clone(),
                language_id: language.clone(),
                version,
                text: text.to_string(),
            },
        );
        docs.insert(
            path,
            DocumentState {
                key: key.clone(),
                uri,
                language_id: language,
                version,
                text: text.to_string(),
            },
        );
        OpenOutcome::Attached(key)
    }

    /// Replaces the content of an open document (full-text sync; converted to a
    /// single full-range edit for servers that only accept incremental changes).
    pub fn did_change(&self, path: &Path, version: i32, full_text: &str) -> Result<(), LspError> {
        let path = absolute(path);
        let mut docs = self.inner.documents.lock();
        let doc = docs
            .get_mut(&path)
            .ok_or_else(|| LspError::DocumentNotOpen(path.display().to_string()))?;
        let previous_end: Position = text_end_position(&doc.text).into();
        doc.version = version;
        doc.text = full_text.to_string();
        let (key, uri) = (doc.key.clone(), doc.uri.clone());
        drop(docs);
        self.send_to(
            &key,
            ServerCommand::Change {
                uri,
                version,
                text: full_text.to_string(),
                previous_end,
            },
        );
        Ok(())
    }

    /// Notifies the server that a document was saved. `text` (the saved content) is
    /// forwarded when the server asks for it.
    pub fn did_save(&self, path: &Path, text: Option<&str>) -> Result<(), LspError> {
        let path = absolute(path);
        let mut docs = self.inner.documents.lock();
        let doc = docs
            .get_mut(&path)
            .ok_or_else(|| LspError::DocumentNotOpen(path.display().to_string()))?;
        if let Some(text) = text {
            doc.text = text.to_string();
        }
        let saved = doc.text.clone();
        let (key, uri) = (doc.key.clone(), doc.uri.clone());
        drop(docs);
        self.send_to(
            &key,
            ServerCommand::Save {
                uri,
                text: Some(saved),
            },
        );
        Ok(())
    }

    /// Closes a document.
    pub fn did_close(&self, path: &Path) -> Result<(), LspError> {
        let path = absolute(path);
        let doc = self
            .inner
            .documents
            .lock()
            .remove(&path)
            .ok_or_else(|| LspError::DocumentNotOpen(path.display().to_string()))?;
        self.send_to(&doc.key, ServerCommand::Close { uri: doc.uri });
        Ok(())
    }

    fn send_to(&self, key: &ServerKey, cmd: ServerCommand) -> bool {
        let servers = self.inner.servers.lock();
        servers
            .get(key)
            .is_some_and(|entry| entry.cmd_tx.send(cmd).is_ok())
    }

    fn request<T, F>(
        &self,
        path: &Path,
        method: &'static str,
        params: impl FnOnce(Uri) -> Value,
        capability: CapabilityCheck,
        timeout: Duration,
        convert: F,
    ) -> LspRequest<T>
    where
        T: Send + 'static,
        F: FnOnce(Value) -> Result<T, LspError> + Send + 'static,
    {
        let path = absolute(path);
        let (key, uri) = match self.inner.documents.lock().get(&path) {
            Some(doc) => (doc.key.clone(), doc.uri.clone()),
            None => {
                return LspRequest::ready(Err(LspError::DocumentNotOpen(
                    path.display().to_string(),
                )))
            }
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let sent = self.send_to(
            &key,
            ServerCommand::Request {
                method,
                params: params(uri),
                capability,
                reply: reply_tx,
            },
        );
        if !sent {
            return LspRequest::ready(Err(LspError::ServerNotRunning(key.server_id)));
        }
        let (result_tx, result_rx) = oneshot::channel();
        let server_id = key.server_id;
        let task = self.inner.handle.spawn(async move {
            let outcome = match tokio::time::timeout(timeout, reply_rx).await {
                Err(_) => Err(LspError::Timeout {
                    method: method.to_string(),
                    timeout,
                }),
                Ok(Err(_)) => Err(LspError::ServerNotRunning(server_id)),
                Ok(Ok(Err(e))) => Err(e),
                Ok(Ok(Ok(value))) => convert(value),
            };
            let _ = result_tx.send(outcome);
        });
        LspRequest {
            rx: Some(result_rx),
            abort: Some(task.abort_handle()),
        }
    }

    /// Requests completions at `(line, utf16_col)`.
    pub fn completion(&self, path: &Path, line: u32, utf16_col: u32) -> LspRequest<Vec<CompletionEntry>> {
        self.request(
            path,
            "textDocument/completion",
            |uri| position_params(&uri, line, utf16_col),
            |c| c.completion_provider.is_some(),
            self.inner.options.completion_timeout,
            |v| {
                let resp: Option<CompletionResponse> = serde_json::from_value(v)?;
                Ok(completion_entries(resp))
            },
        )
    }

    /// Requests hover text (markdown) at `(line, utf16_col)`.
    pub fn hover(&self, path: &Path, line: u32, utf16_col: u32) -> LspRequest<Option<String>> {
        self.request(
            path,
            "textDocument/hover",
            |uri| position_params(&uri, line, utf16_col),
            |c| {
                !matches!(
                    c.hover_provider,
                    None | Some(HoverProviderCapability::Simple(false))
                )
            },
            self.inner.options.request_timeout,
            |v| {
                let hover: Option<Hover> = serde_json::from_value(v)?;
                Ok(hover
                    .map(|h| hover_to_text(&h))
                    .filter(|t| !t.trim().is_empty()))
            },
        )
    }

    /// Requests the definition(s) of the symbol at `(line, utf16_col)`.
    pub fn definition(&self, path: &Path, line: u32, utf16_col: u32) -> LspRequest<Vec<Location>> {
        self.request(
            path,
            "textDocument/definition",
            |uri| position_params(&uri, line, utf16_col),
            |c| !matches!(c.definition_provider, None | Some(OneOf::Left(false))),
            self.inner.options.request_timeout,
            |v| {
                let resp: Option<GotoDefinitionResponse> = serde_json::from_value(v)?;
                Ok(resp.map(definition_to_locations).unwrap_or_default())
            },
        )
    }

    /// Requests all references to the symbol at `(line, utf16_col)`.
    pub fn references(
        &self,
        path: &Path,
        line: u32,
        utf16_col: u32,
        include_declaration: bool,
    ) -> LspRequest<Vec<Location>> {
        self.request(
            path,
            "textDocument/references",
            |uri| {
                let mut params = position_params(&uri, line, utf16_col);
                params["context"] = json!({ "includeDeclaration": include_declaration });
                params
            },
            |c| !matches!(c.references_provider, None | Some(OneOf::Left(false))),
            self.inner.options.request_timeout,
            |v| {
                let locs: Option<Vec<lsp_types::Location>> = serde_json::from_value(v)?;
                Ok(locs
                    .unwrap_or_default()
                    .iter()
                    .filter_map(lsp_location)
                    .collect())
            },
        )
    }

    /// Requests whole-document formatting edits.
    pub fn formatting(
        &self,
        path: &Path,
        tab_size: u32,
        insert_spaces: bool,
    ) -> LspRequest<Vec<LspTextEdit>> {
        self.request(
            path,
            "textDocument/formatting",
            |uri| {
                json!({
                    "textDocument": { "uri": uri },
                    "options": { "tabSize": tab_size, "insertSpaces": insert_spaces }
                })
            },
            |c| {
                !matches!(
                    c.document_formatting_provider,
                    None | Some(OneOf::Left(false))
                )
            },
            self.inner.options.formatting_timeout,
            |v| {
                let edits: Option<Vec<TextEdit>> = serde_json::from_value(v)?;
                Ok(edits
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| LspTextEdit {
                        range: e.range.into(),
                        new_text: e.new_text,
                    })
                    .collect())
            },
        )
    }

    /// Restarts a server (shutting the old instance down) and re-opens its documents.
    pub fn restart_server(&self, key: &ServerKey) -> LspRequest<()> {
        let inner = Arc::clone(&self.inner);
        let key = key.clone();
        let (tx, rx) = oneshot::channel();
        let task = self.inner.handle.spawn(async move {
            let shutdown = {
                let servers = inner.servers.lock();
                servers.get(&key).map(|entry| {
                    entry.stopping.store(true, Ordering::SeqCst);
                    let (reply, wait) = oneshot::channel();
                    let _ = entry.cmd_tx.send(ServerCommand::Shutdown { reply });
                    wait
                })
            };
            let Some(wait) = shutdown else {
                let _ = tx.send(Err(LspError::ServerNotRunning(key.server_id.clone())));
                return;
            };
            let _ = wait.await;
            Inner::respawn(&inner, &key, false);
            let _ = tx.send(Ok(()));
        });
        LspRequest {
            rx: Some(rx),
            abort: Some(task.abort_handle()),
        }
    }

    /// Gracefully shuts down every server (`shutdown` + `exit`, kill on timeout).
    pub fn shutdown_all(&self) -> LspRequest<()> {
        let waits: Vec<oneshot::Receiver<()>> = {
            let servers = self.inner.servers.lock();
            servers
                .values()
                .filter_map(|entry| {
                    entry.stopping.store(true, Ordering::SeqCst);
                    let (reply, wait) = oneshot::channel();
                    entry
                        .cmd_tx
                        .send(ServerCommand::Shutdown { reply })
                        .ok()
                        .map(|_| wait)
                })
                .collect()
        };
        let (tx, rx) = oneshot::channel();
        let task = self.inner.handle.spawn(async move {
            for wait in waits {
                let _ = wait.await;
            }
            let _ = tx.send(Ok(()));
        });
        LspRequest {
            rx: Some(rx),
            abort: Some(task.abort_handle()),
        }
    }
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn position_params(uri: &Uri, line: u32, character: u32) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character }
    })
}

impl Inner {
    /// Starts the server for `key` unless an entry already exists.
    fn ensure_server(
        this: &Arc<Self>,
        key: &ServerKey,
        language_id: &str,
        spec: &ServerSpec,
        executable: &Path,
    ) {
        let mut servers = this.servers.lock();
        if servers.contains_key(key) {
            return;
        }
        let entry = Self::spawn_actor(this, key, language_id, spec, executable, Vec::new());
        servers.insert(key.clone(), entry);
    }

    fn spawn_actor(
        this: &Arc<Self>,
        key: &ServerKey,
        language_id: &str,
        spec: &ServerSpec,
        executable: &Path,
        restarts: Vec<Instant>,
    ) -> ServerEntry {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RwLock::new(ServerState::Starting));
        let capabilities = Arc::new(RwLock::new(None));
        let stopping = Arc::new(AtomicBool::new(false));
        let generation = this.next_generation.fetch_add(1, Ordering::SeqCst);
        let actor = ServerActor {
            inner: Arc::downgrade(this),
            key: key.clone(),
            language_id: language_id.to_string(),
            spec: spec.clone(),
            executable: executable.to_path_buf(),
            state: Arc::clone(&state),
            capabilities: Arc::clone(&capabilities),
            stopping: Arc::clone(&stopping),
            generation,
            events_tx: this.events_tx.clone(),
            progress: HashMap::new(),
            quiescent: None,
            request_timeout: this.options.request_timeout,
            client_name: this.options.client_name.clone(),
        };
        this.handle.spawn(actor.run(cmd_rx));
        ServerEntry {
            language_id: language_id.to_string(),
            spec: spec.clone(),
            executable: executable.to_path_buf(),
            cmd_tx,
            state,
            capabilities,
            generation,
            restarts,
            stopping,
        }
    }

    /// Replaces the server for `key` with a fresh instance and replays open documents.
    fn respawn(this: &Arc<Self>, key: &ServerKey, count_as_crash_restart: bool) {
        let docs = this.documents.lock();
        let mut servers = this.servers.lock();
        let Some(old) = servers.get(key) else { return };
        let mut restarts = old.restarts.clone();
        if count_as_crash_restart {
            restarts.push(Instant::now());
        }
        let entry = Self::spawn_actor(
            this,
            key,
            &old.language_id,
            &old.spec.clone(),
            &old.executable.clone(),
            restarts,
        );
        for doc in docs.values().filter(|d| &d.key == key) {
            let _ = entry.cmd_tx.send(ServerCommand::Open {
                uri: doc.uri.clone(),
                language_id: doc.language_id.clone(),
                version: doc.version,
                text: doc.text.clone(),
            });
        }
        servers.insert(key.clone(), entry);
    }

    /// Called by an actor whose server exited unexpectedly.
    fn on_unexpected_exit(this: &Arc<Self>, key: &ServerKey, generation: u64) {
        let allowed = {
            let mut servers = this.servers.lock();
            let Some(entry) = servers.get_mut(key) else { return };
            if entry.generation != generation {
                return;
            }
            let window = this.options.restart_window;
            entry.restarts.retain(|t| t.elapsed() < window);
            entry.restarts.len() < this.options.max_restarts
        };
        if allowed {
            Self::respawn(this, key, true);
        }
    }
}

struct ServerActor {
    inner: Weak<Inner>,
    key: ServerKey,
    language_id: String,
    spec: ServerSpec,
    executable: PathBuf,
    state: Arc<RwLock<ServerState>>,
    capabilities: Arc<RwLock<Option<ServerCapabilities>>>,
    stopping: Arc<AtomicBool>,
    generation: u64,
    events_tx: mpsc::UnboundedSender<LspEvent>,
    progress: HashMap<String, ProgressInfo>,
    quiescent: Option<bool>,
    request_timeout: Duration,
    client_name: String,
}

#[derive(Default, Clone)]
struct ProgressInfo {
    title: String,
    message: Option<String>,
    percentage: Option<u64>,
}

impl ProgressInfo {
    fn describe(&self) -> String {
        let mut text = self.title.clone();
        if let Some(message) = self.message.as_deref().filter(|m| !m.is_empty()) {
            if !text.is_empty() {
                text.push_str(": ");
            }
            text.push_str(message);
        }
        if let Some(p) = self.percentage {
            text.push_str(&format!(" ({p}%)"));
        }
        text
    }
}

impl ServerActor {
    fn set_state(&self, state: ServerState) {
        {
            let mut current = self.state.write();
            if *current == state {
                return;
            }
            *current = state.clone();
        }
        let _ = self.events_tx.send(LspEvent::ServerStatus {
            server_id: self.key.server_id.clone(),
            language_id: self.language_id.clone(),
            root: self.key.root.clone(),
            state,
        });
    }

    fn recompute_activity_state(&self) {
        if matches!(
            *self.state.read(),
            ServerState::Starting | ServerState::Failed(_) | ServerState::Exited
        ) {
            return;
        }
        if let Some(info) = self.progress.values().last() {
            self.set_state(ServerState::Indexing(info.describe()));
        } else if self.quiescent == Some(false) {
            self.set_state(ServerState::Indexing("Loading workspace".into()));
        } else {
            self.set_state(ServerState::Ready);
        }
    }

    fn failure_message(&self, headline: &str, client: Option<&LspClient>) -> String {
        let mut msg = headline.to_string();
        if let Some(stderr) = client.map(LspClient::stderr_tail).filter(|s| !s.is_empty()) {
            msg.push('\n');
            msg.push_str(&stderr);
        }
        msg
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<ServerCommand>) {
        let _ = self.events_tx.send(LspEvent::ServerStatus {
            server_id: self.key.server_id.clone(),
            language_id: self.language_id.clone(),
            root: self.key.root.clone(),
            state: ServerState::Starting,
        });

        let spawn = SpawnOptions {
            program: self.executable.to_string_lossy().into_owned(),
            args: self.spec.args.clone(),
            cwd: Some(self.key.root.clone()),
            env: self.spec.env.clone(),
            extra_path: self
                .executable
                .parent()
                .map(|p| vec![p.to_path_buf()])
                .unwrap_or_default(),
        };
        let transport = match StdioTransport::spawn_with(&spawn) {
            Ok(t) => t,
            Err(e) => {
                self.set_state(ServerState::Failed(format!(
                    "{e}\n{}",
                    self.spec.install_hint
                )));
                fail_pending(cmd_rx, &self.key.server_id);
                return;
            }
        };

        let root_uri = path_to_uri(&self.key.root).ok();
        let folder_name = self
            .key
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.key.root.display().to_string());
        let workspace_folders: Vec<WorkspaceFolder> = root_uri
            .iter()
            .map(|uri| WorkspaceFolder {
                uri: uri.clone(),
                name: folder_name.clone(),
            })
            .collect();
        let (client, mut events) = LspClient::with_options(
            transport.into_channels(),
            ClientOptions {
                request_timeout: self.request_timeout,
                settings: self.spec.settings.clone(),
                workspace_folders: workspace_folders.clone(),
            },
        );
        let client = Arc::new(client);

        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root_uri.clone(),
            root_path: Some(self.key.root.to_string_lossy().into_owned()),
            initialization_options: self.spec.initialization_options.clone(),
            capabilities: default_client_capabilities(),
            client_info: Some(ClientInfo {
                name: self.client_name.clone(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            workspace_folders: Some(workspace_folders),
            ..Default::default()
        };

        // Initialize while still draining events (servers may log during startup).
        let init = client.initialize(params);
        tokio::pin!(init);
        let init_result = loop {
            tokio::select! {
                result = &mut init => break result,
                Some(event) = events.recv() => {
                    if !matches!(event, ClientEvent::Closed) {
                        self.handle_event(event);
                    }
                }
            }
        };
        match init_result {
            Ok(result) => {
                *self.capabilities.write() = Some(result.capabilities);
                *self.state.write() = ServerState::Ready;
                let _ = self.events_tx.send(LspEvent::ServerStatus {
                    server_id: self.key.server_id.clone(),
                    language_id: self.language_id.clone(),
                    root: self.key.root.clone(),
                    state: ServerState::Ready,
                });
                self.recompute_activity_state();
            }
            Err(e) => {
                let status = client.wait_for_exit(Duration::from_millis(500)).await;
                let headline = match status {
                    Some(status) => format!(
                        "{} exited during initialization ({status}): {e}",
                        self.key.server_id
                    ),
                    None => format!("{} failed to initialize: {e}", self.key.server_id),
                };
                self.set_state(ServerState::Failed(
                    self.failure_message(&headline, Some(&client)),
                ));
                client.kill().await;
                fail_pending(cmd_rx, &self.key.server_id);
                return;
            }
        }

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(ServerCommand::Shutdown { reply }) => {
                        self.stopping.store(true, Ordering::SeqCst);
                        client.shutdown(Duration::from_secs(5)).await;
                        self.set_state(ServerState::Exited);
                        let _ = reply.send(());
                        return;
                    }
                    Some(cmd) => self.handle_command(&client, cmd),
                    None => {
                        // Manager dropped: shut the server down politely.
                        self.stopping.store(true, Ordering::SeqCst);
                        client.shutdown(Duration::from_secs(2)).await;
                        return;
                    }
                },
                event = events.recv() => match event {
                    Some(ClientEvent::Closed) | None => {
                        let status = client.wait_for_exit(Duration::from_secs(2)).await;
                        if self.stopping.load(Ordering::SeqCst) {
                            self.set_state(ServerState::Exited);
                        } else {
                            let headline = match status {
                                Some(status) => format!("{} exited unexpectedly ({status})", self.key.server_id),
                                None => format!("{} closed its connection unexpectedly", self.key.server_id),
                            };
                            self.set_state(ServerState::Failed(self.failure_message(&headline, Some(&client))));
                            client.kill().await;
                            drop(cmd_rx);
                            if let Some(inner) = self.inner.upgrade() {
                                Inner::on_unexpected_exit(&inner, &self.key, self.generation);
                            }
                        }
                        return;
                    }
                    Some(event) => self.handle_event(event),
                },
            }
        }
    }

    fn handle_command(&self, client: &Arc<LspClient>, cmd: ServerCommand) {
        let caps = self.capabilities.read().clone().unwrap_or_default();
        let uri_str = |uri: &Uri| uri.as_str().to_string();
        match cmd {
            ServerCommand::Open {
                uri,
                language_id,
                version,
                text,
            } => {
                let _ = client.did_open(&uri_str(&uri), &language_id, version, text);
            }
            ServerCommand::Change {
                uri,
                version,
                text,
                previous_end,
            } => {
                let kind = match &caps.text_document_sync {
                    Some(TextDocumentSyncCapability::Kind(kind)) => *kind,
                    Some(TextDocumentSyncCapability::Options(opts)) => {
                        opts.change.unwrap_or(TextDocumentSyncKind::NONE)
                    }
                    None => TextDocumentSyncKind::NONE,
                };
                let range = if kind == TextDocumentSyncKind::INCREMENTAL {
                    Some(Range {
                        start: Position::default(),
                        end: previous_end,
                    })
                } else if kind == TextDocumentSyncKind::FULL {
                    None
                } else {
                    return;
                };
                let _ = client.did_change_with_range(&uri_str(&uri), version, range, text);
            }
            ServerCommand::Save { uri, text } => {
                let (send, include_text) = match &caps.text_document_sync {
                    Some(TextDocumentSyncCapability::Options(opts)) => match &opts.save {
                        Some(TextDocumentSyncSaveOptions::Supported(supported)) => {
                            (*supported, false)
                        }
                        Some(TextDocumentSyncSaveOptions::SaveOptions(o)) => {
                            (true, o.include_text.unwrap_or(false))
                        }
                        None => (false, false),
                    },
                    // Kind-only sync does not specify save support; the notification is
                    // harmless and some servers (e.g. typescript-language-server) use it.
                    Some(TextDocumentSyncCapability::Kind(_)) => (true, false),
                    None => (false, false),
                };
                if send {
                    let _ = client.did_save(&uri_str(&uri), text.filter(|_| include_text));
                }
            }
            ServerCommand::Close { uri } => {
                let _ = client.did_close(&uri_str(&uri));
            }
            ServerCommand::Request {
                method,
                params,
                capability,
                mut reply,
            } => {
                if !capability(&caps) {
                    let _ = reply.send(Ok(Value::Null));
                    return;
                }
                match client.start_request(method, params) {
                    Ok(pending) => {
                        tokio::spawn(async move {
                            tokio::select! {
                                result = pending.wait() => { let _ = reply.send(result); }
                                // Caller cancelled or timed out: dropping `pending`
                                // sends $/cancelRequest.
                                _ = reply.closed() => {}
                            }
                        });
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            ServerCommand::Shutdown { .. } => {}
        }
    }

    fn handle_event(&mut self, event: ClientEvent) {
        let server_id = self.key.server_id.clone();
        match event {
            ClientEvent::Notification(notif) => {
                let params = notif.params.unwrap_or(Value::Null);
                match notif.method.as_str() {
                    "textDocument/publishDiagnostics" => {
                        if let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params) {
                            if let Some(path) = uri_to_path(&p.uri) {
                                let _ = self.events_tx.send(LspEvent::Diagnostics {
                                    path,
                                    version: p.version,
                                    items: p.diagnostics.into_iter().map(Into::into).collect(),
                                    server_id,
                                });
                            }
                        }
                    }
                    "window/showMessage" | "window/logMessage" => {
                        let level = MessageLevel::from_lsp(params.get("type"));
                        let message = params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let event = if notif.method == "window/showMessage" {
                            LspEvent::ShowMessage {
                                server_id,
                                level,
                                message,
                            }
                        } else {
                            LspEvent::LogMessage {
                                server_id,
                                level,
                                message,
                            }
                        };
                        let _ = self.events_tx.send(event);
                    }
                    "$/progress" => {
                        let token = match params.get("token") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => return,
                        };
                        let value = params.get("value").cloned().unwrap_or(Value::Null);
                        let text = |k: &str| value.get(k).and_then(Value::as_str).map(str::to_string);
                        match value.get("kind").and_then(Value::as_str) {
                            Some("begin") => {
                                self.progress.insert(
                                    token,
                                    ProgressInfo {
                                        title: text("title").unwrap_or_default(),
                                        message: text("message"),
                                        percentage: value.get("percentage").and_then(Value::as_u64),
                                    },
                                );
                            }
                            Some("report") => {
                                if let Some(info) = self.progress.get_mut(&token) {
                                    if let Some(m) = text("message") {
                                        info.message = Some(m);
                                    }
                                    if let Some(p) = value.get("percentage").and_then(Value::as_u64) {
                                        info.percentage = Some(p);
                                    }
                                }
                            }
                            Some("end") => {
                                self.progress.remove(&token);
                            }
                            _ => return,
                        }
                        self.recompute_activity_state();
                    }
                    "experimental/serverStatus" => {
                        self.quiescent = params.get("quiescent").and_then(Value::as_bool);
                        if params.get("health").and_then(Value::as_str) == Some("error") {
                            if let Some(message) = params.get("message").and_then(Value::as_str) {
                                let _ = self.events_tx.send(LspEvent::ShowMessage {
                                    server_id,
                                    level: MessageLevel::Error,
                                    message: message.to_string(),
                                });
                            }
                        }
                        self.recompute_activity_state();
                    }
                    _ => {}
                }
            }
            ClientEvent::ServerRequest { method, params } => {
                if method == "window/showMessageRequest" {
                    let params = params.unwrap_or(Value::Null);
                    let _ = self.events_tx.send(LspEvent::ShowMessage {
                        server_id,
                        level: MessageLevel::from_lsp(params.get("type")),
                        message: params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
            }
            ClientEvent::Closed => {}
        }
    }
}

/// Answers queued requests of a server that will never run.
fn fail_pending(mut cmd_rx: mpsc::UnboundedReceiver<ServerCommand>, server_id: &str) {
    cmd_rx.close();
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            ServerCommand::Request { reply, .. } => {
                let _ = reply.send(Err(LspError::ServerNotRunning(server_id.to_string())));
            }
            ServerCommand::Shutdown { reply } => {
                let _ = reply.send(());
            }
            _ => {}
        }
    }
}
