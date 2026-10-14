//! `ide-core`: Unified IDE workspace engine and Unidirectional Data Flow contract.
//! Integrates `ide-buffer`, `ide-syntax`, `ide-lsp`, `ide-git`, and `ide-agent`.

pub use ide_agent::{
    AgentError, AgentManager, AgentTask, AgentTaskState, AgentTaskStatus, UnifiedDiffPatch,
};
pub use ide_buffer::{BufferEdit, BufferError, Cursor, CursorDirection, SearchMatch, TextBuffer};
pub use ide_git::{
    DiffMarkerKind, FileChangeKind, GitError, GitFileStatus, GitRepository, LineDiffStatus,
};
pub use ide_lsp::{LspClient, LspError, MockTransport, StdioTransport, WebSocketTransport};
pub use ide_syntax::{HighlightSpan, SyntaxEngine, SyntaxError, SyntaxLanguage};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("Buffer error: {0}")]
    Buffer(#[from] BufferError),
    #[error("Syntax error: {0}")]
    Syntax(#[from] SyntaxError),
    #[error("Git error: {0}")]
    Git(#[from] GitError),
    #[error("Agent error: {0}")]
    Agent(#[from] AgentError),
    #[error("LSP error: {0}")]
    Lsp(#[from] LspError),
    #[error("Buffer {0} not found")]
    BufferNotFound(u64),
}

/// Visual cursor rectangle coordinates in line/column space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorRect {
    pub line: u32,
    pub column: u32,
    pub is_primary: bool,
}

/// Visual selection range in 0-indexed line/column coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRange {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// Severity of a diagnostic item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticSeverityLevel {
    Error,
    Warning,
    Information,
    Hint,
}

/// Diagnostic item included in `ViewportViewModel.diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticItem {
    pub line: u32,
    pub start_col: u32,
    pub end_col: u32,
    pub severity: DiagnosticSeverityLevel,
    pub message: String,
}

/// A single rendered line within the visible viewport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedLine {
    pub line_number: u32,
    pub text: String,
    pub spans: Vec<HighlightSpan>,
}

/// Unidirectional Data Flow input action dispatched from Desktop, Android, or iOS UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditorAction {
    OpenFile {
        path: String,
    },
    SaveFile {
        buffer_id: u64,
    },
    InsertText {
        buffer_id: u64,
        text: String,
    },
    DeleteBackward {
        buffer_id: u64,
    },
    DeleteForward {
        buffer_id: u64,
    },
    MoveCursor {
        buffer_id: u64,
        direction: CursorDirection,
        select: bool,
    },
    SetCursorPosition {
        buffer_id: u64,
        line: u32,
        column: u32,
        select: bool,
    },
    ScrollViewport {
        buffer_id: u64,
        first_visible_line: u32,
        visible_line_count: u32,
    },
    Undo {
        buffer_id: u64,
    },
    Redo {
        buffer_id: u64,
    },
    TriggerLspCompletion {
        buffer_id: u64,
    },
    SubmitAgentPrompt {
        prompt: String,
    },
}

/// Immutable ViewModel snapshot exported via UniFFI (Android/iOS) and consumed by Desktop UI.
/// Contains only the lines in the active viewport (`visible_lines`) to sustain 120fps rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewportViewModel {
    pub buffer_id: u64,
    pub file_path: String,
    pub is_dirty: bool,
    pub total_lines: u32,
    pub first_visible_line: u32,
    pub visible_lines: Vec<RenderedLine>,
    pub cursors: Vec<CursorRect>,
    pub selections: Vec<SelectionRange>,
    pub diagnostics: Vec<DiagnosticItem>,
    pub git_diff_markers: Vec<LineDiffStatus>,
    pub agent_tasks: Vec<AgentTaskStatus>,
}

/// Internal per-document session holding buffer, incremental syntax tree, and viewport window.
struct DocumentSession {
    buffer: TextBuffer,
    syntax: SyntaxEngine,
    first_visible_line: u32,
    visible_line_count: u32,
    diagnostics: Vec<DiagnosticItem>,
    completions: Vec<String>,
}

