//! `ide-core`: Unified IDE workspace engine and Unidirectional Data Flow contract.
//!
//! Integrates `ide-buffer` (rope text + cursors + undo), `ide-syntax` (incremental tree-sitter),
//! `ide-git` (gutter diff markers) and `ide-agent` (reviewable AI patches) behind a single
//! thread-safe [`IdeWorkspace`]. All platforms (Desktop, Android, iOS) drive it through
//! [`EditorAction`]s and render the returned [`ViewportViewModel`].
//!
//! Language servers are intentionally not owned here: a frontend registers a
//! [`DocumentObserver`] to mirror document lifecycle events into its LSP layer and pushes
//! diagnostics back with [`IdeWorkspace::set_external_diagnostics`].

pub use ide_agent::{
    AgentError, AgentManager, AgentTask, AgentTaskState, AgentTaskStatus, UnifiedDiffPatch,
};
pub use ide_buffer::{BufferEdit, BufferError, Cursor, CursorDirection, SearchMatch, TextBuffer};
pub use ide_git::{
    BranchInfo, ChangeKind, CommitSummary, DiffMarkerKind, FileChangeKind, GitError,
    GitFileStatus, GitRepository, LineDiffStatus, StatusEntry,
};
pub use ide_syntax::{HighlightSpan, SyntaxEngine, SyntaxError, SyntaxLanguage};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;

/// Files larger than this are refused by the editor (they would make the UI unusable).
pub const MAX_EDITABLE_FILE_BYTES: u64 = 64 * 1024 * 1024;

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
    #[error("Buffer {0} not found")]
    BufferNotFound(u64),
    #[error("File not found: {0}")]
    FileNotFound(String),
    #[error("{0} is a directory")]
    IsDirectory(String),
    #[error("{0} is a binary or non UTF-8 file and cannot be edited as text")]
    BinaryFile(String),
    #[error("{path} is too large to edit ({size} bytes)")]
    FileTooLarge { path: String, size: u64 },
    #[error("Path {0} is outside the workspace folder")]
    OutsideWorkspace(String),
    #[error("Path already exists: {0}")]
    AlreadyExists(String),
    #[error("Buffer {0} has no file path yet; use Save As")]
    Untitled(u64),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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

/// Diagnostic item included in `ViewportViewModel.diagnostics` (char columns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticItem {
    pub line: u32,
    pub start_col: u32,
    pub end_col: u32,
    pub severity: DiagnosticSeverityLevel,
    pub message: String,
}

/// Diagnostic reported by an external tool (e.g. a language server) in LSP coordinates
/// (0-based lines, UTF-16 code unit columns). Converted to char columns when rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDiagnostic {
    pub start_line: u32,
    pub start_utf16: u32,
    pub end_line: u32,
    pub end_utf16: u32,
    pub severity: DiagnosticSeverityLevel,
    pub message: String,
    pub source: Option<String>,
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
    /// Enter key: newline with auto-indentation.
    InsertNewline {
        buffer_id: u64,
    },
    /// Tab key: indents selected lines, or inserts one indent unit at each cursor.
    Indent {
        buffer_id: u64,
    },
    /// Shift+Tab: removes one indent level from the touched lines.
    Outdent {
        buffer_id: u64,
    },
    /// Ctrl+/: toggles the language's line comment on the touched lines.
    ToggleLineComment {
        buffer_id: u64,
    },
    /// Ctrl+A.
    SelectAll {
        buffer_id: u64,
    },
    /// Ctrl+D: select word / add next occurrence as a new cursor.
    AddNextOccurrence {
        buffer_id: u64,
    },
    /// Ctrl+Backspace.
    DeleteWordBackward {
        buffer_id: u64,
    },
    /// Ctrl+Delete.
    DeleteWordForward {
        buffer_id: u64,
    },
    /// Removes the selection, or the whole cursor line(s) when nothing is selected (Cut).
    DeleteSelectionOrLine {
        buffer_id: u64,
    },
    /// PageUp / PageDown: moves cursors by one viewport height (negative = up) and scrolls.
    MovePage {
        buffer_id: u64,
        pages: i32,
        select: bool,
    },
    /// Escape: collapses multiple cursors / selections to the primary cursor.
    CollapseCursors {
        buffer_id: u64,
    },
}

/// Immutable ViewModel snapshot exported via UniFFI (Android/iOS) and consumed by Desktop UI.
/// Contains only the lines in the active viewport (`visible_lines`).
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

/// Summary of an open buffer for tab bars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenBufferInfo {
    pub buffer_id: u64,
    /// Absolute path, or `None` for untitled buffers.
    pub path: Option<PathBuf>,
    /// Short label shown in tabs (`main.rs`, `Untitled-1`).
    pub title: String,
    pub is_dirty: bool,
}

/// A directory entry for the explorer tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntryInfo {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// True when the entry is excluded by `.gitignore` (shown dimmed, like VS Code).
    pub is_ignored: bool,
}

/// Change detected on disk for an open buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalChange {
    /// The buffer was clean and has been reloaded with the new disk content.
    Reloaded,
    /// The file changed on disk but the buffer has unsaved edits; nothing was overwritten.
    ConflictWithUnsavedChanges,
    /// The file was deleted on disk.
    DeletedOnDisk,
}

/// Receives document lifecycle events so a frontend can mirror them into language servers.
/// Calls happen synchronously on the thread that dispatched the action and must not block.
pub trait DocumentObserver: Send + Sync {
    fn did_open(&self, path: &Path, language: SyntaxLanguage, version: i32, text: &str);
    fn did_change(&self, path: &Path, language: SyntaxLanguage, version: i32, text: &str);
    fn did_save(&self, path: &Path, language: SyntaxLanguage, text: &str);
    fn did_close(&self, path: &Path, language: SyntaxLanguage);
}

/// Returns the LSP `languageId` for a syntax language.
pub fn lsp_language_id(language: SyntaxLanguage) -> &'static str {
    match language {
        SyntaxLanguage::Rust => "rust",
        SyntaxLanguage::TypeScript => "typescript",
        SyntaxLanguage::Tsx => "typescriptreact",
        SyntaxLanguage::JavaScript => "javascript",
        SyntaxLanguage::Python => "python",
        SyntaxLanguage::Json => "json",
        SyntaxLanguage::Markdown => "markdown",
        SyntaxLanguage::Kotlin => "kotlin",
        SyntaxLanguage::Swift => "swift",
        SyntaxLanguage::PlainText => "plaintext",
    }
}

/// Returns the line comment prefix for a language, if it has one.
pub fn line_comment_prefix(language: SyntaxLanguage) -> Option<&'static str> {
    match language {
        SyntaxLanguage::Rust
        | SyntaxLanguage::TypeScript
        | SyntaxLanguage::Tsx
        | SyntaxLanguage::JavaScript
        | SyntaxLanguage::Kotlin
        | SyntaxLanguage::Swift
        | SyntaxLanguage::Json => Some("//"),
        SyntaxLanguage::Python => Some("#"),
        SyntaxLanguage::Markdown | SyntaxLanguage::PlainText => None,
    }
}

/// Detects the indentation unit of a text: a tab if most indented lines start with a tab,
/// otherwise the most common space step (2 or 4), defaulting to 4 spaces.
pub fn detect_indent_unit(text: &str) -> String {
    let mut tabs = 0usize;
    let mut two = 0usize;
    let mut four = 0usize;
    for line in text.lines().take(2000) {
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let spaces = line.chars().take_while(|c| *c == ' ').count();
        if spaces == 0 || line.trim().is_empty() {
            continue;
        }
        if spaces % 4 == 0 {
            four += 1;
        } else if spaces % 2 == 0 {
            two += 1;
        }
    }
    if tabs > two + four {
        "\t".to_string()
    } else if two > four {
        "  ".to_string()
    } else {
        "    ".to_string()
    }
}

/// Normalizes `.` and `..` components lexically (without touching the filesystem).
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn absolutize(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    normalize_lexically(&abs)
}

fn file_mtime(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Reads a text file for editing, rejecting directories, binary and huge files.
fn read_text_file(path: &Path) -> Result<String, WorkspaceError> {
    let display = path.to_string_lossy().into_owned();
    let meta = fs::metadata(path).map_err(|_| WorkspaceError::FileNotFound(display.clone()))?;
    if meta.is_dir() {
        return Err(WorkspaceError::IsDirectory(display));
    }
    if meta.len() > MAX_EDITABLE_FILE_BYTES {
        return Err(WorkspaceError::FileTooLarge {
            path: display,
            size: meta.len(),
        });
    }
    let bytes = fs::read(path)?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(WorkspaceError::BinaryFile(display));
    }
    String::from_utf8(bytes).map_err(|_| WorkspaceError::BinaryFile(display))
}