impl DocumentSession {
    fn new(id: u64, path: Option<PathBuf>, content: &str) -> Result<Self, WorkspaceError> {
        let mut buffer = TextBuffer::new(id, content);
        let language = path
            .as_ref()
            .map(SyntaxLanguage::from_path)
            .unwrap_or(SyntaxLanguage::Rust);
        if let Some(p) = path {
            buffer.set_file_path(p);
        }
        let mut syntax = SyntaxEngine::new(language)?;
        syntax.parse_full(&buffer)?;

        Ok(Self {
            buffer,
            syntax,
            first_visible_line: 0,
            visible_line_count: 60,
            diagnostics: Vec::new(),
            completions: Vec::new(),
        })
    }

    fn refresh_syntax_diagnostics(&mut self) {
        self.diagnostics.clear();
        if let Some(tree) = self.syntax.tree() {
            let root = tree.root_node();
            if root.has_error() {
                collect_syntax_error_nodes(root, &self.buffer, &mut self.diagnostics);
            }
        }
    }

    fn compute_completions(&mut self) {
        let mut items: BTreeSet<String> = BTreeSet::new();
        for kw in [
            "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "async", "await",
            "match", "return", "println!", "Some", "None", "Result", "Ok", "Err",
        ] {
            items.insert(kw.to_string());
        }
        for word in self
            .buffer
            .text()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() >= 3)
        {
            items.insert(word.to_string());
        }
        self.completions = items.into_iter().take(24).collect();
    }
}

fn collect_syntax_error_nodes(
    node: ide_syntax::Node<'_>,
    buffer: &TextBuffer,
    out: &mut Vec<DiagnosticItem>,
) {
    if node.is_error() || node.is_missing() {
        let start_char = buffer.byte_to_char(node.start_byte());
        let end_char = buffer.byte_to_char(node.end_byte());
        let (line, start_col) = buffer.char_to_line_col(start_char);
        let (_, end_col) = buffer.char_to_line_col(end_char);
        out.push(DiagnosticItem {
            line,
            start_col,
            end_col: end_col.max(start_col + 1),
            severity: DiagnosticSeverityLevel::Error,
            message: format!("Syntax error near `{}`", node.kind()),
        });
        return;
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.has_error() {
                collect_syntax_error_nodes(child, buffer, out);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Central workspace state coordinating buffers, incremental syntax, Git, LSP, and AI Agents.
pub struct IdeWorkspace {
    root_dir: RwLock<PathBuf>,
    next_buffer_id: AtomicU64,
    active_buffer_id: AtomicU64,
    documents: RwLock<BTreeMap<u64, DocumentSession>>,
    git_repo: RwLock<Option<GitRepository>>,
    agent_manager: AgentManager,
    lsp_client: RwLock<Option<Arc<LspClient>>>,
}

impl Default for IdeWorkspace {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl IdeWorkspace {
    /// Creates a new `IdeWorkspace` rooted at `root_dir` with a default buffer.
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        let root_path = root_dir.as_ref().to_path_buf();
        let git_repo = GitRepository::open(&root_path).ok();

        let ws = Self {
            root_dir: RwLock::new(root_path),
            next_buffer_id: AtomicU64::new(1),
            active_buffer_id: AtomicU64::new(0),
            documents: RwLock::new(BTreeMap::new()),
            git_repo: RwLock::new(git_repo),
            agent_manager: AgentManager::new(),
            lsp_client: RwLock::new(None),
        };

        let _ = ws.create_buffer(
            Some("welcome.rs"),
            "// IDE Levity - Autonomous Multi-Platform Rust IDE\n\
             use std::collections::BTreeMap;\n\n\
             pub struct ProjectConfig {\n\
             \x20   pub name: String,\n\
             \x20   pub target_fps: u32,\n\
             }\n\n\
             fn main() {\n\
             \x20   let config = ProjectConfig {\n\
             \x20       name: String::from(\"IDE Levity v1.0\"),\n\
             \x20       target_fps: 120,\n\
             \x20   };\n\
             \x20   println!(\"Chào mừng đến với {} ({}fps)\", config.name, config.target_fps);\n\
             }\n",
        );
        ws
    }

    pub fn root_dir(&self) -> PathBuf {
        self.root_dir.read().clone()
    }

    pub fn agent_manager(&self) -> &AgentManager {
        &self.agent_manager
    }

    pub fn attach_lsp_client(&self, client: Arc<LspClient>) {
        *self.lsp_client.write() = Some(client);
    }

    pub fn active_buffer_id(&self) -> u64 {
        self.active_buffer_id.load(Ordering::SeqCst)
    }

    pub fn set_active_buffer_id(&self, buffer_id: u64) -> Result<ViewportViewModel, WorkspaceError> {
        if !self.documents.read().contains_key(&buffer_id) {
            return Err(WorkspaceError::BufferNotFound(buffer_id));
        }
        self.active_buffer_id.store(buffer_id, Ordering::SeqCst);
        self.viewport_view_model(buffer_id)
    }

    /// Creates an in-memory buffer with optional file path and returns its `buffer_id`.
    pub fn create_buffer(
        &self,
        path: Option<&str>,
        initial_text: &str,
    ) -> Result<u64, WorkspaceError> {
        let id = self.next_buffer_id.fetch_add(1, Ordering::SeqCst);
        let path_buf = path.map(PathBuf::from);
        let mut session = DocumentSession::new(id, path_buf, initial_text)?;
        session.refresh_syntax_diagnostics();
        self.documents.write().insert(id, session);
        self.active_buffer_id.store(id, Ordering::SeqCst);
        Ok(id)
    }

    /// Closes a buffer by ID (ensuring at least 1 buffer remains open) and returns the active viewport.
    pub fn close_buffer(&self, buffer_id: u64) -> Result<ViewportViewModel, WorkspaceError> {
        {
            let mut docs = self.documents.write();
            if docs.len() > 1 {
                docs.remove(&buffer_id);
                if self.active_buffer_id() == buffer_id {
                    if let Some(&first_id) = docs.keys().next() {
                        self.active_buffer_id.store(first_id, Ordering::SeqCst);
                    }
                }
            }
        }
        self.active_viewport()
    }

    /// Adds a secondary cursor at `(line, column)` for multi-cursor editing.
    pub fn add_cursor(
        &self,
        buffer_id: u64,
        line: u32,
        column: u32,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        {
            let mut docs = self.documents.write();
            let doc = docs
                .get_mut(&buffer_id)
                .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
            doc.buffer.add_cursor(line, column);
        }
        self.viewport_view_model(buffer_id)
    }

    /// Searches `buffer_id` for `query` and returns all `SearchMatch` positions.
    pub fn search_buffer(
        &self,
        buffer_id: u64,
        query: &str,
        case_sensitive: bool,
    ) -> Result<Vec<SearchMatch>, WorkspaceError> {
        let docs = self.documents.read();
        let doc = docs
            .get(&buffer_id)
            .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
        Ok(doc.buffer.search(query, case_sensitive))
    }

    /// Returns the detected `SyntaxLanguage` of `buffer_id`.
    pub fn buffer_language(&self, buffer_id: u64) -> Option<SyntaxLanguage> {
        self.documents
            .read()
            .get(&buffer_id)
            .map(|d| d.syntax.language())
    }

    /// Returns the list of open buffer `(buffer_id, file_path, is_dirty)`.
    pub fn open_buffers(&self) -> Vec<(u64, String, bool)> {
        self.documents
            .read()
            .iter()
            .map(|(&id, doc)| {
                let path = doc
                    .buffer
                    .file_path()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("untitled-{id}"));
                (id, path, doc.buffer.is_dirty())
            })
            .collect()
    }

    /// Returns the full text of `buffer_id`.
    pub fn buffer_text(&self, buffer_id: u64) -> Result<String, WorkspaceError> {
        let docs = self.documents.read();
        let doc = docs
            .get(&buffer_id)
            .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
        Ok(doc.buffer.text())
    }

    /// Returns the latest completion items computed for `buffer_id`.
    pub fn completions(&self, buffer_id: u64) -> Vec<String> {
        self.documents
            .read()
            .get(&buffer_id)
            .map(|d| d.completions.clone())
            .unwrap_or_default()
    }

    /// Returns the current Git branch name if the workspace is inside a Git repository.
    pub fn current_git_branch(&self) -> Option<String> {
        self.git_repo
            .read()
            .as_ref()
            .and_then(|r| r.current_branch().ok())
    }

    /// Returns the list of modified/added/deleted files in the Git working tree.
    pub fn changed_git_files(&self) -> Vec<GitFileStatus> {
        self.git_repo
            .read()
            .as_ref()
            .and_then(|r| r.changed_files().ok())
            .unwrap_or_default()
    }

    /// Lists workspace files relative to `root_dir` for the file explorer sidebar/drawer.
    pub fn list_workspace_files(&self) -> Vec<String> {
        let root = self.root_dir.read().clone();
        let mut files = Vec::new();
        collect_files_recursive(&root, &root, &mut files, 0);
        files.sort();
        files
    }

    /// Applies an AI Agent task's patch to the specified buffer and updates its syntax tree.
    pub fn accept_agent_task(
        &self,
        task_id: u64,
        buffer_id: u64,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        {
            let mut docs = self.documents.write();
            let doc = docs
                .get_mut(&buffer_id)
                .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
            self.agent_manager
                .apply_task_patch(task_id, &mut doc.buffer)?;
            doc.syntax.parse_full(&doc.buffer)?;
            doc.refresh_syntax_diagnostics();
        }
        self.agent_manager
            .verify_task(task_id, true, "Applied and verified by workspace")?;
        self.viewport_view_model(buffer_id)
    }

    /// Rejects an AI Agent task and reverts the buffer if the patch was previously applied.
    pub fn reject_agent_task(
        &self,
        task_id: u64,
        buffer_id: u64,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        {
            let mut docs = self.documents.write();
            let doc = docs
                .get_mut(&buffer_id)
                .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
            self.agent_manager
                .reject_task(task_id, Some(&mut doc.buffer))?;
            doc.syntax.parse_full(&doc.buffer)?;
            doc.refresh_syntax_diagnostics();
        }
        self.viewport_view_model(buffer_id)
    }

    /// Unidirectional Data Flow entrypoint: processes an `EditorAction` and returns the updated `ViewportViewModel`.
    pub fn dispatch(&self, action: EditorAction) -> Result<ViewportViewModel, WorkspaceError> {
        match action {
            EditorAction::OpenFile { path } => {
                if let Some((&existing_id, _)) = self.documents.read().iter().find(|(_, doc)| {
                    doc.buffer
                        .file_path()
                        .is_some_and(|p| p.to_string_lossy() == path)
                }) {
                    self.active_buffer_id.store(existing_id, Ordering::SeqCst);
                    return self.viewport_view_model(existing_id);
                }

                let path_buf = PathBuf::from(&path);
                let resolved_path = if path_buf.is_absolute() || path_buf.exists() {
                    path_buf
                } else {
                    self.root_dir.read().join(&path_buf)
                };
                let content = if resolved_path.exists() {
                    fs::read_to_string(&resolved_path).map_err(BufferError::Io)?
                } else {
                    String::new()
                };

                let id = self.next_buffer_id.fetch_add(1, Ordering::SeqCst);
                let mut session = DocumentSession::new(id, Some(PathBuf::from(&path)), &content)?;
                session.refresh_syntax_diagnostics();

                if let Some(parent) = resolved_path.parent() {
                    if self.git_repo.read().is_none() {
                        if let Ok(repo) = GitRepository::open(parent) {
                            *self.git_repo.write() = Some(repo);
                        }
                    }
                }

                if let Some(lsp) = self.lsp_client.read().as_ref() {
                    let uri = format!("file://{}", resolved_path.to_string_lossy());
                    let _ = lsp.did_open(&uri, "rust", session.buffer.version(), content);
                }

                self.documents.write().insert(id, session);
                self.active_buffer_id.store(id, Ordering::SeqCst);
                self.viewport_view_model(id)
            }
            EditorAction::SaveFile { buffer_id } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    doc.buffer.save()?;
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::InsertText { buffer_id, text } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    let edits = doc.buffer.insert_text(&text);
                    doc.syntax.apply_edits(&doc.buffer, &edits)?;
                    doc.refresh_syntax_diagnostics();

                    if let (Some(lsp), Some(path)) =
                        (self.lsp_client.read().as_ref(), doc.buffer.file_path())
                    {
                        let uri = format!("file://{}", path.to_string_lossy());
                        let _ = lsp.did_change(&uri, doc.buffer.version(), doc.buffer.text());
                    }
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::DeleteBackward { buffer_id } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    let edits = doc.buffer.delete_backward();
                    doc.syntax.apply_edits(&doc.buffer, &edits)?;
                    doc.refresh_syntax_diagnostics();
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::DeleteForward { buffer_id } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    let edits = doc.buffer.delete_forward();
                    doc.syntax.apply_edits(&doc.buffer, &edits)?;
                    doc.refresh_syntax_diagnostics();
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::MoveCursor {
                buffer_id,
                direction,
                select,
            } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    doc.buffer.move_cursors(direction, select);
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::SetCursorPosition {
                buffer_id,
                line,
                column,
                select,
            } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    doc.buffer.set_cursor_position(line, column, select);
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::ScrollViewport {
                buffer_id,
                first_visible_line,
                visible_line_count,
            } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    let max_first = doc.buffer.len_lines().saturating_sub(1);
                    doc.first_visible_line = first_visible_line.min(max_first);
                    doc.visible_line_count = visible_line_count.max(1);
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::Undo { buffer_id } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    let edits = doc.buffer.undo();
                    doc.syntax.apply_edits(&doc.buffer, &edits)?;
                    doc.refresh_syntax_diagnostics();
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::Redo { buffer_id } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    let edits = doc.buffer.redo();
                    doc.syntax.apply_edits(&doc.buffer, &edits)?;
                    doc.refresh_syntax_diagnostics();
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::TriggerLspCompletion { buffer_id } => {
                {
                    let mut docs = self.documents.write();
                    let doc = docs
                        .get_mut(&buffer_id)
                        .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
                    doc.compute_completions();
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::SubmitAgentPrompt { prompt } => {
                let target_id = self.active_buffer_id();
                {
                    let docs = self.documents.read();
                    let doc = docs
                        .get(&target_id)
                        .ok_or(WorkspaceError::BufferNotFound(target_id))?;
                    self.agent_manager
                        .submit_prompt_for_buffer(&prompt, &doc.buffer);
                }
                self.viewport_view_model(target_id)
            }
        }
    }

    /// Builds the `ViewportViewModel` for `buffer_id`, slicing only `visible_lines` for 120fps rendering.
    pub fn viewport_view_model(&self, buffer_id: u64) -> Result<ViewportViewModel, WorkspaceError> {
        let docs = self.documents.read();
        let doc = docs
            .get(&buffer_id)
            .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;

        let total_lines = doc.buffer.len_lines();
        let first_visible = doc.first_visible_line.min(total_lines.saturating_sub(1));
        let end_visible = (first_visible + doc.visible_line_count).min(total_lines);
        let visible_count = end_visible.saturating_sub(first_visible);

        let spans_per_line = doc
            .syntax
            .highlight_lines(&doc.buffer, first_visible, visible_count);

        let mut visible_lines = Vec::with_capacity(visible_count as usize);
        for (i, line_number) in (first_visible..end_visible).enumerate() {
            let text = doc.buffer.line_text(line_number).unwrap_or_default();
            let spans = spans_per_line.get(i).cloned().unwrap_or_default();
            visible_lines.push(RenderedLine {
                line_number,
                text,
                spans,
            });
        }

        let mut cursors = Vec::with_capacity(doc.buffer.cursors().len());
        let mut selections = Vec::new();
        for (idx, cursor) in doc.buffer.cursors().iter().enumerate() {
            let (line, column) = doc.buffer.char_to_line_col(cursor.head);
            cursors.push(CursorRect {
                line,
                column,
                is_primary: idx == 0,
            });
            if cursor.has_selection() {
                let (s_char, e_char) = cursor.selection_range();
                let (start_line, start_col) = doc.buffer.char_to_line_col(s_char);
                let (end_line, end_col) = doc.buffer.char_to_line_col(e_char);
                selections.push(SelectionRange {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                });
            }
        }

        let file_path = doc
            .buffer
            .file_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("untitled-{buffer_id}"));

        let mut diagnostics = doc.diagnostics.clone();
        if let Some(lsp) = self.lsp_client.read().as_ref() {
            let uri = format!("file://{file_path}");
            for d in lsp.diagnostics_for_uri(&uri) {
                diagnostics.push(DiagnosticItem {
                    line: d.range.start.line,
                    start_col: d.range.start.character,
                    end_col: d.range.end.character,
                    severity: DiagnosticSeverityLevel::Error,
                    message: d.message,
                });
            }
        }

        let git_diff_markers =
            if let (Some(repo), Some(path)) = (self.git_repo.read().as_ref(), doc.buffer.file_path()) {
                repo.diff_buffer_lines(path, &doc.buffer.text())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

        let agent_tasks = self.agent_manager.task_statuses();

        Ok(ViewportViewModel {
            buffer_id,
            file_path,
            is_dirty: doc.buffer.is_dirty(),
            total_lines,
            first_visible_line: first_visible,
            visible_lines,
            cursors,
            selections,
            diagnostics,
            git_diff_markers,
            agent_tasks,
        })
    }

    /// Returns the `ViewportViewModel` for the currently active buffer.
    pub fn active_viewport(&self) -> Result<ViewportViewModel, WorkspaceError> {
        self.viewport_view_model(self.active_buffer_id())
    }
}

fn collect_files_recursive(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 5 || !dir.exists() {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(root, &path, out, depth + 1);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unidirectional_data_flow_all_editor_actions() {
        let ws = IdeWorkspace::new(".");
        let buf_id = ws.active_buffer_id();

        let vm = ws
            .dispatch(EditorAction::SetCursorPosition {
                buffer_id: buf_id,
                line: 1,
                column: 0,
                select: false,
            })
            .unwrap();
        assert_eq!(vm.cursors[0].line, 1);
        assert_eq!(vm.cursors[0].column, 0);

        let initial_lines = vm.total_lines;
        let vm = ws
            .dispatch(EditorAction::InsertText {
                buffer_id: buf_id,
                text: "let x = 123;\n".to_string(),
            })
            .unwrap();
        assert!(vm.is_dirty);
        assert_eq!(vm.total_lines, initial_lines + 1);
        assert!(!vm.visible_lines[1].spans.is_empty());

        let vm = ws
            .dispatch(EditorAction::Undo { buffer_id: buf_id })
            .unwrap();
        assert_eq!(vm.total_lines, initial_lines);
        let vm = ws
            .dispatch(EditorAction::Redo { buffer_id: buf_id })
            .unwrap();
        assert_eq!(vm.total_lines, initial_lines + 1);

        ws.dispatch(EditorAction::MoveCursor {
            buffer_id: buf_id,
            direction: CursorDirection::Left,
            select: false,
        })
        .unwrap();
        ws.dispatch(EditorAction::DeleteBackward { buffer_id: buf_id })
            .unwrap();
        ws.dispatch(EditorAction::DeleteForward { buffer_id: buf_id })
            .unwrap();

        ws.dispatch(EditorAction::TriggerLspCompletion { buffer_id: buf_id })
            .unwrap();
        assert!(!ws.completions(buf_id).is_empty());

        let vm = ws
            .dispatch(EditorAction::SubmitAgentPrompt {
                prompt: "123 -> 999".to_string(),
            })
            .unwrap();
        assert_eq!(vm.agent_tasks.len(), 1);
        let task_id = vm.agent_tasks[0].task_id;
        let vm = ws.accept_agent_task(task_id, buf_id).unwrap();
        assert!(vm.visible_lines.iter().any(|l| l.text.contains("999")));
    }

    #[test]
    fn test_viewport_virtualization_120fps_window() {
        let ws = IdeWorkspace::new(".");
        let mut many_lines = String::new();
        for i in 0..500 {
            many_lines.push_str(&format!("let line_{i} = {i};\n"));
        }
        let buf_id = ws.create_buffer(Some("large.rs"), &many_lines).unwrap();

        let vm = ws
            .dispatch(EditorAction::ScrollViewport {
                buffer_id: buf_id,
                first_visible_line: 100,
                visible_line_count: 25,
            })
            .unwrap();

        assert_eq!(vm.total_lines, 501);
        assert_eq!(vm.first_visible_line, 100);
        assert_eq!(vm.visible_lines.len(), 25);
        assert_eq!(vm.visible_lines[0].line_number, 100);
        assert_eq!(vm.visible_lines[24].line_number, 124);
    }

    #[test]
    fn test_multi_tab_search_and_multi_cursor_workspace_methods() {
        let ws = IdeWorkspace::new(".");
        let b1 = ws.active_buffer_id();
        let b2 = ws
            .create_buffer(Some("second.py"), "def hello():\n    return 42\n")
            .unwrap();
        assert_eq!(ws.open_buffers().len(), 2);
        assert_eq!(ws.buffer_language(b2), Some(SyntaxLanguage::Python));

        let matches = ws.search_buffer(b2, "hello", false).unwrap();
        assert_eq!(matches.len(), 1);

        let vm = ws.add_cursor(b2, 1, 4).unwrap();
        assert_eq!(vm.cursors.len(), 2);

        let vm_after_close = ws.close_buffer(b2).unwrap();
        assert_eq!(vm_after_close.buffer_id, b1);
        assert_eq!(ws.open_buffers().len(), 1);
    }
}