/// Internal per-document session holding buffer, incremental syntax tree, and viewport window.
struct DocumentSession {
    buffer: TextBuffer,
    syntax: SyntaxEngine,
    untitled_index: u32,
    first_visible_line: u32,
    visible_line_count: u32,
    syntax_diagnostics: Vec<DiagnosticItem>,
    completions: Vec<String>,
    indent_unit: String,
    disk_stamp: Option<(SystemTime, u64)>,
    git_markers: Option<(i32, Vec<LineDiffStatus>)>,
}

impl DocumentSession {
    fn new(
        id: u64,
        path: Option<PathBuf>,
        content: &str,
        untitled_index: u32,
    ) -> Result<Self, WorkspaceError> {
        let mut buffer = TextBuffer::new(id, content);
        let language = path
            .as_ref()
            .map(SyntaxLanguage::from_path)
            .unwrap_or(SyntaxLanguage::PlainText);
        let disk_stamp = path.as_deref().and_then(file_mtime);
        if let Some(p) = path {
            buffer.set_file_path(p);
        }
        let mut syntax = SyntaxEngine::new(language)?;
        syntax.parse_full(&buffer)?;
        let mut session = Self {
            buffer,
            syntax,
            untitled_index,
            first_visible_line: 0,
            visible_line_count: 40,
            syntax_diagnostics: Vec::new(),
            completions: Vec::new(),
            indent_unit: detect_indent_unit(content),
            disk_stamp,
            git_markers: None,
        };
        session.refresh_syntax_diagnostics();
        Ok(session)
    }

    fn language(&self) -> SyntaxLanguage {
        self.syntax.language()
    }

    fn title(&self) -> String {
        match self.buffer.file_path() {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
            None => format!("Untitled-{}", self.untitled_index),
        }
    }

    fn refresh_syntax_diagnostics(&mut self) {
        self.syntax_diagnostics.clear();
        if let Some(tree) = self.syntax.tree() {
            let root = tree.root_node();
            if root.has_error() {
                collect_syntax_error_nodes(root, &self.buffer, &mut self.syntax_diagnostics);
            }
        }
    }

    fn apply_edits(&mut self, edits: &[BufferEdit]) -> Result<(), WorkspaceError> {
        if !edits.is_empty() {
            self.syntax.apply_edits(&self.buffer, edits)?;
            self.refresh_syntax_diagnostics();
        }
        Ok(())
    }

    /// Word-based completions for the identifier prefix under the primary cursor
    /// (used when no language server provides completions).
    fn compute_completions(&mut self) {
        let prefix = self.word_prefix_at_cursor();
        let text = self.buffer.text();
        let mut items: BTreeSet<String> = BTreeSet::new();
        for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word.chars().count() >= 2
                && word != prefix
                && word.to_lowercase().starts_with(&prefix.to_lowercase())
            {
                items.insert(word.to_string());
            }
        }
        self.completions = items.into_iter().take(50).collect();
    }

    fn word_prefix_at_cursor(&self) -> String {
        let Some(c) = self.buffer.cursors().first() else {
            return String::new();
        };
        let (line, col) = self.buffer.char_to_line_col(c.head);
        let text = self.buffer.line_text(line).unwrap_or_default();
        let before: Vec<char> = text.chars().take(col as usize).collect();
        let start = before
            .iter()
            .rposition(|ch| !(ch.is_alphanumeric() || *ch == '_'))
            .map(|i| i + 1)
            .unwrap_or(0);
        before[start..].iter().collect()
    }

    /// Scrolls so the primary cursor stays visible with a small margin.
    fn reveal_primary_cursor(&mut self) {
        let Some(c) = self.buffer.cursors().first() else {
            return;
        };
        let (line, _) = self.buffer.char_to_line_col(c.head);
        let visible = self.visible_line_count.max(1);
        let margin = if visible > 8 { 2 } else { 0 };
        if line < self.first_visible_line + margin {
            self.first_visible_line = line.saturating_sub(margin);
        } else if line + margin >= self.first_visible_line + visible {
            self.first_visible_line = (line + margin + 1).saturating_sub(visible);
        }
        let max_first = self.buffer.len_lines().saturating_sub(1);
        self.first_visible_line = self.first_visible_line.min(max_first);
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
        let (end_line, end_col) = buffer.char_to_line_col(end_char);
        let end_col = if end_line == line {
            end_col.max(start_col + 1)
        } else {
            buffer.line_len_chars(line) as u32
        };
        out.push(DiagnosticItem {
            line,
            start_col,
            end_col: end_col.max(start_col + 1),
            severity: DiagnosticSeverityLevel::Error,
            message: if node.is_missing() {
                format!("Syntax error: missing `{}`", node.kind())
            } else {
                "Syntax error".to_string()
            },
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

/// Central workspace state coordinating buffers, incremental syntax, Git and AI Agents.
pub struct IdeWorkspace {
    root_dir: RwLock<PathBuf>,
    next_buffer_id: AtomicU64,
    next_untitled: AtomicU64,
    active_buffer_id: AtomicU64,
    documents: RwLock<BTreeMap<u64, DocumentSession>>,
    git_repo: RwLock<Option<GitRepository>>,
    agent_manager: AgentManager,
    agent_task_buffers: RwLock<HashMap<u64, u64>>,
    observer: RwLock<Option<Arc<dyn DocumentObserver>>>,
    external_diagnostics: RwLock<HashMap<PathBuf, Vec<ExternalDiagnostic>>>,
}

impl Default for IdeWorkspace {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl IdeWorkspace {
    /// Creates a new `IdeWorkspace` rooted at `root_dir` with one empty untitled buffer.
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        let root_path = absolutize(root_dir.as_ref());
        let git_repo = GitRepository::open(&root_path).ok();

        let ws = Self {
            root_dir: RwLock::new(root_path),
            next_buffer_id: AtomicU64::new(1),
            next_untitled: AtomicU64::new(1),
            active_buffer_id: AtomicU64::new(0),
            documents: RwLock::new(BTreeMap::new()),
            git_repo: RwLock::new(git_repo),
            agent_manager: AgentManager::new(),
            agent_task_buffers: RwLock::new(HashMap::new()),
            observer: RwLock::new(None),
            external_diagnostics: RwLock::new(HashMap::new()),
        };
        let _ = ws.new_untitled_buffer();
        ws
    }

    pub fn root_dir(&self) -> PathBuf {
        self.root_dir.read().clone()
    }

    pub fn agent_manager(&self) -> &AgentManager {
        &self.agent_manager
    }

    /// Registers the observer that mirrors document lifecycle events (e.g. into LSP).
    /// Already open file buffers are announced immediately.
    pub fn set_document_observer(&self, observer: Option<Arc<dyn DocumentObserver>>) {
        if let Some(obs) = observer.as_ref() {
            for doc in self.documents.read().values() {
                if let Some(path) = doc.buffer.file_path() {
                    obs.did_open(path, doc.language(), doc.buffer.version(), &doc.buffer.text());
                }
            }
        }
        *self.observer.write() = observer;
    }

    fn observer(&self) -> Option<Arc<dyn DocumentObserver>> {
        self.observer.read().clone()
    }

    /// Replaces the external (language server) diagnostics for `path`.
    pub fn set_external_diagnostics(&self, path: &Path, items: Vec<ExternalDiagnostic>) {
        let key = absolutize(path);
        let mut map = self.external_diagnostics.write();
        if items.is_empty() {
            map.remove(&key);
        } else {
            map.insert(key, items);
        }
    }

    /// Returns all external diagnostics grouped by file (for a Problems panel).
    pub fn all_external_diagnostics(&self) -> Vec<(PathBuf, Vec<ExternalDiagnostic>)> {
        let mut v: Vec<_> = self
            .external_diagnostics
            .read()
            .iter()
            .map(|(p, d)| (p.clone(), d.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Switches the workspace to another folder: closes every buffer (callers must have saved
    /// or confirmed discarding unsaved changes), reopens Git and starts with an untitled buffer.
    pub fn open_folder(&self, folder: impl AsRef<Path>) -> Result<ViewportViewModel, WorkspaceError> {
        let folder = absolutize(folder.as_ref());
        if !folder.is_dir() {
            return Err(WorkspaceError::FileNotFound(folder.to_string_lossy().into_owned()));
        }
        let closed: Vec<(PathBuf, SyntaxLanguage)> = {
            let mut docs = self.documents.write();
            let list = docs
                .values()
                .filter_map(|d| d.buffer.file_path().map(|p| (p.to_path_buf(), d.language())))
                .collect();
            docs.clear();
            list
        };
        if let Some(obs) = self.observer() {
            for (p, lang) in &closed {
                obs.did_close(p, *lang);
            }
        }
        *self.root_dir.write() = folder.clone();
        *self.git_repo.write() = GitRepository::open(&folder).ok();
        self.external_diagnostics.write().clear();
        self.next_untitled.store(1, Ordering::SeqCst);
        self.new_untitled_buffer()?;
        self.active_viewport()
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

    /// Resolves a user supplied path (absolute, or relative to the workspace root).
    pub fn resolve_path(&self, path: impl AsRef<Path>) -> PathBuf {
        let p = path.as_ref();
        if p.is_absolute() {
            normalize_lexically(p)
        } else {
            normalize_lexically(&self.root_dir.read().join(p))
        }
    }

    /// Returns `path` relative to the workspace root when it is inside it.
    pub fn relative_path(&self, path: impl AsRef<Path>) -> String {
        let root = self.root_dir.read().clone();
        let p = path.as_ref();
        p.strip_prefix(&root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.to_string_lossy().into_owned())
    }

    fn ensure_inside_root(&self, path: &Path) -> Result<(), WorkspaceError> {
        let root = self.root_dir.read().clone();
        if path.starts_with(&root) && path != root {
            Ok(())
        } else {
            Err(WorkspaceError::OutsideWorkspace(
                path.to_string_lossy().into_owned(),
            ))
        }
    }

    /// Creates an empty untitled buffer and makes it active.
    pub fn new_untitled_buffer(&self) -> Result<u64, WorkspaceError> {
        let id = self.next_buffer_id.fetch_add(1, Ordering::SeqCst);
        let n = self.next_untitled.fetch_add(1, Ordering::SeqCst) as u32;
        let session = DocumentSession::new(id, None, "", n)?;
        self.documents.write().insert(id, session);
        self.active_buffer_id.store(id, Ordering::SeqCst);
        Ok(id)
    }

    /// Creates an in-memory buffer with optional file path and returns its `buffer_id`.
    /// Relative paths are resolved against the workspace root. Nothing is written to disk.
    pub fn create_buffer(
        &self,
        path: Option<&str>,
        initial_text: &str,
    ) -> Result<u64, WorkspaceError> {
        let id = self.next_buffer_id.fetch_add(1, Ordering::SeqCst);
        let path_buf = path.map(|p| self.resolve_path(p));
        let untitled = if path_buf.is_none() {
            self.next_untitled.fetch_add(1, Ordering::SeqCst) as u32
        } else {
            0
        };
        let session = DocumentSession::new(id, path_buf, initial_text, untitled)?;
        self.documents.write().insert(id, session);
        self.active_buffer_id.store(id, Ordering::SeqCst);
        Ok(id)
    }

    /// Closes a buffer. Closing the last buffer leaves a fresh untitled buffer open.
    /// The neighbouring tab becomes active, like in VS Code.
    pub fn close_buffer(&self, buffer_id: u64) -> Result<ViewportViewModel, WorkspaceError> {
        let closed = {
            let mut docs = self.documents.write();
            let ids: Vec<u64> = docs.keys().copied().collect();
            let Some(pos) = ids.iter().position(|&i| i == buffer_id) else {
                return Err(WorkspaceError::BufferNotFound(buffer_id));
            };
            let doc = docs.remove(&buffer_id);
            if self.active_buffer_id() == buffer_id {
                let next = if pos > 0 {
                    ids.get(pos - 1)
                } else {
                    ids.get(pos + 1)
                };
                if let Some(&n) = next {
                    self.active_buffer_id.store(n, Ordering::SeqCst);
                }
            }
            doc
        };
        if let (Some(doc), Some(obs)) = (closed.as_ref(), self.observer()) {
            if let Some(p) = doc.buffer.file_path() {
                obs.did_close(p, doc.language());
            }
        }
        if self.documents.read().is_empty() {
            self.new_untitled_buffer()?;
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
        self.with_doc(buffer_id, |doc| {
            doc.buffer.add_cursor(line, column);
            Ok(())
        })?;
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

    /// Replaces every match of `query` in the buffer with `replacement` as one undo step.
    /// Returns the number of replacements.
    pub fn replace_all_in_buffer(
        &self,
        buffer_id: u64,
        query: &str,
        replacement: &str,
        case_sensitive: bool,
    ) -> Result<usize, WorkspaceError> {
        let count = self.with_doc(buffer_id, |doc| {
            let matches = doc.buffer.search(query, case_sensitive);
            if matches.is_empty() {
                return Ok(0);
            }
            doc.buffer.begin_transaction();
            let mut edits = Vec::new();
            for m in matches.iter().rev() {
                edits.push(doc.buffer.replace_range(m.start_char, m.end_char, replacement));
            }
            doc.buffer.commit_transaction();
            doc.syntax.parse_full(&doc.buffer)?;
            doc.refresh_syntax_diagnostics();
            Ok(matches.len())
        })?;
        if count > 0 {
            self.notify_change(buffer_id);
        }
        Ok(count)
    }

    /// Returns the detected `SyntaxLanguage` of `buffer_id`.
    pub fn buffer_language(&self, buffer_id: u64) -> Option<SyntaxLanguage> {
        self.documents.read().get(&buffer_id).map(|d| d.language())
    }

    /// Returns the absolute file path of a buffer (`None` for untitled buffers).
    pub fn buffer_path(&self, buffer_id: u64) -> Option<PathBuf> {
        self.documents
            .read()
            .get(&buffer_id)
            .and_then(|d| d.buffer.file_path().map(Path::to_path_buf))
    }

    /// Returns the buffer id that has `path` open, if any.
    pub fn buffer_for_path(&self, path: impl AsRef<Path>) -> Option<u64> {
        let target = self.resolve_path(path);
        self.documents
            .read()
            .iter()
            .find(|(_, d)| d.buffer.file_path() == Some(target.as_path()))
            .map(|(&id, _)| id)
    }

    /// Returns the list of open buffer `(buffer_id, file_path, is_dirty)`.
    pub fn open_buffers(&self) -> Vec<(u64, String, bool)> {
        self.open_buffer_infos()
            .into_iter()
            .map(|b| {
                let path = b
                    .path
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| b.title.clone());
                (b.buffer_id, path, b.is_dirty)
            })
            .collect()
    }

    /// Returns rich tab information for every open buffer, in tab order.
    pub fn open_buffer_infos(&self) -> Vec<OpenBufferInfo> {
        self.documents
            .read()
            .iter()
            .map(|(&id, doc)| OpenBufferInfo {
                buffer_id: id,
                path: doc.buffer.file_path().map(Path::to_path_buf),
                title: doc.title(),
                is_dirty: doc.buffer.is_dirty(),
            })
            .collect()
    }

    /// Returns true when any open buffer has unsaved changes.
    pub fn has_unsaved_changes(&self) -> bool {
        self.documents.read().values().any(|d| d.buffer.is_dirty())
    }

    /// Returns the full text of `buffer_id`.
    pub fn buffer_text(&self, buffer_id: u64) -> Result<String, WorkspaceError> {
        let docs = self.documents.read();
        let doc = docs
            .get(&buffer_id)
            .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
        Ok(doc.buffer.text())
    }

    /// Returns the document version (incremented on every edit).
    pub fn buffer_version(&self, buffer_id: u64) -> Option<i32> {
        self.documents
            .read()
            .get(&buffer_id)
            .map(|d| d.buffer.version())
    }

    /// Text to put on the clipboard for Copy/Cut: the selections, or the whole current line
    /// (with trailing newline) when nothing is selected. Returns `(text, is_whole_line)`.
    pub fn clipboard_text(&self, buffer_id: u64) -> Result<(String, bool), WorkspaceError> {
        let docs = self.documents.read();
        let doc = docs
            .get(&buffer_id)
            .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
        if let Some(sel) = doc.buffer.selected_text() {
            return Ok((sel, false));
        }
        let mut lines = Vec::new();
        for l in doc.buffer.cursor_line_set() {
            lines.push(doc.buffer.line_text(l).unwrap_or_default());
        }
        Ok((format!("{}\n", lines.join("\n")), true))
    }

    /// Returns the primary cursor position as (line, char column).
    pub fn primary_cursor(&self, buffer_id: u64) -> Option<(u32, u32)> {
        let docs = self.documents.read();
        let doc = docs.get(&buffer_id)?;
        let c = doc.buffer.cursors().first()?;
        Some(doc.buffer.char_to_line_col(c.head))
    }

    /// Returns the primary cursor in LSP coordinates (line, UTF-16 column).
    pub fn primary_cursor_utf16(&self, buffer_id: u64) -> Option<(u32, u32)> {
        let docs = self.documents.read();
        let doc = docs.get(&buffer_id)?;
        let c = doc.buffer.cursors().first()?;
        Some(doc.buffer.char_to_utf16_line_col(c.head))
    }

    /// Converts an LSP position (line, UTF-16 column) of a buffer to (line, char column).
    pub fn utf16_to_char_col(&self, buffer_id: u64, line: u32, utf16_col: u32) -> Option<(u32, u32)> {
        let docs = self.documents.read();
        let doc = docs.get(&buffer_id)?;
        let ch = doc.buffer.utf16_line_col_to_char(line, utf16_col);
        Some(doc.buffer.char_to_line_col(ch))
    }

    /// Converts a buffer (line, char column) to an LSP (line, UTF-16 column) position.
    pub fn char_to_utf16_col(&self, buffer_id: u64, line: u32, col: u32) -> Option<(u32, u32)> {
        let docs = self.documents.read();
        let doc = docs.get(&buffer_id)?;
        let ch = doc.buffer.line_col_to_char(line, col);
        Some(doc.buffer.char_to_utf16_line_col(ch))
    }

    /// Returns the identifier prefix immediately before the primary cursor.
    pub fn word_prefix_at_cursor(&self, buffer_id: u64) -> String {
        self.documents
            .read()
            .get(&buffer_id)
            .map(|d| d.word_prefix_at_cursor())
            .unwrap_or_default()
    }

    /// Replaces the range `(start_line, start_col)..(end_line, end_col)` (char columns) with
    /// `text` as one undo step and places the cursor after the inserted text. Used to apply
    /// completions and code actions.
    pub fn replace_range(
        &self,
        buffer_id: u64,
        start: (u32, u32),
        end: (u32, u32),
        text: &str,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        self.with_doc(buffer_id, |doc| {
            let s = doc.buffer.line_col_to_char(start.0, start.1);
            let e = doc.buffer.line_col_to_char(end.0, end.1).max(s);
            let edit = doc.buffer.replace_range(s, e, text);
            doc.apply_edits(&[edit])?;
            doc.reveal_primary_cursor();
            Ok(())
        })?;
        self.notify_change(buffer_id);
        self.viewport_view_model(buffer_id)
    }

    /// Returns the latest word-based completion items computed for `buffer_id`.
    pub fn completions(&self, buffer_id: u64) -> Vec<String> {
        self.documents
            .read()
            .get(&buffer_id)
            .map(|d| d.completions.clone())
            .unwrap_or_default()
    }

    /// Returns a handle to the active Git repository if the workspace is inside one.
    pub fn git_repository(&self) -> Option<GitRepository> {
        self.git_repo.read().clone()
    }

    /// Returns the current Git branch name if the workspace is inside a Git repository.
    pub fn current_git_branch(&self) -> Option<String> {
        self.git_repo
            .read()
            .as_ref()
            .and_then(|r| r.current_branch().ok())
    }

    /// Re-opens the Git repository (after commits, checkouts, or external git commands)
    /// and drops cached gutter markers.
    pub fn refresh_git(&self) {
        let root = self.root_dir.read().clone();
        *self.git_repo.write() = GitRepository::open(&root).ok();
        for doc in self.documents.write().values_mut() {
            doc.git_markers = None;
        }
    }

    /// Returns the list of modified/added/deleted files in the Git working tree.
    pub fn changed_git_files(&self) -> Vec<GitFileStatus> {
        self.git_repo
            .read()
            .as_ref()
            .and_then(|r| r.changed_files().ok())
            .unwrap_or_default()
    }

    /// Lists workspace files relative to `root_dir` (gitignore-aware) for quick open.
    pub fn list_workspace_files(&self) -> Vec<String> {
        let root = self.root_dir.read().clone();
        let mut files = Vec::new();
        let walker = ignore::WalkBuilder::new(&root)
            .hidden(true)
            .git_ignore(true)
            .git_global(false)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                if let Ok(rel) = entry.path().strip_prefix(&root) {
                    files.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
            if files.len() >= 200_000 {
                break;
            }
        }
        files.sort();
        files
    }

    /// Lists one directory level for the explorer tree: folders first, then files, both sorted
    /// case-insensitively. `.git` is hidden; gitignored entries are flagged `is_ignored`.
    pub fn list_directory(&self, dir: impl AsRef<Path>) -> Result<Vec<DirEntryInfo>, WorkspaceError> {
        let dir = self.resolve_path(dir);
        let mut visible: BTreeSet<PathBuf> = BTreeSet::new();
        let walker = ignore::WalkBuilder::new(&dir)
            .max_depth(Some(1))
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .parents(true)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            if entry.depth() == 1 {
                visible.insert(entry.path().to_path_buf());
            }
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                || (entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) && path.is_dir());
            out.push(DirEntryInfo {
                is_ignored: !visible.contains(&path),
                name,
                path,
                is_dir,
            });
        }
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(out)
    }

    /// Creates a new empty file (parents are created) inside the workspace and opens it.
    pub fn create_file(&self, path: impl AsRef<Path>) -> Result<ViewportViewModel, WorkspaceError> {
        let abs = self.resolve_path(path);
        self.ensure_inside_root(&abs)?;
        if abs.exists() {
            return Err(WorkspaceError::AlreadyExists(abs.to_string_lossy().into_owned()));
        }
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, "")?;
        self.dispatch(EditorAction::OpenFile {
            path: abs.to_string_lossy().into_owned(),
        })
    }

    /// Creates a new folder (and parents) inside the workspace.
    pub fn create_dir(&self, path: impl AsRef<Path>) -> Result<PathBuf, WorkspaceError> {
        let abs = self.resolve_path(path);
        self.ensure_inside_root(&abs)?;
        if abs.exists() {
            return Err(WorkspaceError::AlreadyExists(abs.to_string_lossy().into_owned()));
        }
        fs::create_dir_all(&abs)?;
        Ok(abs)
    }

    /// Renames/moves a file or folder inside the workspace and retargets open buffers.
    pub fn rename_path(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
    ) -> Result<PathBuf, WorkspaceError> {
        let from = self.resolve_path(from);
        let to = self.resolve_path(to);
        self.ensure_inside_root(&from)?;
        self.ensure_inside_root(&to)?;
        if !from.exists() {
            return Err(WorkspaceError::FileNotFound(from.to_string_lossy().into_owned()));
        }
        if to.exists() {
            return Err(WorkspaceError::AlreadyExists(to.to_string_lossy().into_owned()));
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&from, &to)?;
        let obs = self.observer();
        let mut docs = self.documents.write();
        for doc in docs.values_mut() {
            let Some(old) = doc.buffer.file_path().map(Path::to_path_buf) else {
                continue;
            };
            if let Ok(rest) = old.strip_prefix(&from) {
                let new_path = if rest.as_os_str().is_empty() {
                    to.clone()
                } else {
                    to.join(rest)
                };
                if let Some(o) = obs.as_ref() {
                    o.did_close(&old, doc.language());
                }
                doc.buffer.set_file_path(new_path.clone());
                let lang = SyntaxLanguage::from_path(&new_path);
                if lang != doc.language() {
                    if let Ok(mut engine) = SyntaxEngine::new(lang) {
                        if engine.parse_full(&doc.buffer).is_ok() {
                            doc.syntax = engine;
                            doc.refresh_syntax_diagnostics();
                        }
                    }
                }
                doc.disk_stamp = file_mtime(&new_path);
                doc.git_markers = None;
                if let Some(o) = obs.as_ref() {
                    o.did_open(&new_path, doc.language(), doc.buffer.version(), &doc.buffer.text());
                }
            }
        }
        Ok(to)
    }

    /// Deletes a file or folder (recursively) inside the workspace and closes its buffers,
    /// discarding unsaved edits in them (callers confirm with the user first).
    pub fn delete_path(&self, path: impl AsRef<Path>) -> Result<(), WorkspaceError> {
        let abs = self.resolve_path(path);
        self.ensure_inside_root(&abs)?;
        let meta = fs::symlink_metadata(&abs)
            .map_err(|_| WorkspaceError::FileNotFound(abs.to_string_lossy().into_owned()))?;
        if meta.is_dir() {
            fs::remove_dir_all(&abs)?;
        } else {
            fs::remove_file(&abs)?;
        }
        let to_close: Vec<u64> = self
            .documents
            .read()
            .iter()
            .filter(|(_, d)| d.buffer.file_path().is_some_and(|p| p.starts_with(&abs)))
            .map(|(&id, _)| id)
            .collect();
        for id in to_close {
            self.close_buffer(id)?;
        }
        Ok(())
    }

    /// Saves an untitled (or any) buffer under a new path and re-detects its language.
    pub fn save_buffer_as(
        &self,
        buffer_id: u64,
        path: impl AsRef<Path>,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        let abs = self.resolve_path(path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        let (old_path, lang, text) = self.with_doc(buffer_id, |doc| {
            let old = doc.buffer.file_path().map(Path::to_path_buf);
            let old_lang = doc.language();
            doc.buffer.save_as(&abs)?;
            let lang = SyntaxLanguage::from_path(&abs);
            if lang != doc.language() {
                let mut engine = SyntaxEngine::new(lang)?;
                engine.parse_full(&doc.buffer)?;
                doc.syntax = engine;
                doc.refresh_syntax_diagnostics();
            }
            doc.disk_stamp = file_mtime(&abs);
            doc.git_markers = None;
            Ok((old.map(|p| (p, old_lang)), doc.language(), doc.buffer.text()))
        })?;
        if let Some(obs) = self.observer() {
            if let Some((p, l)) = old_path {
                obs.did_close(&p, l);
            }
            let version = self.buffer_version(buffer_id).unwrap_or(1);
            obs.did_open(&abs, lang, version, &text);
            obs.did_save(&abs, lang, &text);
        }
        self.viewport_view_model(buffer_id)
    }

    /// Checks open file buffers against the disk. Clean buffers whose file changed are
    /// reloaded; dirty ones are reported as conflicts; deleted files are reported.
    pub fn poll_external_changes(&self) -> Vec<(u64, ExternalChange)> {
        let mut changes = Vec::new();
        let ids: Vec<_> = self
            .documents
            .read()
            .iter()
            .filter_map(|(&id, d)| {
                d.buffer
                    .file_path()
                    .map(|p| (id, p.to_path_buf(), d.disk_stamp, d.buffer.is_dirty()))
            })
            .collect();
        for (id, path, stamp, dirty) in ids {
            let current = file_mtime(&path);
            if current == stamp {
                continue;
            }
            match current {
                None => {
                    if stamp.is_some() {
                        changes.push((id, ExternalChange::DeletedOnDisk));
                    }
                    let _ = self.with_doc(id, |doc| {
                        doc.disk_stamp = None;
                        Ok(())
                    });
                }
                Some(new_stamp) => {
                    if dirty {
                        changes.push((id, ExternalChange::ConflictWithUnsavedChanges));
                        let _ = self.with_doc(id, |doc| {
                            doc.disk_stamp = Some(new_stamp);
                            Ok(())
                        });
                        continue;
                    }
                    let Ok(text) = read_text_file(&path) else {
                        continue;
                    };
                    let reloaded = self.with_doc(id, |doc| {
                        doc.disk_stamp = Some(new_stamp);
                        if doc.buffer.text() == text {
                            return Ok(false);
                        }
                        let edits = doc.buffer.replace_all_text(&text);
                        doc.buffer.mark_clean();
                        doc.apply_edits(&edits)?;
                        doc.git_markers = None;
                        Ok(true)
                    });
                    if reloaded.unwrap_or(false) {
                        self.notify_change(id);
                        changes.push((id, ExternalChange::Reloaded));
                    }
                }
            }
        }
        changes
    }

    /// Records which buffer an agent task targets (so Accept applies to the right file).
    pub fn bind_agent_task(&self, task_id: u64, buffer_id: u64) {
        self.agent_task_buffers.write().insert(task_id, buffer_id);
    }

    /// Returns the buffer an agent task was created for.
    pub fn agent_task_buffer(&self, task_id: u64) -> Option<u64> {
        self.agent_task_buffers.read().get(&task_id).copied()
    }

    /// Applies an AI Agent task's patch to the buffer it was created for (falls back to
    /// `buffer_id`) and updates its syntax tree.
    pub fn accept_agent_task(
        &self,
        task_id: u64,
        buffer_id: u64,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        let target = self.agent_task_buffer(task_id).unwrap_or(buffer_id);
        self.with_doc(target, |doc| {
            self.agent_manager.apply_task_patch(task_id, &mut doc.buffer)?;
            doc.syntax.parse_full(&doc.buffer)?;
            doc.refresh_syntax_diagnostics();
            doc.reveal_primary_cursor();
            Ok(())
        })?;
        self.agent_manager
            .verify_task(task_id, true, "Applied by the user")?;
        self.notify_change(target);
        self.active_buffer_id.store(target, Ordering::SeqCst);
        self.viewport_view_model(target)
    }

    /// Rejects an AI Agent task and reverts the buffer if the patch was previously applied.
    pub fn reject_agent_task(
        &self,
        task_id: u64,
        buffer_id: u64,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        let target = self.agent_task_buffer(task_id).unwrap_or(buffer_id);
        self.with_doc(target, |doc| {
            self.agent_manager
                .reject_task(task_id, Some(&mut doc.buffer))?;
            doc.syntax.parse_full(&doc.buffer)?;
            doc.refresh_syntax_diagnostics();
            Ok(())
        })?;
        self.notify_change(target);
        self.viewport_view_model(target)
    }

    /// Runs `f` with mutable access to a document session.
    fn with_doc<R>(
        &self,
        buffer_id: u64,
        f: impl FnOnce(&mut DocumentSession) -> Result<R, WorkspaceError>,
    ) -> Result<R, WorkspaceError> {
        let mut docs = self.documents.write();
        let doc = docs
            .get_mut(&buffer_id)
            .ok_or(WorkspaceError::BufferNotFound(buffer_id))?;
        f(doc)
    }

    /// Notifies the document observer that a buffer's content changed.
    fn notify_change(&self, buffer_id: u64) {
        let Some(obs) = self.observer() else {
            return;
        };
        let snapshot = {
            let docs = self.documents.read();
            docs.get(&buffer_id).and_then(|d| {
                d.buffer
                    .file_path()
                    .map(|p| (p.to_path_buf(), d.language(), d.buffer.version(), d.buffer.text()))
            })
        };
        if let Some((p, lang, version, text)) = snapshot {
            obs.did_change(&p, lang, version, &text);
        }
    }

    /// Applies a text-editing operation, updates syntax, scrolls to the cursor and notifies
    /// the observer.
    fn edit(
        &self,
        buffer_id: u64,
        op: impl FnOnce(&mut DocumentSession) -> Vec<BufferEdit>,
    ) -> Result<ViewportViewModel, WorkspaceError> {
        let changed = self.with_doc(buffer_id, |doc| {
            let edits = op(doc);
            doc.apply_edits(&edits)?;
            doc.reveal_primary_cursor();
            Ok(!edits.is_empty())
        })?;
        if changed {
            self.notify_change(buffer_id);
        }
        self.viewport_view_model(buffer_id)
    }

    /// Applies a cursor-only operation and scrolls to the primary cursor.
    fn navigate(
        &self,
        buffer_id: u64,
        op: impl FnOnce(&mut DocumentSession),
    ) -> Result<ViewportViewModel, WorkspaceError> {
        self.with_doc(buffer_id, |doc| {
            op(doc);
            doc.reveal_primary_cursor();
            Ok(())
        })?;
        self.viewport_view_model(buffer_id)
    }

    /// Opens a file (absolute or workspace-relative), or focuses it if already open.
    fn open_file(&self, path: &str) -> Result<ViewportViewModel, WorkspaceError> {
        let abs = self.resolve_path(path);
        if let Some(existing) = self.buffer_for_path(&abs) {
            self.active_buffer_id.store(existing, Ordering::SeqCst);
            return self.viewport_view_model(existing);
        }
        let content = read_text_file(&abs)?;
        let id = self.next_buffer_id.fetch_add(1, Ordering::SeqCst);
        let session = DocumentSession::new(id, Some(abs.clone()), &content, 0)?;
        let lang = session.language();
        let version = session.buffer.version();

        if self.git_repo.read().is_none() {
            if let Some(parent) = abs.parent() {
                if let Ok(repo) = GitRepository::open(parent) {
                    *self.git_repo.write() = Some(repo);
                }
            }
        }

        // Replace a pristine untitled buffer (like VS Code's initial empty editor).
        let replace_untitled = {
            let docs = self.documents.read();
            docs.len() == 1
                && docs.values().all(|d| {
                    d.buffer.file_path().is_none() && !d.buffer.is_dirty() && d.buffer.len_chars() == 0
                })
        };
        {
            let mut docs = self.documents.write();
            if replace_untitled {
                docs.clear();
            }
            docs.insert(id, session);
        }
        self.active_buffer_id.store(id, Ordering::SeqCst);
        if let Some(obs) = self.observer() {
            obs.did_open(&abs, lang, version, &content);
        }
        self.viewport_view_model(id)
    }

    /// Unidirectional Data Flow entrypoint: processes an `EditorAction` and returns the
    /// updated `ViewportViewModel`.
    pub fn dispatch(&self, action: EditorAction) -> Result<ViewportViewModel, WorkspaceError> {
        match action {
            EditorAction::OpenFile { path } => self.open_file(&path),
            EditorAction::SaveFile { buffer_id } => {
                let saved = self.with_doc(buffer_id, |doc| {
                    if doc.buffer.file_path().is_none() {
                        return Err(WorkspaceError::Untitled(buffer_id));
                    }
                    doc.buffer.save()?;
                    let path = doc.buffer.file_path().map(Path::to_path_buf);
                    doc.disk_stamp = path.as_deref().and_then(file_mtime);
                    Ok(path.map(|p| (p, doc.language(), doc.buffer.text())))
                })?;
                if let (Some((p, lang, text)), Some(obs)) = (saved, self.observer()) {
                    obs.did_save(&p, lang, &text);
                }
                self.viewport_view_model(buffer_id)
            }
            EditorAction::InsertText { buffer_id, text } => {
                self.edit(buffer_id, |doc| doc.buffer.insert_text(&text))
            }
            EditorAction::InsertNewline { buffer_id } => self.edit(buffer_id, |doc| {
                let unit = doc.indent_unit.clone();
                doc.buffer.insert_newline_with_indent(&unit)
            }),
            EditorAction::Indent { buffer_id } => self.edit(buffer_id, |doc| {
                let unit = doc.indent_unit.clone();
                let multi_line = doc.buffer.cursors().iter().any(|c| {
                    let (s, e) = c.selection_range();
                    doc.buffer.char_to_line_col(s).0 != doc.buffer.char_to_line_col(e).0
                });
                if multi_line {
                    doc.buffer.indent_lines(&unit)
                } else if unit == "\t" {
                    doc.buffer.insert_text("\t")
                } else {
                    // Insert spaces up to the next tab stop.
                    let width = unit.len();
                    let col = doc
                        .buffer
                        .cursors()
                        .first()
                        .map(|c| doc.buffer.char_to_line_col(c.selection_range().0).1 as usize)
                        .unwrap_or(0);
                    let n = width - (col % width);
                    doc.buffer.insert_text(&" ".repeat(n))
                }
            }),
            EditorAction::Outdent { buffer_id } => self.edit(buffer_id, |doc| {
                let width = if doc.indent_unit == "\t" {
                    4
                } else {
                    doc.indent_unit.len()
                };
                doc.buffer.outdent_lines(width)
            }),
            EditorAction::ToggleLineComment { buffer_id } => self.edit(buffer_id, |doc| {
                match line_comment_prefix(doc.language()) {
                    Some(prefix) => doc.buffer.toggle_line_comment(prefix),
                    None => Vec::new(),
                }
            }),
            EditorAction::DeleteBackward { buffer_id } => {
                self.edit(buffer_id, |doc| doc.buffer.delete_backward())
            }
            EditorAction::DeleteForward { buffer_id } => {
                self.edit(buffer_id, |doc| doc.buffer.delete_forward())
            }
            EditorAction::DeleteWordBackward { buffer_id } => self.edit(buffer_id, |doc| {
                if doc.buffer.cursors().iter().all(|c| !c.has_selection()) {
                    doc.buffer.move_cursors(CursorDirection::WordLeft, true);
                }
                doc.buffer.delete_backward()
            }),
            EditorAction::DeleteWordForward { buffer_id } => self.edit(buffer_id, |doc| {
                if doc.buffer.cursors().iter().all(|c| !c.has_selection()) {
                    doc.buffer.move_cursors(CursorDirection::WordRight, true);
                }
                doc.buffer.delete_backward()
            }),
            EditorAction::DeleteSelectionOrLine { buffer_id } => self.edit(buffer_id, |doc| {
                if doc.buffer.cursors().iter().all(|c| !c.has_selection()) {
                    doc.buffer.select_cursor_lines();
                }
                doc.buffer.delete_backward()
            }),
            EditorAction::MoveCursor {
                buffer_id,
                direction,
                select,
            } => self.navigate(buffer_id, |doc| doc.buffer.move_cursors(direction, select)),
            EditorAction::MovePage {
                buffer_id,
                pages,
                select,
            } => self.with_doc(buffer_id, |doc| {
                let page = doc.visible_line_count.saturating_sub(1).max(1) as i64;
                let delta = page * pages as i64;
                doc.buffer.move_cursors_by_lines(delta, select);
                let max_first = doc.buffer.len_lines().saturating_sub(1) as i64;
                doc.first_visible_line =
                    (doc.first_visible_line as i64 + delta).clamp(0, max_first) as u32;
                doc.reveal_primary_cursor();
                Ok(())
            })
            .and_then(|_| self.viewport_view_model(buffer_id)),
            EditorAction::SetCursorPosition {
                buffer_id,
                line,
                column,
                select,
            } => self.navigate(buffer_id, |doc| {
                doc.buffer.set_cursor_position(line, column, select)
            }),
            EditorAction::SelectAll { buffer_id } => {
                self.with_doc(buffer_id, |doc| {
                    doc.buffer.select_all();
                    Ok(())
                })?;
                self.viewport_view_model(buffer_id)
            }
            EditorAction::AddNextOccurrence { buffer_id } => {
                self.navigate(buffer_id, |doc| {
                    doc.buffer.add_next_occurrence();
                })
            }
            EditorAction::CollapseCursors { buffer_id } => self.navigate(buffer_id, |doc| {
                if let Some(c) = doc.buffer.cursors().first().copied() {
                    let (l, col) = doc.buffer.char_to_line_col(c.head);
                    doc.buffer.set_cursor_position(l, col, false);
                }
            }),
            EditorAction::ScrollViewport {
                buffer_id,
                first_visible_line,
                visible_line_count,
            } => {
                self.with_doc(buffer_id, |doc| {
                    let max_first = doc.buffer.len_lines().saturating_sub(1);
                    doc.first_visible_line = first_visible_line.min(max_first);
                    doc.visible_line_count = visible_line_count.max(1);
                    Ok(())
                })?;
                self.viewport_view_model(buffer_id)
            }
            EditorAction::Undo { buffer_id } => self.edit(buffer_id, |doc| doc.buffer.undo()),
            EditorAction::Redo { buffer_id } => self.edit(buffer_id, |doc| doc.buffer.redo()),
            EditorAction::TriggerLspCompletion { buffer_id } => {
                self.with_doc(buffer_id, |doc| {
                    doc.compute_completions();
                    Ok(())
                })?;
                self.viewport_view_model(buffer_id)
            }
            EditorAction::SubmitAgentPrompt { prompt } => {
                let target_id = self.active_buffer_id();
                let task_id = {
                    let docs = self.documents.read();
                    let doc = docs
                        .get(&target_id)
                        .ok_or(WorkspaceError::BufferNotFound(target_id))?;
                    self.agent_manager
                        .submit_prompt_for_buffer(&prompt, &doc.buffer)
                };
                self.bind_agent_task(task_id, target_id);
                self.viewport_view_model(target_id)
            }
        }
    }

    /// Builds the `ViewportViewModel` for `buffer_id`, slicing only `visible_lines`.
    pub fn viewport_view_model(&self, buffer_id: u64) -> Result<ViewportViewModel, WorkspaceError> {
        // Refresh cached git markers (cheap when the version is unchanged).
        self.refresh_git_markers(buffer_id);

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
            .unwrap_or_else(|| doc.title());

        // Language server diagnostics take precedence over raw tree-sitter error nodes.
        let external = doc
            .buffer
            .file_path()
            .and_then(|p| self.external_diagnostics.read().get(p).cloned());
        let diagnostics = match external {
            Some(items) => items
                .iter()
                .map(|d| {
                    let s = doc.buffer.utf16_line_col_to_char(d.start_line, d.start_utf16);
                    let (line, start_col) = doc.buffer.char_to_line_col(s);
                    let end_col = if d.end_line == d.start_line {
                        let e = doc.buffer.utf16_line_col_to_char(d.end_line, d.end_utf16);
                        doc.buffer.char_to_line_col(e).1
                    } else {
                        doc.buffer.line_len_chars(line) as u32
                    };
                    let message = match &d.source {
                        Some(src) => format!("{} ({src})", d.message),
                        None => d.message.clone(),
                    };
                    DiagnosticItem {
                        line,
                        start_col,
                        end_col: end_col.max(start_col + 1),
                        severity: d.severity,
                        message,
                    }
                })
                .collect(),
            None => doc.syntax_diagnostics.clone(),
        };

        let git_diff_markers = doc
            .git_markers
            .as_ref()
            .map(|(_, m)| m.clone())
            .unwrap_or_default();

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

    fn refresh_git_markers(&self, buffer_id: u64) {
        let needs = {
            let docs = self.documents.read();
            match docs.get(&buffer_id) {
                Some(d) if d.buffer.file_path().is_some() => d
                    .git_markers
                    .as_ref()
                    .is_none_or(|(v, _)| *v != d.buffer.version()),
                _ => false,
            }
        };
        if !needs {
            return;
        }
        let (path, text, version) = {
            let docs = self.documents.read();
            let Some(d) = docs.get(&buffer_id) else {
                return;
            };
            let Some(p) = d.buffer.file_path() else {
                return;
            };
            (p.to_path_buf(), d.buffer.text(), d.buffer.version())
        };
        let markers = self
            .git_repo
            .read()
            .as_ref()
            .and_then(|repo| repo.diff_buffer_lines(&path, &text).ok())
            .unwrap_or_default();
        if let Some(d) = self.documents.write().get_mut(&buffer_id) {
            d.git_markers = Some((version, markers));
        }
    }

    /// Returns the `ViewportViewModel` for the currently active buffer.
    pub fn active_viewport(&self) -> Result<ViewportViewModel, WorkspaceError> {
        self.viewport_view_model(self.active_buffer_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("levity-core-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SAMPLE: &str = "use std::collections::BTreeMap;\n\npub struct ProjectConfig {\n    pub name: String,\n    pub target_fps: u32,\n}\n\nfn main() {\n    let config = ProjectConfig {\n        name: String::from(\"IDE Levity\"),\n        target_fps: 120,\n    };\n    println!(\"Chào mừng {} ({}fps)\", config.name, config.target_fps);\n}\n";

    #[test]
    fn test_unidirectional_data_flow_all_editor_actions() {
        let ws = IdeWorkspace::new(".");
        let buf_id = ws.create_buffer(Some("welcome.rs"), SAMPLE).unwrap();

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

        let vm = ws.dispatch(EditorAction::Undo { buffer_id: buf_id }).unwrap();
        assert_eq!(vm.total_lines, initial_lines);
        assert!(!vm.is_dirty, "undo back to the original content is clean");
        let vm = ws.dispatch(EditorAction::Redo { buffer_id: buf_id }).unwrap();
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

        ws.dispatch(EditorAction::SetCursorPosition {
            buffer_id: buf_id,
            line: 9,
            column: 18,
            select: false,
        })
        .unwrap();
        ws.dispatch(EditorAction::InsertText {
            buffer_id: buf_id,
            text: "\n        Pro".to_string(),
        })
        .unwrap();
        ws.dispatch(EditorAction::TriggerLspCompletion { buffer_id: buf_id })
            .unwrap();
        assert!(ws.completions(buf_id).contains(&"ProjectConfig".to_string()));
    }

    #[test]
    fn test_viewport_virtualization_and_cursor_follow() {
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

        // Moving the cursor far below scrolls the viewport so it stays visible.
        let vm = ws
            .dispatch(EditorAction::SetCursorPosition {
                buffer_id: buf_id,
                line: 300,
                column: 0,
                select: false,
            })
            .unwrap();
        assert!(vm.first_visible_line <= 300 && 300 < vm.first_visible_line + 25);
        let vm = ws
            .dispatch(EditorAction::MovePage {
                buffer_id: buf_id,
                pages: -1,
                select: false,
            })
            .unwrap();
        assert_eq!(vm.cursors[0].line, 276);
        assert!(vm.first_visible_line <= 276 && 276 < vm.first_visible_line + 25);
        let vm = ws
            .dispatch(EditorAction::MoveCursor {
                buffer_id: buf_id,
                direction: CursorDirection::DocumentStart,
                select: false,
            })
            .unwrap();
        assert_eq!(vm.first_visible_line, 0);
    }

    #[test]
    fn test_multi_tab_search_and_multi_cursor_workspace_methods() {
        let ws = IdeWorkspace::new(".");
        let b1 = ws.create_buffer(Some("first.rs"), "fn a() {}\n").unwrap();
        let b2 = ws
            .create_buffer(Some("second.py"), "def hello():\n    return 42\n")
            .unwrap();
        assert_eq!(ws.open_buffers().len(), 3);
        assert_eq!(ws.buffer_language(b2), Some(SyntaxLanguage::Python));

        let matches = ws.search_buffer(b2, "hello", false).unwrap();
        assert_eq!(matches.len(), 1);

        let vm = ws.add_cursor(b2, 1, 4).unwrap();
        assert_eq!(vm.cursors.len(), 2);

        let vm_after_close = ws.close_buffer(b2).unwrap();
        assert_eq!(vm_after_close.buffer_id, b1);
        assert_eq!(ws.open_buffers().len(), 2);
        ws.close_buffer(b1).unwrap();
        let only = ws.open_buffer_infos();
        assert_eq!(only.len(), 1);
        ws.close_buffer(only[0].buffer_id).unwrap();
        let fresh = ws.open_buffer_infos();
        assert_eq!(fresh.len(), 1, "closing the last tab leaves an untitled buffer");
        assert!(fresh[0].path.is_none());
    }

    #[test]
    fn test_paths_are_absolute_and_saved_inside_workspace_root() {
        let root = temp_root("paths");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let ws = IdeWorkspace::new(&root);
        let vm = ws
            .dispatch(EditorAction::OpenFile {
                path: "src/main.rs".into(),
            })
            .unwrap();
        assert_eq!(PathBuf::from(&vm.file_path), root.join("src/main.rs"));
        assert_eq!(ws.open_buffer_infos().len(), 1, "pristine untitled tab is replaced");
        let id = vm.buffer_id;
        ws.dispatch(EditorAction::SetCursorPosition {
            buffer_id: id,
            line: 0,
            column: 11,
            select: false,
        })
        .unwrap();
        ws.dispatch(EditorAction::InsertText {
            buffer_id: id,
            text: " println!(\"hi\"); ".into(),
        })
        .unwrap();
        ws.dispatch(EditorAction::SaveFile { buffer_id: id }).unwrap();
        assert!(fs::read_to_string(root.join("src/main.rs"))
            .unwrap()
            .contains("println!"));
        assert_eq!(ws.relative_path(root.join("src/main.rs")), "src/main.rs");

        // Missing and binary files are refused instead of silently opening empty buffers.
        assert!(matches!(
            ws.dispatch(EditorAction::OpenFile { path: "nope.rs".into() }),
            Err(WorkspaceError::FileNotFound(_))
        ));
        fs::write(root.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        assert!(matches!(
            ws.dispatch(EditorAction::OpenFile { path: "blob.bin".into() }),
            Err(WorkspaceError::BinaryFile(_))
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_untitled_save_as_redetects_language() {
        let root = temp_root("saveas");
        let ws = IdeWorkspace::new(&root);
        let id = ws.active_buffer_id();
        assert_eq!(ws.buffer_language(id), Some(SyntaxLanguage::PlainText));
        ws.dispatch(EditorAction::InsertText {
            buffer_id: id,
            text: "def f():\n    return 1\n".into(),
        })
        .unwrap();
        assert!(matches!(
            ws.dispatch(EditorAction::SaveFile { buffer_id: id }),
            Err(WorkspaceError::Untitled(_))
        ));
        let vm = ws.save_buffer_as(id, "pkg/script.py").unwrap();
        assert!(!vm.is_dirty);
        assert_eq!(ws.buffer_language(id), Some(SyntaxLanguage::Python));
        assert!(root.join("pkg/script.py").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_explorer_listing_and_file_operations() {
        let root = temp_root("explorer");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        fs::write(root.join("A.md"), "a").unwrap();
        fs::write(root.join("debug.log"), "x").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
        let ws = IdeWorkspace::new(&root);

        let entries = ws.list_directory(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "target", ".gitignore", "A.md", "b.txt", "debug.log"]);
        let ignored: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_ignored)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(ignored, vec!["target", "debug.log"]);
        assert!(!ws.list_workspace_files().iter().any(|f| f.starts_with("target")));

        let vm = ws.create_file("src/new_mod.rs").unwrap();
        assert!(root.join("src/new_mod.rs").exists());
        let id = vm.buffer_id;
        ws.create_dir("docs/guides").unwrap();
        assert!(root.join("docs/guides").is_dir());
        assert!(ws.create_file("../escape.rs").is_err());

        ws.rename_path("src", "core").unwrap();
        assert_eq!(ws.buffer_path(id), Some(root.join("core/new_mod.rs")));
        ws.rename_path("core/new_mod.rs", "core/renamed.py").unwrap();
        assert_eq!(ws.buffer_language(id), Some(SyntaxLanguage::Python));

        ws.delete_path("core").unwrap();
        assert!(!root.join("core").exists());
        assert!(ws.buffer_path(id).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_external_changes_reload_clean_buffers_and_flag_conflicts() {
        let root = temp_root("external");
        let file = root.join("notes.txt");
        fs::write(&file, "one\n").unwrap();
        let ws = IdeWorkspace::new(&root);
        let id = ws
            .dispatch(EditorAction::OpenFile {
                path: "notes.txt".into(),
            })
            .unwrap()
            .buffer_id;
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&file, "one\ntwo\n").unwrap();
        // Make sure the stamp differs even on coarse-mtime filesystems (size changed too).
        let changes = ws.poll_external_changes();
        assert_eq!(changes, vec![(id, ExternalChange::Reloaded)]);
        assert_eq!(ws.buffer_text(id).unwrap(), "one\ntwo\n");
        assert!(!ws.active_viewport().unwrap().is_dirty);

        ws.dispatch(EditorAction::InsertText {
            buffer_id: id,
            text: "local ".into(),
        })
        .unwrap();
        fs::write(&file, "remote rewrite\n").unwrap();
        let changes = ws.poll_external_changes();
        assert_eq!(changes, vec![(id, ExternalChange::ConflictWithUnsavedChanges)]);
        assert!(ws.buffer_text(id).unwrap().starts_with("local "));

        fs::remove_file(&file).unwrap();
        assert_eq!(ws.poll_external_changes(), vec![(id, ExternalChange::DeletedOnDisk)]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<String>>,
    }

    impl DocumentObserver for RecordingObserver {
        fn did_open(&self, path: &Path, language: SyntaxLanguage, version: i32, _text: &str) {
            self.events.lock().push(format!(
                "open {} {} v{version}",
                path.file_name().unwrap().to_string_lossy(),
                lsp_language_id(language)
            ));
        }
        fn did_change(&self, path: &Path, _language: SyntaxLanguage, version: i32, _text: &str) {
            self.events.lock().push(format!(
                "change {} v{version}",
                path.file_name().unwrap().to_string_lossy()
            ));
        }
        fn did_save(&self, path: &Path, _language: SyntaxLanguage, _text: &str) {
            self.events
                .lock()
                .push(format!("save {}", path.file_name().unwrap().to_string_lossy()));
        }
        fn did_close(&self, path: &Path, _language: SyntaxLanguage) {
            self.events
                .lock()
                .push(format!("close {}", path.file_name().unwrap().to_string_lossy()));
        }
    }

    #[test]
    fn test_document_observer_and_external_diagnostics() {
        let root = temp_root("observer");
        fs::write(root.join("main.rs"), "fn main() {\n    let é = 1;\n}\n").unwrap();
        let ws = IdeWorkspace::new(&root);
        let obs = Arc::new(RecordingObserver::default());
        ws.set_document_observer(Some(obs.clone()));
        let id = ws
            .dispatch(EditorAction::OpenFile {
                path: "main.rs".into(),
            })
            .unwrap()
            .buffer_id;
        ws.dispatch(EditorAction::InsertText {
            buffer_id: id,
            text: "// x\n".into(),
        })
        .unwrap();
        ws.dispatch(EditorAction::SaveFile { buffer_id: id }).unwrap();
        ws.close_buffer(id).unwrap();
        let events = obs.events.lock().clone();
        assert_eq!(
            events,
            vec!["open main.rs rust v1", "change main.rs v2", "save main.rs", "close main.rs"]
        );

        let id = ws
            .dispatch(EditorAction::OpenFile {
                path: "main.rs".into(),
            })
            .unwrap()
            .buffer_id;
        ws.set_external_diagnostics(
            &root.join("main.rs"),
            vec![ExternalDiagnostic {
                start_line: 2,
                start_utf16: 8,
                end_line: 2,
                end_utf16: 9,
                severity: DiagnosticSeverityLevel::Warning,
                message: "unused variable".into(),
                source: Some("rustc".into()),
            }],
        );
        let vm = ws.viewport_view_model(id).unwrap();
        assert_eq!(vm.diagnostics.len(), 1);
        assert_eq!(vm.diagnostics[0].line, 2);
        assert_eq!(vm.diagnostics[0].start_col, 8);
        assert_eq!(vm.diagnostics[0].severity, DiagnosticSeverityLevel::Warning);
        assert!(vm.diagnostics[0].message.contains("rustc"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_editing_actions_clipboard_comment_indent() {
        let ws = IdeWorkspace::new(".");
        let id = ws
            .create_buffer(Some("edit.py"), "def f():\n    return 1\n")
            .unwrap();
        ws.dispatch(EditorAction::SetCursorPosition {
            buffer_id: id,
            line: 1,
            column: 5,
            select: false,
        })
        .unwrap();
        let (text, whole) = ws.clipboard_text(id).unwrap();
        assert_eq!((text.as_str(), whole), ("    return 1\n", true));
        ws.dispatch(EditorAction::ToggleLineComment { buffer_id: id })
            .unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\n    # return 1\n");
        ws.dispatch(EditorAction::Undo { buffer_id: id }).unwrap();
        ws.dispatch(EditorAction::MoveCursor {
            buffer_id: id,
            direction: CursorDirection::LineEnd,
            select: false,
        })
        .unwrap();
        ws.dispatch(EditorAction::InsertNewline { buffer_id: id })
            .unwrap();
        ws.dispatch(EditorAction::InsertText {
            buffer_id: id,
            text: "x".into(),
        })
        .unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\n    return 1\n    x\n");
        ws.dispatch(EditorAction::DeleteSelectionOrLine { buffer_id: id })
            .unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\n    return 1\n");
        ws.dispatch(EditorAction::SelectAll { buffer_id: id }).unwrap();
        assert_eq!(ws.clipboard_text(id).unwrap().0, "def f():\n    return 1\n");
        ws.dispatch(EditorAction::Outdent { buffer_id: id }).unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\nreturn 1\n");
        ws.dispatch(EditorAction::SetCursorPosition {
            buffer_id: id,
            line: 1,
            column: 3,
            select: false,
        })
        .unwrap();
        ws.dispatch(EditorAction::Indent { buffer_id: id }).unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\nret urn 1\n");
        ws.dispatch(EditorAction::Undo { buffer_id: id }).unwrap();
        ws.dispatch(EditorAction::MoveCursor {
            buffer_id: id,
            direction: CursorDirection::LineStart,
            select: false,
        })
        .unwrap();
        ws.dispatch(EditorAction::Indent { buffer_id: id }).unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\n    return 1\n");
        ws.dispatch(EditorAction::DeleteWordBackward { buffer_id: id })
            .unwrap();
        assert_eq!(ws.buffer_text(id).unwrap(), "def f():\nreturn 1\n");
    }
}
