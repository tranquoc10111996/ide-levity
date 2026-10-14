//! `ide-desktop`: Cross-platform Desktop IDE (macOS, Windows, Linux) built with `iced`,
//! `cosmic-text`, `alacritty_terminal`, `ide-core`, `ide-lsp`, `ide-terminal`, `ide-search`,
//! and `ide-ai`.
//!
//! Features Antigravity-inspired Dual-View architecture:
//! 1. **Editor View**:
//!    - Activity Bar & Sidebar with 3 panels:
//!      * **Explorer**: Hierarchical expandable directory tree (`list_directory`), gitignore dimming,
//!        create file/folder, delete file/folder.
//!      * **Search**: Workspace-wide gitignore-aware search and regex replace (`ide-search`).
//!      * **Source Control**: Real Git status, stage, unstage, stage all, discard, and commit (`ide-git`).
//!    - Multi-buffer tab bar with dirty indicators (`*`) and close buttons (`×`).
//!    - Toggleable Find & Replace bar (`Ctrl+F` / `Ctrl+H`).
//!    - 120fps `EditorCanvas` (`editor.rs`) with bundled `JetBrains Mono` font, `cosmic-text` +
//!      `unicode-width` cell grid, syntax spans, diagnostics squiggles, Git gutter markers,
//!      multi-cursors, overview ruler scrollbar, double/triple-click word/line selection.
//!    - Bottom panel with real PTY Terminal (`ide-terminal`), Diagnostics (tree-sitter + real LSP
//!      `publishDiagnostics`), and Completions (real LSP + word fallback).
//! 2. **Agent Manager View**:
//!    - Multi-provider AI Agent (`ide-ai` supporting OpenAI, Anthropic, Gemini, OpenRouter, Groq,
//!      DeepSeek, Mistral, xAI, Together, Ollama, LM Studio + deterministic offline edits) with
//!      Unified Diff preview and one-click Accept / Reject controls.

pub mod editor;
pub mod theme;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
use cosmic_text::{Attrs, Buffer as CosmicBuffer, Family, FontSystem, Metrics, Shaping};
use editor::{EditorCanvas, EditorEvent, EditorMetrics};
use iced::keyboard::{self, key::Named, Key, Modifiers};
use iced::widget::canvas::Canvas;
use iced::widget::{
    button, column, container, horizontal_space, row, scrollable, text, text_input, Column, Row,
};
use iced::{Border, Color, Element, Event, Length, Subscription, Task};
use ide_ai::{AiClient, AiConfig, EditProposal, EditRequest, ResolveOptions};
use ide_core::{
    lsp_language_id, ChangeKind, CursorDirection, DiagnosticSeverityLevel, DirEntryInfo,
    DocumentObserver, EditorAction, ExternalDiagnostic, IdeWorkspace, SearchMatch, StatusEntry,
    SyntaxLanguage, ViewportViewModel,
};
use ide_lsp::{
    CompletionEntry, DiagnosticLevel, LanguageServerManager, LspEvent, LspEventReceiver,
    ManagerOptions, ServerState,
};
use ide_search::{FileMatches, SearchQuery};
use ide_terminal::{TerminalEvent, TerminalOptions, TerminalSession};
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver as StdReceiver;
use std::sync::Arc;
use std::time::Duration;

/// Active top-level view in the Dual-View Desktop IDE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopViewMode {
    Editor,
    AgentManager,
}

/// Active panel in the left sidebar of Editor View.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Explorer,
    Search,
    SourceControl,
}

/// Active tab in the bottom panel of Editor View.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomPanelTab {
    Terminal,
    Diagnostics,
    Completions,
}

/// Mode of the modal Command Palette overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPaletteMode {
    Closed,
    /// `Cmd/Ctrl + P`: Quick file open.
    QuickOpenFile,
    /// `Cmd/Ctrl + Shift + P`: IDE command runner.
    CommandRunner,
}

/// Built-in commands executable from the `Cmd/Ctrl + Shift + P` Command Palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    SwitchToEditorView,
    SwitchToAgentManagerView,
    ShowExplorerSidebar,
    ShowWorkspaceSearchSidebar,
    ShowSourceControlSidebar,
    ToggleFindBar,
    NewUntitledFile,
    SaveActiveFile,
    UndoEdit,
    RedoEdit,
    ToggleLineComment,
    AddNextOccurrence,
    GotoDefinition,
    TriggerCompletion,
}

impl PaletteCommand {
    pub fn all() -> &'static [PaletteCommand] {
        &[
            Self::SwitchToEditorView,
            Self::SwitchToAgentManagerView,
            Self::ShowExplorerSidebar,
            Self::ShowWorkspaceSearchSidebar,
            Self::ShowSourceControlSidebar,
            Self::ToggleFindBar,
            Self::NewUntitledFile,
            Self::SaveActiveFile,
            Self::UndoEdit,
            Self::RedoEdit,
            Self::ToggleLineComment,
            Self::AddNextOccurrence,
            Self::GotoDefinition,
            Self::TriggerCompletion,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SwitchToEditorView => "View: Switch to Editor View",
            Self::SwitchToAgentManagerView => "View: Switch to Agent Manager View",
            Self::ShowExplorerSidebar => "View: Show Explorer Sidebar (Ctrl+Shift+E)",
            Self::ShowWorkspaceSearchSidebar => "View: Search in Workspace (Ctrl+Shift+F)",
            Self::ShowSourceControlSidebar => "View: Show Source Control Git (Ctrl+Shift+G)",
            Self::ToggleFindBar => "Edit: Find & Replace in Buffer (Ctrl+F)",
            Self::NewUntitledFile => "File: New Untitled Buffer (Ctrl+N)",
            Self::SaveActiveFile => "File: Save Active Buffer (Ctrl+S)",
            Self::UndoEdit => "Edit: Undo Last Transaction (Ctrl+Z)",
            Self::RedoEdit => "Edit: Redo Last Transaction (Ctrl+Y)",
            Self::ToggleLineComment => "Edit: Toggle Line Comment (Ctrl+/)",
            Self::AddNextOccurrence => "Selection: Add Next Occurrence (Ctrl+D)",
            Self::GotoDefinition => "LSP: Go to Definition (F12)",
            Self::TriggerCompletion => "LSP: Trigger Code Completion (Ctrl+Space)",
        }
    }
}

/// Messages handled by the `IdeDesktopApp` Unidirectional state machine.
#[derive(Debug, Clone)]
pub enum DesktopMessage {
    SwitchViewMode(DesktopViewMode),
    SelectSidebarTab(SidebarTab),
    SelectBottomTab(BottomPanelTab),
    OpenFileFromSidebar(String),
    ToggleDirectory(PathBuf),
    NewFileNameChanged(String),
    CreateNewFile,
    CreateNewFolder,
    DeletePath(PathBuf),
    SelectBufferTab(u64),
    CloseBufferTab(u64),
    NewUntitledBuffer,
    DispatchEditorAction(EditorAction),
    EditorCanvasEvent(EditorEvent),
    AddMultiCursor(u32, u32),
    ToggleSearchBar,
    SearchQueryChanged(String),
    ReplaceBufferQueryChanged(String),
    ReplaceAllInActiveBuffer,
    JumpToNextSearchMatch,
    WorkspaceSearchQueryChanged(String),
    WorkspaceReplaceQueryChanged(String),
    RunWorkspaceSearch,
    RunWorkspaceReplaceAll,
    OpenWorkspaceSearchHit(PathBuf, u32, u32),
    GitStageFile(String),
    GitUnstageFile(String),
    GitStageAll,
    GitDiscardFile(String),
    GitCommitMessageChanged(String),
    GitCommit,
    GitRefresh,
    ToggleCommandPalette(CommandPaletteMode),
    CommandPaletteQueryChanged(String),
    ExecutePaletteSelection(usize),
    InsertCompletionItem(String),
    GotoDefinitionAtCursor,
    ClipboardContentLoaded(Option<String>),
    AgentPromptChanged(String),
    SubmitAgentPrompt,
    AiEditCompleted {
        task_id: u64,
        buffer_id: u64,
        file_path: String,
        original_text: String,
        result: Result<EditProposal, String>,
    },
    SelectAgentTask(u64),
    AcceptAgentTask(u64),
    RejectAgentTask(u64),
    TerminalCommandChanged(String),
    ExecuteTerminalCommand,
    BackgroundTick,
    KeyboardEvent(Key, Modifiers, Option<String>),
}

/// Bridges `IdeWorkspace` document lifecycle notifications into `LanguageServerManager`.
struct LspObserverBridge {
    manager: LanguageServerManager,
}

impl DocumentObserver for LspObserverBridge {
    fn did_open(&self, path: &Path, language: SyntaxLanguage, version: i32, text: &str) {
        let _ = self
            .manager
            .did_open(path, lsp_language_id(language), version, text);
    }

    fn did_change(&self, path: &Path, _language: SyntaxLanguage, version: i32, text: &str) {
        let _ = self.manager.did_change(path, version, text);
    }

    fn did_save(&self, path: &Path, _language: SyntaxLanguage, text: &str) {
        let _ = self.manager.did_save(path, Some(text));
    }

    fn did_close(&self, path: &Path, _language: SyntaxLanguage) {
        let _ = self.manager.did_close(path);
    }
}

/// Flattened visible row in the hierarchical directory tree sidebar.
#[derive(Debug, Clone)]
pub struct FlatTreeEntry {
    pub info: DirEntryInfo,
    pub depth: usize,
    pub is_expanded: bool,
}

/// Terminal emulator wrapper powered by `alacritty_terminal`.
pub struct TerminalEmulator {
    term: Term<VoidListener>,
    processor: AnsiProcessor,
}

impl Default for TerminalEmulator {
    fn default() -> Self {
        Self::new(110, 28)
    }
}

impl TerminalEmulator {
    pub fn new(columns: usize, lines: usize) -> Self {
        let size = TermSize::new(columns, lines);
        let term = Term::new(TermConfig::default(), &size, VoidListener);
        let processor = AnsiProcessor::new();
        let mut emu = Self { term, processor };
        emu.feed_bytes(b"IDE Levity Terminal (alacritty_terminal 0.24 + PTY) - Ready\r\n$ ");
        emu
    }

    /// Feeds raw VT100/ANSI bytes into the `alacritty_terminal` state machine.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.processor.advance(&mut self.term, b);
        }
    }

    /// Extracts the current visible screen lines from the `alacritty_terminal` grid.
    pub fn visible_lines(&self) -> Vec<String> {
        let rows = self.term.screen_lines();
        let cols = self.term.columns();
        if rows == 0 || cols == 0 {
            return Vec::new();
        }
        let raw = self.term.bounds_to_string(
            TermPoint::new(TermLine(0), TermColumn(0)),
            TermPoint::new(TermLine(rows as i32 - 1), TermColumn(cols - 1)),
        );
        raw.lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// Text shaping and measurement engine powered by `cosmic-text`.
pub struct CosmicTextShaper {
    font_system: FontSystem,
    line_buffer: CosmicBuffer,
    font_size: f32,
    line_height: f32,
}

impl Default for CosmicTextShaper {
    fn default() -> Self {
        Self::new(14.0, 21.0)
    }
}

impl CosmicTextShaper {
    pub fn new(font_size: f32, line_height: f32) -> Self {
        let mut font_system = FontSystem::new_with_fonts(
            theme::FONT_BYTES
                .iter()
                .map(|b| cosmic_text::fontdb::Source::Binary(Arc::new(b.to_vec()))),
        );
        let metrics = Metrics::new(font_size, line_height);
        let line_buffer = CosmicBuffer::new(&mut font_system, metrics);
        Self {
            font_system,
            line_buffer,
            font_size,
            line_height,
        }
    }

    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// Shapes `text` with `cosmic-text` and returns the advance width in pixels.
    pub fn measure_line_width(&mut self, text: &str) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        self.line_buffer.set_text(
            &mut self.font_system,
            text,
            Attrs::new().family(Family::Name(theme::MONO_FAMILY)),
            Shaping::Advanced,
        );
        self.line_buffer
            .shape_until_scroll(&mut self.font_system, false);
        let shaped_width: f32 = self
            .line_buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max);
        if shaped_width > 0.0 {
            shaped_width
        } else {
            text.chars().count() as f32 * (self.font_size * 0.6)
        }
    }

    /// Computes the horizontal pixel offset for a 0-indexed character `column` in `line_text`.
    pub fn column_x_offset(&mut self, line_text: &str, column: u32) -> f32 {
        let prefix: String = line_text.chars().take(column as usize).collect();
        self.measure_line_width(&prefix)
    }
}

/// Complete application state for `ide-desktop`.
pub struct IdeDesktopApp {
    pub workspace: Arc<IdeWorkspace>,
    pub viewport: ViewportViewModel,
    pub view_mode: DesktopViewMode,
    pub sidebar_tab: SidebarTab,
    pub bottom_tab: BottomPanelTab,
    pub palette_mode: CommandPaletteMode,
    pub palette_query: String,
    pub search_bar_open: bool,
    pub search_query: String,
    pub replace_buffer_query: String,
    pub search_matches: Vec<SearchMatch>,
    pub current_match_idx: usize,
    pub workspace_search_query: String,
    pub workspace_replace_query: String,
    pub workspace_search_results: Vec<FileMatches>,
    pub expanded_dirs: BTreeSet<PathBuf>,
    pub new_item_name: String,
    pub git_status_entries: Vec<StatusEntry>,
    pub git_commit_message: String,
    pub git_status_banner: Option<String>,
    pub agent_prompt_input: String,
    pub selected_agent_task_id: Option<u64>,
    pub ai_config: Option<AiConfig>,
    pub terminal_input: String,
    pub terminal: Arc<Mutex<TerminalEmulator>>,
    pub pty_session: Option<Arc<TerminalSession>>,
    pub pty_events: Arc<Mutex<Option<StdReceiver<TerminalEvent>>>>,
    pub shaper: Arc<Mutex<CosmicTextShaper>>,
    pub editor_metrics: EditorMetrics,
    pub scroll_x: f32,
    pub cursor_visible: bool,
    pub internal_clipboard: String,
    pub lsp_manager: Option<LanguageServerManager>,
    pub lsp_events: Arc<Mutex<Option<LspEventReceiver>>>,
    pub lsp_completions: Vec<CompletionEntry>,
    pub lsp_status_text: String,
    pub hover_tooltip: Option<String>,
    pub workspace_files: Vec<String>,
    pub git_branch: String,
}

impl Default for IdeDesktopApp {
    fn default() -> Self {
        Self::new(".")
    }
}

impl IdeDesktopApp {
    pub fn new(root_dir: &str) -> Self {
        let path_meta = std::fs::metadata(root_dir).ok();
        let (ws_root, initial_file) = if path_meta.as_ref().is_some_and(|m| m.is_file()) {
            let p = std::fs::canonicalize(root_dir)
                .unwrap_or_else(|_| PathBuf::from(root_dir));
            let parent = p.parent().unwrap_or_else(|| Path::new("."));
            let repo_root = parent
                .ancestors()
                .find(|a| a.join(".git").exists())
                .unwrap_or(parent);
            (
                repo_root.to_string_lossy().into_owned(),
                Some(p.to_string_lossy().into_owned()),
            )
        } else {
            (root_dir.to_string(), None)
        };

        let workspace = Arc::new(IdeWorkspace::new(&ws_root));

        let (lsp_manager, lsp_events) = match LanguageServerManager::new(ManagerOptions::default())
        {
            Ok((mgr, rx)) => {
                workspace.set_document_observer(Some(Arc::new(LspObserverBridge {
                    manager: mgr.clone(),
                })));
                (Some(mgr), Arc::new(Mutex::new(Some(rx))))
            }
            Err(_) => (None, Arc::new(Mutex::new(None))),
        };

        let workspace_files = workspace.list_workspace_files();
        if let Some(f) = initial_file {
            let _ = workspace.dispatch(EditorAction::OpenFile { path: f });
        } else if workspace_files.iter().any(|f| f == "Cargo.toml") {
            let _ = workspace.dispatch(EditorAction::OpenFile {
                path: "Cargo.toml".to_string(),
            });
        }

        let viewport = workspace
            .active_viewport()
            .expect("Default buffer viewport must exist");
        let git_branch = workspace
            .current_git_branch()
            .unwrap_or_else(|| "main".to_string());
        let git_status_entries = workspace
            .git_repository()
            .and_then(|r| r.status_detailed().ok())
            .unwrap_or_default();

        let ai_config = if let Some(mgr) = &lsp_manager {
            let mut opts = ResolveOptions::from_process_env();
            opts.probe_local = false;
            mgr.runtime_handle()
                .block_on(ide_ai::resolve_config(&opts))
                .config()
                .cloned()
        } else {
            None
        };

        let pty_opts = TerminalOptions {
            working_dir: Some(workspace.root_dir()),
            cols: 110,
            rows: 24,
            ..TerminalOptions::default()
        };
        let (pty_session, pty_events) = match TerminalSession::spawn(pty_opts) {
            Ok(mut sess) => {
                let rx = sess.take_events();
                (Some(Arc::new(sess)), Arc::new(Mutex::new(rx)))
            }
            Err(_) => (None, Arc::new(Mutex::new(None))),
        };

        let mut expanded_dirs = BTreeSet::new();
        expanded_dirs.insert(workspace.root_dir());

        let initial_view_env = std::env::var("IDE_INITIAL_VIEW").unwrap_or_default();
        let view_mode = if initial_view_env.eq_ignore_ascii_case("agent") {
            DesktopViewMode::AgentManager
        } else {
            DesktopViewMode::Editor
        };
        let sidebar_tab = if initial_view_env.eq_ignore_ascii_case("search") {
            SidebarTab::Search
        } else if initial_view_env.eq_ignore_ascii_case("git") {
            SidebarTab::SourceControl
        } else {
            SidebarTab::Explorer
        };

        Self {
            workspace,
            viewport,
            view_mode,
            sidebar_tab,
            bottom_tab: BottomPanelTab::Terminal,
            palette_mode: CommandPaletteMode::Closed,
            palette_query: String::new(),
            search_bar_open: false,
            search_query: String::new(),
            replace_buffer_query: String::new(),
            search_matches: Vec::new(),
            current_match_idx: 0,
            workspace_search_query: String::new(),
            workspace_replace_query: String::new(),
            workspace_search_results: Vec::new(),
            expanded_dirs,
            new_item_name: String::new(),
            git_status_entries,
            git_commit_message: String::new(),
            git_status_banner: None,
            agent_prompt_input: String::new(),
            selected_agent_task_id: None,
            ai_config,
            terminal_input: String::new(),
            terminal: Arc::new(Mutex::new(TerminalEmulator::default())),
            pty_session,
            pty_events,
            shaper: Arc::new(Mutex::new(CosmicTextShaper::default())),
            editor_metrics: EditorMetrics::measure(14.0),
            scroll_x: 0.0,
            cursor_visible: true,
            internal_clipboard: String::new(),
            lsp_manager,
            lsp_events,
            lsp_completions: Vec::new(),
            lsp_status_text: "LSP: Ready".to_string(),
            hover_tooltip: None,
            workspace_files,
            git_branch,
        }
    }

    pub fn title(&self) -> String {
        let dirty = if self.viewport.is_dirty { " *" } else { "" };
        format!(
            "IDE Levity - {}{} [{}]",
            self.viewport.file_path, dirty, self.git_branch
        )
    }

    /// Flattens the currently expanded directories in the workspace into a tree list.
    pub fn visible_tree_entries(&self) -> Vec<FlatTreeEntry> {
        let mut out = Vec::new();
        self.collect_tree_dir(&self.workspace.root_dir(), 0, &mut out);
        out
    }

    fn collect_tree_dir(&self, dir: &Path, depth: usize, out: &mut Vec<FlatTreeEntry>) {
        let Ok(entries) = self.workspace.list_directory(dir) else {
            return;
        };
        for info in entries {
            let is_expanded = info.is_dir && self.expanded_dirs.contains(&info.path);
            let path = info.path.clone();
            let is_dir = info.is_dir;
            out.push(FlatTreeEntry {
                info,
                depth,
                is_expanded,
            });
            if is_dir && is_expanded {
                self.collect_tree_dir(&path, depth + 1, out);
            }
        }
    }

    fn refresh_git_state(&mut self) {
        self.workspace.refresh_git();
        self.git_branch = self
            .workspace
            .current_git_branch()
            .unwrap_or_else(|| "main".to_string());
        self.git_status_entries = self
            .workspace
            .git_repository()
            .and_then(|r| r.status_detailed().ok())
            .unwrap_or_default();
        if let Ok(vm) = self.workspace.viewport_view_model(self.viewport.buffer_id) {
            self.viewport = vm;
        }
    }

    fn refresh_search_matches(&mut self) {
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.current_match_idx = 0;
        } else {
            self.search_matches = self
                .workspace
                .search_buffer(self.viewport.buffer_id, &self.search_query, false)
                .unwrap_or_default();
            if self.current_match_idx >= self.search_matches.len() {
                self.current_match_idx = 0;
            }
        }
    }

    /// Polls non-blocking LSP events, PTY events, and external file changes.
    pub fn poll_background_events(&mut self) {
        let mut needs_viewport_refresh = false;
        if let Some(rx) = self.lsp_events.lock().as_mut() {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    LspEvent::Diagnostics { path, items, .. } => {
                        let ext: Vec<ExternalDiagnostic> = items
                            .into_iter()
                            .map(|d| ExternalDiagnostic {
                                start_line: d.range.start.line,
                                start_utf16: d.range.start.character,
                                end_line: d.range.end.line,
                                end_utf16: d.range.end.character,
                                severity: match d.severity {
                                    DiagnosticLevel::Error => DiagnosticSeverityLevel::Error,
                                    DiagnosticLevel::Warning => {
                                        DiagnosticSeverityLevel::Warning
                                    }
                                    DiagnosticLevel::Information => {
                                        DiagnosticSeverityLevel::Information
                                    }
                                    DiagnosticLevel::Hint => DiagnosticSeverityLevel::Hint,
                                },
                                message: d.message,
                                source: d.source,
                            })
                            .collect();
                        self.workspace.set_external_diagnostics(&path, ext);
                        needs_viewport_refresh = true;
                    }
                    LspEvent::ServerStatus {
                        server_id, state, ..
                    } => {
                        self.lsp_status_text = match state {
                            ServerState::Starting => format!("LSP: {server_id} (Starting...)"),
                            ServerState::Ready => format!("LSP: {server_id} (Ready)"),
                            ServerState::Indexing(msg) => {
                                if msg.is_empty() {
                                    format!("LSP: {server_id} (Indexing...)")
                                } else {
                                    format!("LSP: {server_id} ({msg})")
                                }
                            }
                            ServerState::Failed(err) => format!("LSP: {server_id} failed ({err})"),
                            ServerState::Exited => format!("LSP: {server_id} exited"),
                        };
                    }
                    LspEvent::ServerMissing {
                        language_id,
                        install_hint,
                        ..
                    } => {
                        self.lsp_status_text = format!("LSP ({language_id}): {install_hint}");
                    }
                    LspEvent::ShowMessage {
                        server_id, message, ..
                    } => {
                        self.lsp_status_text = format!("{server_id}: {message}");
                    }
                    LspEvent::LogMessage { .. } => {}
                }
            }
        }

        if let Some(pty) = &self.pty_session {
            let mut got_wakeup = false;
            if let Some(rx) = self.pty_events.lock().as_ref() {
                while let Ok(ev) = rx.try_recv() {
                    if matches!(ev, TerminalEvent::Wakeup) {
                        got_wakeup = true;
                    }
                }
            }
            if got_wakeup {
                let snap = pty.snapshot();
                let mut text_block = String::new();
                for row_cells in &snap.cells {
                    let line: String = row_cells.iter().map(|c| c.ch).collect();
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        text_block.push_str(trimmed);
                        text_block.push_str("\r\n");
                    }
                }
                if !text_block.is_empty() {
                    self.terminal.lock().feed_bytes(text_block.as_bytes());
                }
            }
        }

        if !self.workspace.poll_external_changes().is_empty() {
            needs_viewport_refresh = true;
        }

        if needs_viewport_refresh {
            if let Ok(vm) = self.workspace.viewport_view_model(self.viewport.buffer_id) {
                self.viewport = vm;
            }
        }
    }

    /// Returns filtered file paths when the Command Palette is in `QuickOpenFile` mode.
    pub fn filtered_palette_files(&self) -> Vec<String> {
        let q = self.palette_query.trim().to_lowercase();
        self.workspace_files
            .iter()
            .filter(|f| q.is_empty() || f.to_lowercase().contains(&q))
            .take(15)
            .cloned()
            .collect()
    }

    /// Returns filtered commands when the Command Palette is in `CommandRunner` mode.
    pub fn filtered_palette_commands(&self) -> Vec<PaletteCommand> {
        let q = self.palette_query.trim().to_lowercase();
        PaletteCommand::all()
            .iter()
            .copied()
            .filter(|cmd| q.is_empty() || cmd.label().to_lowercase().contains(&q))
            .collect()
    }

    /// Processes a `DesktopMessage` and updates the state.
    pub fn update(&mut self, message: DesktopMessage) -> Task<DesktopMessage> {
        self.poll_background_events();
        match message {
            DesktopMessage::BackgroundTick => {
                self.cursor_visible = !self.cursor_visible;
            }
            DesktopMessage::SwitchViewMode(mode) => {
                self.view_mode = mode;
            }
            DesktopMessage::SelectSidebarTab(tab) => {
                self.sidebar_tab = tab;
                if tab == SidebarTab::SourceControl {
                    self.refresh_git_state();
                }
            }
            DesktopMessage::SelectBottomTab(tab) => {
                self.bottom_tab = tab;
            }
            DesktopMessage::OpenFileFromSidebar(path) => {
                if let Ok(vm) = self.workspace.dispatch(EditorAction::OpenFile { path }) {
                    self.viewport = vm;
                    self.scroll_x = 0.0;
                    self.refresh_search_matches();
                }
                self.palette_mode = CommandPaletteMode::Closed;
            }
            DesktopMessage::ToggleDirectory(path) => {
                if !self.expanded_dirs.remove(&path) {
                    self.expanded_dirs.insert(path);
                }
            }
            DesktopMessage::NewFileNameChanged(name) => {
                self.new_item_name = name;
            }
            DesktopMessage::CreateNewFile => {
                let rel = self.new_item_name.trim().to_string();
                if !rel.is_empty() {
                    if let Ok(vm) = self.workspace.create_file(&rel) {
                        self.viewport = vm;
                        self.workspace_files = self.workspace.list_workspace_files();
                        self.new_item_name.clear();
                    }
                }
            }
            DesktopMessage::CreateNewFolder => {
                let rel = self.new_item_name.trim().to_string();
                if !rel.is_empty() {
                    if let Ok(dir) = self.workspace.create_dir(&rel) {
                        self.expanded_dirs.insert(dir);
                        self.new_item_name.clear();
                    }
                }
            }
            DesktopMessage::DeletePath(path) => {
                if self.workspace.delete_path(&path).is_ok() {
                    self.workspace_files = self.workspace.list_workspace_files();
                    if let Ok(vm) = self.workspace.active_viewport() {
                        self.viewport = vm;
                    }
                }
            }
            DesktopMessage::NewUntitledBuffer => {
                if let Ok(id) = self.workspace.new_untitled_buffer() {
                    if let Ok(vm) = self.workspace.viewport_view_model(id) {
                        self.viewport = vm;
                    }
                }
            }
            DesktopMessage::SelectBufferTab(buffer_id) => {
                if let Ok(vm) = self.workspace.set_active_buffer_id(buffer_id) {
                    self.viewport = vm;
                    self.scroll_x = 0.0;
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::CloseBufferTab(buffer_id) => {
                if let Ok(vm) = self.workspace.close_buffer(buffer_id) {
                    self.viewport = vm;
                    self.scroll_x = 0.0;
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::DispatchEditorAction(action) => {
                let is_completion = matches!(action, EditorAction::TriggerLspCompletion { .. });
                let is_save = matches!(action, EditorAction::SaveFile { .. });
                if let Ok(vm) = self.workspace.dispatch(action) {
                    self.viewport = vm;
                    self.cursor_visible = true;
                    self.refresh_search_matches();
                    if is_save {
                        self.refresh_git_state();
                    }
                    if is_completion {
                        self.request_lsp_completions();
                        self.bottom_tab = BottomPanelTab::Completions;
                    }
                }
            }
            DesktopMessage::EditorCanvasEvent(ev) => {
                let buffer_id = self.viewport.buffer_id;
                match ev {
                    EditorEvent::Press {
                        line,
                        col,
                        shift,
                        alt,
                        clicks,
                        ..
                    } => {
                        self.cursor_visible = true;
                        if alt {
                            return self.update(DesktopMessage::AddMultiCursor(line, col));
                        }
                        match clicks {
                            2 => {
                                let _ = self.workspace.dispatch(EditorAction::SetCursorPosition {
                                    buffer_id,
                                    line,
                                    column: col,
                                    select: false,
                                });
                                return self.update(DesktopMessage::DispatchEditorAction(
                                    EditorAction::AddNextOccurrence { buffer_id },
                                ));
                            }
                            3 => {
                                let _ = self.workspace.dispatch(EditorAction::SetCursorPosition {
                                    buffer_id,
                                    line,
                                    column: 0,
                                    select: false,
                                });
                                return self.update(DesktopMessage::DispatchEditorAction(
                                    EditorAction::MoveCursor {
                                        buffer_id,
                                        direction: CursorDirection::LineEnd,
                                        select: true,
                                    },
                                ));
                            }
                            _ => {
                                return self.update(DesktopMessage::DispatchEditorAction(
                                    EditorAction::SetCursorPosition {
                                        buffer_id,
                                        line,
                                        column: col,
                                        select: shift,
                                    },
                                ));
                            }
                        }
                    }
                    EditorEvent::Drag { line, col } => {
                        return self.update(DesktopMessage::DispatchEditorAction(
                            EditorAction::SetCursorPosition {
                                buffer_id,
                                line,
                                column: col,
                                select: true,
                            },
                        ));
                    }
                    EditorEvent::Scroll { lines, pixels_x } => {
                        self.scroll_x = (self.scroll_x + pixels_x).max(0.0);
                        let new_first =
                            (self.viewport.first_visible_line as i32 + lines).max(0) as u32;
                        let visible_count = self.viewport.visible_lines.len().max(25) as u32;
                        return self.update(DesktopMessage::DispatchEditorAction(
                            EditorAction::ScrollViewport {
                                buffer_id,
                                first_visible_line: new_first,
                                visible_line_count: visible_count,
                            },
                        ));
                    }
                    EditorEvent::ScrollTo { fraction } => {
                        let max_line = self.viewport.total_lines.saturating_sub(1);
                        let new_first = ((max_line as f32) * fraction).round() as u32;
                        let visible_count = self.viewport.visible_lines.len().max(25) as u32;
                        return self.update(DesktopMessage::DispatchEditorAction(
                            EditorAction::ScrollViewport {
                                buffer_id,
                                first_visible_line: new_first,
                                visible_line_count: visible_count,
                            },
                        ));
                    }
                    EditorEvent::Resized { rows, .. } => {
                        return self.update(DesktopMessage::DispatchEditorAction(
                            EditorAction::ScrollViewport {
                                buffer_id,
                                first_visible_line: self.viewport.first_visible_line,
                                visible_line_count: rows.max(10),
                            },
                        ));
                    }
                    EditorEvent::Hover(_) => {}
                }
            }
            DesktopMessage::AddMultiCursor(line, col) => {
                if let Ok(vm) = self
                    .workspace
                    .add_cursor(self.viewport.buffer_id, line, col)
                {
                    self.viewport = vm;
                }
            }
            DesktopMessage::ToggleSearchBar => {
                self.search_bar_open = !self.search_bar_open;
                if !self.search_bar_open {
                    self.search_matches.clear();
                } else {
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::SearchQueryChanged(q) => {
                self.search_query = q;
                self.refresh_search_matches();
            }
            DesktopMessage::ReplaceBufferQueryChanged(r) => {
                self.replace_buffer_query = r;
            }
            DesktopMessage::ReplaceAllInActiveBuffer => {
                if !self.search_query.is_empty() {
                    let _ = self.workspace.replace_all_in_buffer(
                        self.viewport.buffer_id,
                        &self.search_query,
                        &self.replace_buffer_query,
                        false,
                    );
                    if let Ok(vm) = self.workspace.viewport_view_model(self.viewport.buffer_id) {
                        self.viewport = vm;
                    }
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::JumpToNextSearchMatch => {
                if !self.search_matches.is_empty() {
                    let m = &self.search_matches[self.current_match_idx % self.search_matches.len()];
                    let line = m.line;
                    let column = m.column;
                    self.current_match_idx =
                        (self.current_match_idx + 1) % self.search_matches.len();
                    if let Ok(vm) = self.workspace.dispatch(EditorAction::SetCursorPosition {
                        buffer_id: self.viewport.buffer_id,
                        line,
                        column,
                        select: false,
                    }) {
                        self.viewport = vm;
                    }
                }
            }
            DesktopMessage::WorkspaceSearchQueryChanged(q) => {
                self.workspace_search_query = q;
            }
            DesktopMessage::WorkspaceReplaceQueryChanged(r) => {
                self.workspace_replace_query = r;
            }
            DesktopMessage::RunWorkspaceSearch => {
                let q = self.workspace_search_query.trim().to_string();
                if q.is_empty() {
                    self.workspace_search_results.clear();
                } else {
                    let query = SearchQuery::new(q);
                    let cancel = AtomicBool::new(false);
                    if let Ok(res) =
                        ide_search::search_workspace(&self.workspace.root_dir(), &query, &cancel)
                    {
                        self.workspace_search_results = res.files;
                    }
                }
            }
            DesktopMessage::RunWorkspaceReplaceAll => {
                let q = self.workspace_search_query.trim().to_string();
                if !q.is_empty() {
                    let query = SearchQuery::new(q);
                    if ide_search::replace_in_files(
                        &self.workspace.root_dir(),
                        &query,
                        &self.workspace_replace_query,
                        None,
                    )
                    .is_ok()
                    {
                        let _ = self.workspace.poll_external_changes();
                        if let Ok(vm) = self.workspace.viewport_view_model(self.viewport.buffer_id)
                        {
                            self.viewport = vm;
                        }
                        return self.update(DesktopMessage::RunWorkspaceSearch);
                    }
                }
            }
            DesktopMessage::OpenWorkspaceSearchHit(path, line, col) => {
                let path_str = path.to_string_lossy().into_owned();
                if let Ok(vm) = self
                    .workspace
                    .dispatch(EditorAction::OpenFile { path: path_str })
                {
                    let buf_id = vm.buffer_id;
                    self.viewport = self
                        .workspace
                        .dispatch(EditorAction::SetCursorPosition {
                            buffer_id: buf_id,
                            line,
                            column: col,
                            select: false,
                        })
                        .unwrap_or(vm);
                }
            }
            DesktopMessage::GitStageFile(rel_path) => {
                if let Some(repo) = self.workspace.git_repository() {
                    let _ = repo.stage(&[Path::new(&rel_path)]);
                    self.refresh_git_state();
                }
            }
            DesktopMessage::GitUnstageFile(rel_path) => {
                if let Some(repo) = self.workspace.git_repository() {
                    let _ = repo.unstage(&[Path::new(&rel_path)]);
                    self.refresh_git_state();
                }
            }
            DesktopMessage::GitStageAll => {
                if let Some(repo) = self.workspace.git_repository() {
                    let _ = repo.stage_all();
                    self.refresh_git_state();
                }
            }
            DesktopMessage::GitDiscardFile(rel_path) => {
                if let Some(repo) = self.workspace.git_repository() {
                    let _ = repo.discard_worktree_changes(&[Path::new(&rel_path)]);
                    let _ = self.workspace.poll_external_changes();
                    self.refresh_git_state();
                }
            }
            DesktopMessage::GitCommitMessageChanged(msg) => {
                self.git_commit_message = msg;
            }
            DesktopMessage::GitCommit => {
                let msg = self.git_commit_message.trim().to_string();
                if !msg.is_empty() {
                    if let Some(repo) = self.workspace.git_repository() {
                        match repo.commit(&msg, None, None) {
                            Ok(oid) => {
                                let short = &oid[..oid.len().min(7)];
                                self.git_status_banner = Some(format!("Committed {short}: {msg}"));
                                self.git_commit_message.clear();
                                self.refresh_git_state();
                            }
                            Err(e) => {
                                self.git_status_banner = Some(format!("Commit error: {e}"));
                            }
                        }
                    }
                }
            }
            DesktopMessage::GitRefresh => {
                self.refresh_git_state();
            }
            DesktopMessage::ToggleCommandPalette(mode) => {
                if self.palette_mode == mode {
                    self.palette_mode = CommandPaletteMode::Closed;
                } else {
                    self.palette_mode = mode;
                    self.palette_query.clear();
                }
            }
            DesktopMessage::CommandPaletteQueryChanged(q) => {
                self.palette_query = q;
            }
            DesktopMessage::ExecutePaletteSelection(idx) => {
                let mode = self.palette_mode;
                self.palette_mode = CommandPaletteMode::Closed;
                match mode {
                    CommandPaletteMode::QuickOpenFile => {
                        let files = self.filtered_palette_files();
                        if let Some(chosen) = files.get(idx) {
                            return self.update(DesktopMessage::OpenFileFromSidebar(chosen.clone()));
                        }
                    }
                    CommandPaletteMode::CommandRunner => {
                        let cmds = self.filtered_palette_commands();
                        if let Some(&cmd) = cmds.get(idx) {
                            let buf_id = self.viewport.buffer_id;
                            match cmd {
                                PaletteCommand::SwitchToEditorView => {
                                    self.view_mode = DesktopViewMode::Editor;
                                }
                                PaletteCommand::SwitchToAgentManagerView => {
                                    self.view_mode = DesktopViewMode::AgentManager;
                                }
                                PaletteCommand::ShowExplorerSidebar => {
                                    self.view_mode = DesktopViewMode::Editor;
                                    self.sidebar_tab = SidebarTab::Explorer;
                                }
                                PaletteCommand::ShowWorkspaceSearchSidebar => {
                                    self.view_mode = DesktopViewMode::Editor;
                                    self.sidebar_tab = SidebarTab::Search;
                                }
                                PaletteCommand::ShowSourceControlSidebar => {
                                    self.view_mode = DesktopViewMode::Editor;
                                    self.sidebar_tab = SidebarTab::SourceControl;
                                    self.refresh_git_state();
                                }
                                PaletteCommand::ToggleFindBar => {
                                    return self.update(DesktopMessage::ToggleSearchBar);
                                }
                                PaletteCommand::NewUntitledFile => {
                                    return self.update(DesktopMessage::NewUntitledBuffer);
                                }
                                PaletteCommand::SaveActiveFile => {
                                    return self.update(DesktopMessage::DispatchEditorAction(
                                        EditorAction::SaveFile { buffer_id: buf_id },
                                    ));
                                }
                                PaletteCommand::UndoEdit => {
                                    return self.update(DesktopMessage::DispatchEditorAction(
                                        EditorAction::Undo { buffer_id: buf_id },
                                    ));
                                }
                                PaletteCommand::RedoEdit => {
                                    return self.update(DesktopMessage::DispatchEditorAction(
                                        EditorAction::Redo { buffer_id: buf_id },
                                    ));
                                }
                                PaletteCommand::ToggleLineComment => {
                                    return self.update(DesktopMessage::DispatchEditorAction(
                                        EditorAction::ToggleLineComment { buffer_id: buf_id },
                                    ));
                                }
                                PaletteCommand::AddNextOccurrence => {
                                    return self.update(DesktopMessage::DispatchEditorAction(
                                        EditorAction::AddNextOccurrence { buffer_id: buf_id },
                                    ));
                                }
                                PaletteCommand::GotoDefinition => {
                                    return self.update(DesktopMessage::GotoDefinitionAtCursor);
                                }
                                PaletteCommand::TriggerCompletion => {
                                    return self.update(DesktopMessage::DispatchEditorAction(
                                        EditorAction::TriggerLspCompletion { buffer_id: buf_id },
                                    ));
                                }
                            }
                        }
                    }
                    CommandPaletteMode::Closed => {}
                }
            }
            DesktopMessage::InsertCompletionItem(item) => {
                let prefix = self.workspace.word_prefix_at_cursor(self.viewport.buffer_id);
                let suffix = item
                    .strip_prefix(&prefix)
                    .map(str::to_string)
                    .unwrap_or(item);
                if let Ok(vm) = self.workspace.dispatch(EditorAction::InsertText {
                    buffer_id: self.viewport.buffer_id,
                    text: suffix,
                }) {
                    self.viewport = vm;
                }
            }
            DesktopMessage::GotoDefinitionAtCursor => {
                if let (Some(mgr), Some(path), Some((line, utf16_col))) = (
                    &self.lsp_manager,
                    self.workspace.buffer_path(self.viewport.buffer_id),
                    self.workspace.primary_cursor_utf16(self.viewport.buffer_id),
                ) {
                    if let Ok(locs) = mgr.definition(&path, line, utf16_col).wait() {
                        if let Some(loc) = locs.first() {
                            let target_path = loc.path.to_string_lossy().into_owned();
                            if let Ok(vm) = self
                                .workspace
                                .dispatch(EditorAction::OpenFile { path: target_path })
                            {
                                let buf_id = vm.buffer_id;
                                let (char_line, char_col) = self
                                    .workspace
                                    .utf16_to_char_col(
                                        buf_id,
                                        loc.range.start.line,
                                        loc.range.start.character,
                                    )
                                    .unwrap_or((loc.range.start.line, loc.range.start.character));
                                if let Ok(vm2) =
                                    self.workspace.dispatch(EditorAction::SetCursorPosition {
                                        buffer_id: buf_id,
                                        line: char_line,
                                        column: char_col,
                                        select: false,
                                    })
                                {
                                    self.viewport = vm2;
                                }
                            }
                        }
                    }
                }
            }
            DesktopMessage::ClipboardContentLoaded(clip_opt) => {
                let text_to_paste = clip_opt.unwrap_or_else(|| self.internal_clipboard.clone());
                if !text_to_paste.is_empty() {
                    if let Ok(vm) = self.workspace.dispatch(EditorAction::InsertText {
                        buffer_id: self.viewport.buffer_id,
                        text: text_to_paste,
                    }) {
                        self.viewport = vm;
                        self.refresh_search_matches();
                    }
                }
            }
            DesktopMessage::AgentPromptChanged(val) => {
                self.agent_prompt_input = val;
            }
            DesktopMessage::SubmitAgentPrompt => {
                let prompt = self.agent_prompt_input.trim().to_string();
                if !prompt.is_empty() {
                    self.agent_prompt_input.clear();
                    let is_deterministic_syntax =
                        prompt.contains("->") || prompt.starts_with("doc:");
                    if !is_deterministic_syntax {
                        if let Some(cfg) = self.ai_config.clone() {
                            let buffer_id = self.viewport.buffer_id;
                            let file_path = self.viewport.file_path.clone();
                            let original_text =
                                self.workspace.buffer_text(buffer_id).unwrap_or_default();
                            let language = self
                                .workspace
                                .buffer_language(buffer_id)
                                .map(|l| lsp_language_id(l).to_string())
                                .unwrap_or_else(|| "text".to_string());
                            let selection = self
                                .viewport
                                .selections
                                .first()
                                .map(|s| (s.start_line, s.end_line));
                            let task_id = self
                                .workspace
                                .agent_manager()
                                .start_running_task(prompt.clone(), file_path.clone());
                            self.workspace.bind_agent_task(task_id, buffer_id);
                            self.selected_agent_task_id = Some(task_id);
                            if let Ok(vm) = self.workspace.viewport_view_model(buffer_id) {
                                self.viewport = vm;
                            }
                            let req = EditRequest {
                                instruction: prompt,
                                file_path: file_path.clone(),
                                language,
                                file_text: original_text.clone(),
                                selection,
                                extra_context: Vec::new(),
                            };
                            return Task::perform(
                                async move {
                                    let client =
                                        AiClient::new(cfg).map_err(|e| e.to_string())?;
                                    ide_ai::propose_edit(&client, &req)
                                        .await
                                        .map_err(|e| e.to_string())
                                },
                                move |result| DesktopMessage::AiEditCompleted {
                                    task_id,
                                    buffer_id,
                                    file_path: file_path.clone(),
                                    original_text: original_text.clone(),
                                    result,
                                },
                            );
                        }
                    }
                    if let Ok(vm) = self
                        .workspace
                        .dispatch(EditorAction::SubmitAgentPrompt { prompt })
                    {
                        self.selected_agent_task_id = vm.agent_tasks.last().map(|t| t.task_id);
                        self.viewport = vm;
                    }
                }
            }
            DesktopMessage::AiEditCompleted {
                task_id,
                buffer_id,
                file_path,
                original_text,
                result,
            } => {
                match result {
                    Ok(proposal) => {
                        let _ = self.workspace.agent_manager().complete_task_with_proposal(
                            task_id,
                            &file_path,
                            &original_text,
                            &proposal.new_text,
                            proposal.explanation,
                        );
                    }
                    Err(err_msg) => {
                        let _ = self.workspace.agent_manager().fail_task(task_id, err_msg);
                    }
                }
                if let Ok(vm) = self.workspace.viewport_view_model(buffer_id) {
                    self.viewport = vm;
                }
            }
            DesktopMessage::SelectAgentTask(task_id) => {
                self.selected_agent_task_id = Some(task_id);
            }
            DesktopMessage::AcceptAgentTask(task_id) => {
                if let Ok(vm) = self
                    .workspace
                    .accept_agent_task(task_id, self.viewport.buffer_id)
                {
                    self.viewport = vm;
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::RejectAgentTask(task_id) => {
                if let Ok(vm) = self
                    .workspace
                    .reject_agent_task(task_id, self.viewport.buffer_id)
                {
                    self.viewport = vm;
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::TerminalCommandChanged(cmd) => {
                self.terminal_input = cmd;
            }
            DesktopMessage::ExecuteTerminalCommand => {
                let cmd = self.terminal_input.trim().to_string();
                if !cmd.is_empty() {
                    if let Some(pty) = &self.pty_session {
                        pty.write_str(&format!("{cmd}\r"));
                    }
                    let banner = format!("{cmd}\r\n");
                    self.terminal.lock().feed_bytes(banner.as_bytes());
                    let shell_out = ProcessCommand::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .current_dir(self.workspace.root_dir())
                        .output();
                    match shell_out {
                        Ok(out) => {
                            for line in String::from_utf8_lossy(&out.stdout).lines().take(20) {
                                let formatted = format!("{line}\r\n");
                                self.terminal.lock().feed_bytes(formatted.as_bytes());
                            }
                            for line in String::from_utf8_lossy(&out.stderr).lines().take(10) {
                                let formatted = format!("{line}\r\n");
                                self.terminal.lock().feed_bytes(formatted.as_bytes());
                            }
                            let done = format!(
                                "[completed: {cmd} (exit {})]\r\n$ ",
                                out.status.code().unwrap_or(0)
                            );
                            self.terminal.lock().feed_bytes(done.as_bytes());
                        }
                        Err(e) => {
                            let err_msg = format!("Error: {e}\r\n$ ");
                            self.terminal.lock().feed_bytes(err_msg.as_bytes());
                        }
                    }
                    self.terminal_input.clear();
                }
            }
            DesktopMessage::KeyboardEvent(key, modifiers, text_opt) => {
                let cmd_or_ctrl = modifiers.command() || modifiers.control();
                let buffer_id = self.viewport.buffer_id;

                if key == Key::Named(Named::F12) {
                    return self.update(DesktopMessage::GotoDefinitionAtCursor);
                }

                if cmd_or_ctrl {
                    if key == Key::Named(Named::Space) {
                        return self.update(DesktopMessage::DispatchEditorAction(
                            EditorAction::TriggerLspCompletion { buffer_id },
                        ));
                    }
                    if let Key::Character(c) = &key {
                        if c.eq_ignore_ascii_case("p") {
                            if modifiers.shift() {
                                return self.update(DesktopMessage::ToggleCommandPalette(
                                    CommandPaletteMode::CommandRunner,
                                ));
                            } else {
                                return self.update(DesktopMessage::ToggleCommandPalette(
                                    CommandPaletteMode::QuickOpenFile,
                                ));
                            }
                        } else if c.eq_ignore_ascii_case("f") {
                            if modifiers.shift() {
                                self.view_mode = DesktopViewMode::Editor;
                                self.sidebar_tab = SidebarTab::Search;
                                return Task::none();
                            } else {
                                return self.update(DesktopMessage::ToggleSearchBar);
                            }
                        } else if c.eq_ignore_ascii_case("g") && modifiers.shift() {
                            self.view_mode = DesktopViewMode::Editor;
                            return self.update(DesktopMessage::SelectSidebarTab(
                                SidebarTab::SourceControl,
                            ));
                        } else if c.eq_ignore_ascii_case("e") && modifiers.shift() {
                            self.view_mode = DesktopViewMode::Editor;
                            return self
                                .update(DesktopMessage::SelectSidebarTab(SidebarTab::Explorer));
                        } else if c.eq_ignore_ascii_case("n") {
                            return self.update(DesktopMessage::NewUntitledBuffer);
                        } else if c.eq_ignore_ascii_case("w") {
                            return self.update(DesktopMessage::CloseBufferTab(buffer_id));
                        } else if c.eq_ignore_ascii_case("z") {
                            if modifiers.shift() {
                                return self.update(DesktopMessage::DispatchEditorAction(
                                    EditorAction::Redo { buffer_id },
                                ));
                            } else {
                                return self.update(DesktopMessage::DispatchEditorAction(
                                    EditorAction::Undo { buffer_id },
                                ));
                            }
                        } else if c.eq_ignore_ascii_case("y") {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::Redo { buffer_id },
                            ));
                        } else if c.eq_ignore_ascii_case("s") {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::SaveFile { buffer_id },
                            ));
                        } else if c.eq_ignore_ascii_case("a")
                            && self.view_mode == DesktopViewMode::Editor
                            && !self.search_bar_open
                        {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::SelectAll { buffer_id },
                            ));
                        } else if c.eq_ignore_ascii_case("d")
                            && self.view_mode == DesktopViewMode::Editor
                            && !self.search_bar_open
                        {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::AddNextOccurrence { buffer_id },
                            ));
                        } else if c == "/"
                            && self.view_mode == DesktopViewMode::Editor
                            && !self.search_bar_open
                        {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::ToggleLineComment { buffer_id },
                            ));
                        } else if c.eq_ignore_ascii_case("c")
                            && self.view_mode == DesktopViewMode::Editor
                            && !self.search_bar_open
                        {
                            if let Ok((txt, _)) = self.workspace.clipboard_text(buffer_id) {
                                self.internal_clipboard = txt.clone();
                                return iced::clipboard::write(txt);
                            }
                        } else if c.eq_ignore_ascii_case("x")
                            && self.view_mode == DesktopViewMode::Editor
                            && !self.search_bar_open
                        {
                            if let Ok((txt, _)) = self.workspace.clipboard_text(buffer_id) {
                                self.internal_clipboard = txt.clone();
                                let _ = self.update(DesktopMessage::DispatchEditorAction(
                                    EditorAction::DeleteSelectionOrLine { buffer_id },
                                ));
                                return iced::clipboard::write(txt);
                            }
                        } else if c.eq_ignore_ascii_case("v")
                            && self.view_mode == DesktopViewMode::Editor
                            && !self.search_bar_open
                        {
                            return iced::clipboard::read()
                                .map(DesktopMessage::ClipboardContentLoaded);
                        }
                    }
                }

                if self.palette_mode != CommandPaletteMode::Closed {
                    if key == Key::Named(Named::Escape) {
                        self.palette_mode = CommandPaletteMode::Closed;
                    }
                    return Task::none();
                }

                if self.view_mode == DesktopViewMode::Editor && !self.search_bar_open {
                    let select = modifiers.shift();
                    match key {
                        Key::Named(Named::ArrowUp) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction: CursorDirection::Up,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::ArrowDown) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction: CursorDirection::Down,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::ArrowLeft) => {
                            let direction = if cmd_or_ctrl {
                                CursorDirection::WordLeft
                            } else {
                                CursorDirection::Left
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::ArrowRight) => {
                            let direction = if cmd_or_ctrl {
                                CursorDirection::WordRight
                            } else {
                                CursorDirection::Right
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::PageUp) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MovePage {
                                    buffer_id,
                                    pages: -1,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::PageDown) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MovePage {
                                    buffer_id,
                                    pages: 1,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::Home) => {
                            let direction = if cmd_or_ctrl {
                                CursorDirection::DocumentStart
                            } else {
                                CursorDirection::LineStart
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::End) => {
                            let direction = if cmd_or_ctrl {
                                CursorDirection::DocumentEnd
                            } else {
                                CursorDirection::LineEnd
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::Backspace) => {
                            let action = if cmd_or_ctrl {
                                EditorAction::DeleteWordBackward { buffer_id }
                            } else {
                                EditorAction::DeleteBackward { buffer_id }
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(action));
                        }
                        Key::Named(Named::Delete) => {
                            let action = if cmd_or_ctrl {
                                EditorAction::DeleteWordForward { buffer_id }
                            } else {
                                EditorAction::DeleteForward { buffer_id }
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(action));
                        }
                        Key::Named(Named::Enter) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::InsertNewline { buffer_id },
                            ));
                        }
                        Key::Named(Named::Tab) => {
                            let action = if modifiers.shift() {
                                EditorAction::Outdent { buffer_id }
                            } else {
                                EditorAction::Indent { buffer_id }
                            };
                            return self.update(DesktopMessage::DispatchEditorAction(action));
                        }
                        Key::Named(Named::Escape) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::CollapseCursors { buffer_id },
                            ));
                        }
                        _ => {
                            if !cmd_or_ctrl {
                                if let Some(txt) = text_opt {
                                    if !txt.chars().any(|c| c.is_control()) {
                                        return self.update(DesktopMessage::DispatchEditorAction(
                                            EditorAction::InsertText {
                                                buffer_id,
                                                text: txt,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Task::none()
    }

    fn request_lsp_completions(&mut self) {
        self.lsp_completions.clear();
        if let (Some(mgr), Some(path), Some((line, utf16_col))) = (
            &self.lsp_manager,
            self.workspace.buffer_path(self.viewport.buffer_id),
            self.workspace.primary_cursor_utf16(self.viewport.buffer_id),
        ) {
            if let Some(Ok(entries)) = mgr.completion(&path, line, utf16_col).try_take() {
                self.lsp_completions = entries;
            }
        }
    }

    pub fn subscription(&self) -> Subscription<DesktopMessage> {
        let keys = iced::event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) => Some(DesktopMessage::KeyboardEvent(
                key,
                modifiers,
                text.map(|s| s.to_string()),
            )),
            _ => None,
        });
        let tick =
            iced::time::every(Duration::from_millis(450)).map(|_| DesktopMessage::BackgroundTick);
        Subscription::batch([keys, tick])
    }

    pub fn view(&self) -> Element<'_, DesktopMessage> {
        let is_editor = self.view_mode == DesktopViewMode::Editor;
        let is_agent = self.view_mode == DesktopViewMode::AgentManager;
        let search_open = self.search_bar_open;
        let quick_open = self.palette_mode == CommandPaletteMode::QuickOpenFile;
        let cmd_open = self.palette_mode == CommandPaletteMode::CommandRunner;

        let top_bar = container(
            row![
                text("IDE LEVITY")
                    .font(theme::MONO_BOLD)
                    .size(14)
                    .shaping(text::Shaping::Advanced)
                    .color(theme::ACCENT_BRIGHT),
                button(
                    text(if is_editor {
                        "• Editor View"
                    } else {
                        "Editor View"
                    })
                    .size(13)
                    .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| nav_btn_style(is_editor, status))
                .on_press(DesktopMessage::SwitchViewMode(DesktopViewMode::Editor)),
                button(
                    text(if is_agent {
                        format!("• Agent Manager ({})", self.viewport.agent_tasks.len())
                    } else {
                        format!("Agent Manager ({})", self.viewport.agent_tasks.len())
                    })
                    .size(13)
                    .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| nav_btn_style(is_agent, status))
                .on_press(DesktopMessage::SwitchViewMode(
                    DesktopViewMode::AgentManager
                )),
                horizontal_space(),
                button(text("Find (Ctrl+F)").size(12))
                    .style(move |_theme, status| nav_btn_style(search_open, status))
                    .on_press(DesktopMessage::ToggleSearchBar),
                button(text("Quick Open (Ctrl+P)").size(12))
                    .style(move |_theme, status| nav_btn_style(quick_open, status))
                    .on_press(DesktopMessage::ToggleCommandPalette(
                        CommandPaletteMode::QuickOpenFile
                    )),
                button(text("Commands (Ctrl+Shift+P)").size(12))
                    .style(move |_theme, status| nav_btn_style(cmd_open, status))
                    .on_press(DesktopMessage::ToggleCommandPalette(
                        CommandPaletteMode::CommandRunner
                    )),
            ]
            .spacing(8)
            .padding(8),
        )
        .width(Length::Fill)
        .style(theme::panel);

        let mut layout = Column::new().push(top_bar);
        if self.palette_mode != CommandPaletteMode::Closed {
            layout = layout.push(self.view_command_palette());
        }

        let main_body: Element<'_, DesktopMessage> = match self.view_mode {
            DesktopViewMode::Editor => self.view_editor_mode(),
            DesktopViewMode::AgentManager => self.view_agent_manager_mode(),
        };
        layout = layout.push(main_body);
        layout = layout.push(self.view_status_bar());

        container(layout)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(theme::editor_bg)
            .into()
    }

    fn view_editor_mode(&self) -> Element<'_, DesktopMessage> {
        let sidebar = self.view_sidebar();

        // Open Buffer Tabs Bar
        let mut tabs_row = Row::new().spacing(4).padding(4);
        for info in self.workspace.open_buffer_infos() {
            let tab_title = if info.is_dirty {
                format!("{} *", info.title)
            } else {
                info.title.clone()
            };
            let is_current = info.buffer_id == self.viewport.buffer_id;
            let tab_item = row![
                button(
                    text(if is_current {
                        format!("• {tab_title}")
                    } else {
                        tab_title
                    })
                    .size(12)
                    .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| tab_btn_style(is_current, status))
                .on_press(DesktopMessage::SelectBufferTab(info.buffer_id)),
                button(text("×").size(12))
                    .style(move |_theme, status| tab_btn_style(false, status))
                    .on_press(DesktopMessage::CloseBufferTab(info.buffer_id)),
            ]
            .spacing(2);
            tabs_row = tabs_row.push(tab_item);
        }
        tabs_row = tabs_row.push(
            button(text("+").size(12))
                .style(|_theme, status| tab_btn_style(false, status))
                .on_press(DesktopMessage::NewUntitledBuffer),
        );
        let tab_bar = container(tabs_row).width(Length::Fill).style(theme::panel);

        // Center Code Editor Canvas (`editor::EditorCanvas`)
        let current_match = (!self.search_matches.is_empty())
            .then(|| self.current_match_idx % self.search_matches.len());
        let raw_canvas: Element<'_, EditorEvent> = Canvas::new(EditorCanvas {
            vm: &self.viewport,
            metrics: self.editor_metrics,
            scroll_x: self.scroll_x,
            search_matches: &self.search_matches,
            current_match,
            focused: self.palette_mode == CommandPaletteMode::Closed && !self.search_bar_open,
            cursor_visible: self.cursor_visible,
            highlight: None,
        })
        .width(Length::Fill)
        .height(Length::FillPortion(3))
        .into();
        let editor_canvas = raw_canvas.map(DesktopMessage::EditorCanvasEvent);

        // Bottom Terminal / Diagnostics / Completions Panel
        let bottom_panel = self.view_bottom_panel();

        let mut center_col = Column::new().push(tab_bar);
        if self.search_bar_open {
            let search_bar = container(
                row![
                    text("Find:").size(12),
                    text_input("Search in buffer...", &self.search_query)
                        .on_input(DesktopMessage::SearchQueryChanged)
                        .on_submit(DesktopMessage::JumpToNextSearchMatch)
                        .font(theme::MONO)
                        .size(12),
                    text(format!("{} matches", self.search_matches.len())).size(12),
                    button(text("Next").size(12))
                        .style(|_theme, status| nav_btn_style(true, status))
                        .on_press(DesktopMessage::JumpToNextSearchMatch),
                    text("Replace:").size(12),
                    text_input("Replace with...", &self.replace_buffer_query)
                        .on_input(DesktopMessage::ReplaceBufferQueryChanged)
                        .on_submit(DesktopMessage::ReplaceAllInActiveBuffer)
                        .font(theme::MONO)
                        .size(12),
                    button(text("Replace All").size(12))
                        .style(|_theme, status| nav_btn_style(false, status))
                        .on_press(DesktopMessage::ReplaceAllInActiveBuffer),
                    button(text("×").size(12))
                        .style(|_theme, status| nav_btn_style(false, status))
                        .on_press(DesktopMessage::ToggleSearchBar),
                ]
                .spacing(6)
                .padding(6),
            )
            .width(Length::Fill)
            .style(theme::panel);
            center_col = center_col.push(search_bar);
        }
        center_col = center_col
            .push(editor_canvas)
            .push(bottom_panel)
            .width(Length::Fill)
            .height(Length::Fill);

        row![sidebar, center_col]
            .width(Length::Fill)
            .height(Length::FillPortion(1))
            .into()
    }

    fn view_sidebar(&self) -> Element<'_, DesktopMessage> {
        let is_exp = self.sidebar_tab == SidebarTab::Explorer;
        let is_srch = self.sidebar_tab == SidebarTab::Search;
        let is_git = self.sidebar_tab == SidebarTab::SourceControl;

        let tab_switcher = row![
            button(text("Files").size(11))
                .style(move |_theme, status| nav_btn_style(is_exp, status))
                .on_press(DesktopMessage::SelectSidebarTab(SidebarTab::Explorer)),
            button(text("Search").size(11))
                .style(move |_theme, status| nav_btn_style(is_srch, status))
                .on_press(DesktopMessage::SelectSidebarTab(SidebarTab::Search)),
            button(
                text(format!("Git ({})", self.git_status_entries.len())).size(11)
            )
            .style(move |_theme, status| nav_btn_style(is_git, status))
            .on_press(DesktopMessage::SelectSidebarTab(SidebarTab::SourceControl)),
        ]
        .spacing(4)
        .padding(6);

        let body: Element<'_, DesktopMessage> = match self.sidebar_tab {
            SidebarTab::Explorer => {
                let mut col = Column::new().spacing(2).padding(6);
                let new_row = row![
                    text_input("New file/folder path...", &self.new_item_name)
                        .on_input(DesktopMessage::NewFileNameChanged)
                        .on_submit(DesktopMessage::CreateNewFile)
                        .font(theme::MONO)
                        .size(11),
                    button(text("+F").size(11))
                        .style(|_theme, status| nav_btn_style(false, status))
                        .on_press(DesktopMessage::CreateNewFile),
                    button(text("+D").size(11))
                        .style(|_theme, status| nav_btn_style(false, status))
                        .on_press(DesktopMessage::CreateNewFolder),
                ]
                .spacing(3);
                col = col.push(new_row);

                for entry in self.visible_tree_entries().into_iter().take(120) {
                    let indent = "  ".repeat(entry.depth);
                    if entry.info.is_dir {
                        let arrow = if entry.is_expanded { "v" } else { ">" };
                        let label = format!("{indent}{arrow} {}/", entry.info.name);
                        let dir_path = entry.info.path.clone();
                        col = col.push(
                            button(
                                text(label)
                                    .font(theme::MONO_BOLD)
                                    .size(12)
                                    .shaping(text::Shaping::Advanced)
                                    .color(if entry.info.is_ignored {
                                        theme::TEXT_FAINT
                                    } else {
                                        theme::ACCENT_BRIGHT
                                    }),
                            )
                            .width(Length::Fill)
                            .style(|_theme, status| sidebar_btn_style(false, status))
                            .on_press(DesktopMessage::ToggleDirectory(dir_path)),
                        );
                    } else {
                        let rel = self.workspace.relative_path(&entry.info.path);
                        let is_active = self.viewport.file_path == rel
                            || self.viewport.file_path
                                == entry.info.path.to_string_lossy().as_ref();
                        let label = format!("{indent}  {}", entry.info.name);
                        col = col.push(
                            button(
                                text(label)
                                    .font(theme::MONO)
                                    .size(12)
                                    .shaping(text::Shaping::Advanced)
                                    .color(if entry.info.is_ignored {
                                        theme::TEXT_FAINT
                                    } else if is_active {
                                        theme::ACCENT_BRIGHT
                                    } else {
                                        theme::TEXT_SECONDARY
                                    }),
                            )
                            .width(Length::Fill)
                            .style(move |_theme, status| sidebar_btn_style(is_active, status))
                            .on_press(DesktopMessage::OpenFileFromSidebar(rel)),
                        );
                    }
                }
                scrollable(col).height(Length::Fill).into()
            }
            SidebarTab::Search => {
                let mut col = Column::new().spacing(4).padding(6);
                col = col.push(
                    text_input("Search in workspace...", &self.workspace_search_query)
                        .on_input(DesktopMessage::WorkspaceSearchQueryChanged)
                        .on_submit(DesktopMessage::RunWorkspaceSearch)
                        .font(theme::MONO)
                        .size(12),
                );
                col = col.push(
                    text_input("Replace in files...", &self.workspace_replace_query)
                        .on_input(DesktopMessage::WorkspaceReplaceQueryChanged)
                        .font(theme::MONO)
                        .size(12),
                );
                col = col.push(
                    row![
                        button(text("Search").size(11))
                            .style(|_theme, status| nav_btn_style(true, status))
                            .on_press(DesktopMessage::RunWorkspaceSearch),
                        button(text("Replace All").size(11))
                            .style(|_theme, status| nav_btn_style(false, status))
                            .on_press(DesktopMessage::RunWorkspaceReplaceAll),
                    ]
                    .spacing(4),
                );
                for fm in self.workspace_search_results.iter().take(30) {
                    let rel = self.workspace.relative_path(&fm.path);
                    col = col.push(
                        text(format!("{rel} ({})", fm.matches.len()))
                            .font(theme::MONO_BOLD)
                            .size(11)
                            .color(theme::ACCENT_BRIGHT),
                    );
                    for m in fm.matches.iter().take(5) {
                        let p = fm.path.clone();
                        let ln = m.line;
                        let col_num = m.ranges.first().map(|r| r.0).unwrap_or(0);
                        let preview = m.line_text.trim();
                        col = col.push(
                            button(
                                text(format!("  {ln}: {preview}"))
                                    .font(theme::MONO)
                                    .size(11)
                                    .shaping(text::Shaping::Advanced),
                            )
                            .width(Length::Fill)
                            .style(|_theme, status| sidebar_btn_style(false, status))
                            .on_press(DesktopMessage::OpenWorkspaceSearchHit(p, ln, col_num)),
                        );
                    }
                }
                scrollable(col).height(Length::Fill).into()
            }
            SidebarTab::SourceControl => {
                let mut col = Column::new().spacing(4).padding(6);
                col = col.push(
                    row![
                        text(format!("Branch: {}", self.git_branch))
                            .font(theme::MONO_BOLD)
                            .size(12)
                            .color(theme::ACCENT_BRIGHT),
                        horizontal_space(),
                        button(text("+ All").size(11))
                            .style(|_theme, status| nav_btn_style(false, status))
                            .on_press(DesktopMessage::GitStageAll),
                        button(text("Refresh").size(11))
                            .style(|_theme, status| nav_btn_style(false, status))
                            .on_press(DesktopMessage::GitRefresh),
                    ]
                    .spacing(4),
                );
                col = col.push(
                    text_input("Commit message...", &self.git_commit_message)
                        .on_input(DesktopMessage::GitCommitMessageChanged)
                        .on_submit(DesktopMessage::GitCommit)
                        .size(12),
                );
                col = col.push(
                    button(text("Commit Staged Changes").size(12))
                        .width(Length::Fill)
                        .style(|_theme, status| nav_btn_style(true, status))
                        .on_press(DesktopMessage::GitCommit),
                );
                if let Some(banner) = &self.git_status_banner {
                    col = col.push(text(banner).size(11).color(theme::SUCCESS));
                }
                for entry in &self.git_status_entries {
                    let p_open = entry.path.clone();
                    let p_stage = entry.path.clone();
                    let p_unstage = entry.path.clone();
                    let p_discard = entry.path.clone();
                    let badge = match (entry.index_status, entry.worktree_status) {
                        (Some(ChangeKind::Added), _) => "A ",
                        (Some(ChangeKind::Modified), _) => "M ",
                        (Some(ChangeKind::Deleted), _) => "D ",
                        (_, Some(ChangeKind::Untracked)) => "U ",
                        (_, Some(ChangeKind::Modified)) => " M",
                        (_, Some(ChangeKind::Deleted)) => " D",
                        _ => " ~",
                    };
                    let mut item_row = row![
                        button(
                            text(format!("{badge} {}", entry.path))
                                .font(theme::MONO)
                                .size(11)
                                .shaping(text::Shaping::Advanced)
                        )
                        .width(Length::Fill)
                        .style(|_theme, status| sidebar_btn_style(false, status))
                        .on_press(DesktopMessage::OpenFileFromSidebar(p_open)),
                    ]
                    .spacing(2);
                    if entry.worktree_status.is_some() {
                        item_row = item_row.push(
                            button(text("+").size(11))
                                .style(|_theme, status| nav_btn_style(false, status))
                                .on_press(DesktopMessage::GitStageFile(p_stage)),
                        );
                        if entry.worktree_status != Some(ChangeKind::Untracked) {
                            item_row = item_row.push(
                                button(text("x").size(11))
                                    .style(|_theme, status| nav_btn_style(false, status))
                                    .on_press(DesktopMessage::GitDiscardFile(p_discard)),
                            );
                        }
                    }
                    if entry.index_status.is_some() {
                        item_row = item_row.push(
                            button(text("-").size(11))
                                .style(|_theme, status| nav_btn_style(false, status))
                                .on_press(DesktopMessage::GitUnstageFile(p_unstage)),
                        );
                    }
                    col = col.push(item_row);
                }
                scrollable(col).height(Length::Fill).into()
            }
        };

        container(column![tab_switcher, body])
            .width(Length::Fixed(260.0))
            .height(Length::Fill)
            .style(theme::panel)
            .into()
    }

    fn view_bottom_panel(&self) -> Element<'_, DesktopMessage> {
        let word_completions = self.workspace.completions(self.viewport.buffer_id);
        let total_completions = if !self.lsp_completions.is_empty() {
            self.lsp_completions.len()
        } else {
            word_completions.len()
        };
        let is_term = self.bottom_tab == BottomPanelTab::Terminal;
        let is_diag = self.bottom_tab == BottomPanelTab::Diagnostics;
        let is_comp = self.bottom_tab == BottomPanelTab::Completions;

        let bottom_header = container(
            row![
                button(
                    text(if is_term { "• TERMINAL" } else { "TERMINAL" })
                        .size(12)
                        .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| nav_btn_style(is_term, status))
                .on_press(DesktopMessage::SelectBottomTab(BottomPanelTab::Terminal)),
                button(
                    text(if is_diag {
                        format!("• DIAGNOSTICS ({})", self.viewport.diagnostics.len())
                    } else {
                        format!("DIAGNOSTICS ({})", self.viewport.diagnostics.len())
                    })
                    .size(12)
                    .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| nav_btn_style(is_diag, status))
                .on_press(DesktopMessage::SelectBottomTab(BottomPanelTab::Diagnostics)),
                button(
                    text(if is_comp {
                        format!("• COMPLETIONS ({total_completions})")
                    } else {
                        format!("COMPLETIONS ({total_completions})")
                    })
                    .size(12)
                    .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| nav_btn_style(is_comp, status))
                .on_press(DesktopMessage::SelectBottomTab(BottomPanelTab::Completions)),
                horizontal_space(),
                button(text("Go to Definition (F12)").size(11))
                    .style(|_theme, status| nav_btn_style(false, status))
                    .on_press(DesktopMessage::GotoDefinitionAtCursor),
            ]
            .spacing(8)
            .padding(4),
        )
        .width(Length::Fill)
        .style(theme::panel);

        let bottom_content: Element<'_, DesktopMessage> = match self.bottom_tab {
            BottomPanelTab::Terminal => {
                let mut term_lines_col = Column::new().spacing(2);
                for line in self.terminal.lock().visible_lines() {
                    term_lines_col = term_lines_col.push(
                        text(line)
                            .font(theme::MONO)
                            .size(12)
                            .shaping(text::Shaping::Advanced)
                            .color(theme::SUCCESS),
                    );
                }
                let input_row = row![
                    text("$").font(theme::MONO).size(13),
                    text_input("Run shell command in workspace...", &self.terminal_input)
                        .on_input(DesktopMessage::TerminalCommandChanged)
                        .on_submit(DesktopMessage::ExecuteTerminalCommand)
                        .font(theme::MONO)
                        .size(13),
                    button(text("Run").size(12))
                        .style(|_theme, status| nav_btn_style(true, status))
                        .on_press(DesktopMessage::ExecuteTerminalCommand),
                ]
                .spacing(6);
                column![scrollable(term_lines_col).height(Length::Fill), input_row]
                    .spacing(4)
                    .padding(6)
                    .into()
            }
            BottomPanelTab::Diagnostics => {
                let mut diag_col = Column::new().spacing(4).padding(6);
                if self.viewport.diagnostics.is_empty() {
                    diag_col = diag_col.push(
                        text("OK: Zero syntax or LSP errors in active buffer.")
                            .size(13)
                            .shaping(text::Shaping::Advanced)
                            .color(theme::SUCCESS),
                    );
                } else {
                    for d in &self.viewport.diagnostics {
                        diag_col = diag_col.push(
                            text(format!(
                                "! [Ln {}, Col {}] {}",
                                d.line + 1,
                                d.start_col + 1,
                                d.message
                            ))
                            .font(theme::MONO)
                            .size(13)
                            .shaping(text::Shaping::Advanced)
                            .color(editor::severity_color(d.severity)),
                        );
                    }
                }
                scrollable(diag_col).height(Length::Fill).into()
            }
            BottomPanelTab::Completions => {
                let mut comp_row = Row::new().spacing(6).padding(6);
                if !self.lsp_completions.is_empty() {
                    for entry in self.lsp_completions.iter().take(15) {
                        let insert_val = entry.insert_text.clone();
                        let label = entry.label.clone();
                        comp_row = comp_row.push(
                            button(text(label).font(theme::MONO).size(12))
                                .style(|_theme, status| nav_btn_style(false, status))
                                .on_press(DesktopMessage::InsertCompletionItem(insert_val)),
                        );
                    }
                } else if word_completions.is_empty() {
                    comp_row = comp_row.push(
                        button(text("Trigger Completion (Ctrl+Space)").size(12))
                            .style(|_theme, status| nav_btn_style(false, status))
                            .on_press(DesktopMessage::DispatchEditorAction(
                                EditorAction::TriggerLspCompletion {
                                    buffer_id: self.viewport.buffer_id,
                                },
                            )),
                    );
                } else {
                    for item in word_completions.into_iter().take(15) {
                        let insert_val = item.clone();
                        comp_row = comp_row.push(
                            button(text(item).font(theme::MONO).size(12))
                                .style(|_theme, status| nav_btn_style(false, status))
                                .on_press(DesktopMessage::InsertCompletionItem(insert_val)),
                        );
                    }
                }
                scrollable(comp_row).height(Length::Fill).into()
            }
        };

        container(column![bottom_header, bottom_content])
            .width(Length::Fill)
            .height(Length::FillPortion(1))
            .style(theme::editor_bg)
            .into()
    }

    fn view_agent_manager_mode(&self) -> Element<'_, DesktopMessage> {
        let provider_badge = match &self.ai_config {
            Some(cfg) => format!("AI Provider: {} ({})", cfg.provider.display_name(), cfg.model),
            None => "Engine: Deterministic Patch Mode (set OPENAI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY or run Ollama for LLM mode)".to_string(),
        };

        let prompt_bar = container(
            column![
                text(provider_badge).size(11).color(theme::ACCENT_BRIGHT),
                row![
                    text_input(
                        "Dispatch AI Agent task (e.g. `old -> new`, `doc: Core module`, or natural language instruction)...",
                        &self.agent_prompt_input
                    )
                    .on_input(DesktopMessage::AgentPromptChanged)
                    .on_submit(DesktopMessage::SubmitAgentPrompt)
                    .size(13),
                    button(text("Spawn Agent Task").size(13))
                        .style(|_theme, status| nav_btn_style(true, status))
                        .on_press(DesktopMessage::SubmitAgentPrompt),
                ]
                .spacing(8),
            ]
            .spacing(4)
            .padding(10),
        )
        .width(Length::Fill)
        .style(theme::panel);

        let mut task_list_col = Column::new().spacing(8).padding(10);
        task_list_col = task_list_col.push(
            text("PARALLEL AGENT TASKS")
                .size(13)
                .color(theme::ACCENT_BRIGHT),
        );
        if self.viewport.agent_tasks.is_empty() {
            task_list_col = task_list_col.push(
                text("No agent tasks submitted yet. Enter a prompt above to generate a Unified Diff patch.")
                    .size(13)
                    .color(theme::TEXT_MUTED),
            );
        } else {
            for t in &self.viewport.agent_tasks {
                let badge = if t.verification_passed {
                    "Verified"
                } else {
                    "Pending"
                };
                let is_selected = self.selected_agent_task_id == Some(t.task_id)
                    || (self.selected_agent_task_id.is_none()
                        && self.viewport.agent_tasks.last().map(|x| x.task_id) == Some(t.task_id));
                let item_row = column![
                    button(
                        text(format!("#{} {} [{} | {}]", t.task_id, t.prompt, t.state, badge))
                            .size(13)
                            .shaping(text::Shaping::Advanced)
                    )
                    .width(Length::Fill)
                    .style(move |_theme, status| nav_btn_style(is_selected, status))
                    .on_press(DesktopMessage::SelectAgentTask(t.task_id)),
                    row![
                        button(
                            text("+ Accept (Chấp nhận)")
                                .size(12)
                                .shaping(text::Shaping::Advanced)
                        )
                        .style(|_theme, status| accept_btn_style(status))
                        .on_press(DesktopMessage::AcceptAgentTask(t.task_id)),
                        button(
                            text("× Reject (Từ chối)")
                                .size(12)
                                .shaping(text::Shaping::Advanced)
                        )
                        .style(|_theme, status| reject_btn_style(status))
                        .on_press(DesktopMessage::RejectAgentTask(t.task_id)),
                    ]
                    .spacing(6),
                ]
                .spacing(4);
                task_list_col = task_list_col.push(item_row);
            }
        }

        let active_task = self
            .selected_agent_task_id
            .and_then(|id| self.viewport.agent_tasks.iter().find(|t| t.task_id == id))
            .or_else(|| self.viewport.agent_tasks.last());

        let mut diff_col = Column::new().spacing(4).padding(12);
        diff_col = diff_col.push(
            text("UNIFIED DIFF PREVIEW")
                .size(13)
                .color(theme::ACCENT_BRIGHT),
        );
        if let Some(task) = active_task {
            if let Some(diff_text) = &task.unified_diff_preview {
                for line in diff_text.lines() {
                    let color = if line.starts_with('+') && !line.starts_with("+++") {
                        theme::SUCCESS
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        theme::DANGER
                    } else if line.starts_with("@@") {
                        theme::ACCENT_BRIGHT
                    } else {
                        theme::TEXT_SECONDARY
                    };
                    diff_col = diff_col.push(
                        text(line)
                            .font(theme::MONO)
                            .size(13)
                            .shaping(text::Shaping::Advanced)
                            .color(color),
                    );
                }
            } else {
                diff_col = diff_col.push(text("Task is still generating patch...").size(13));
            }
        } else {
            diff_col = diff_col.push(
                text("Select or spawn an Agent Task to preview its Unified Diff.")
                    .size(13)
                    .color(theme::TEXT_MUTED),
            );
        }

        let split = Row::new()
            .push(
                container(scrollable(task_list_col))
                    .width(Length::FillPortion(1))
                    .height(Length::Fill)
                    .style(theme::panel),
            )
            .push(
                container(scrollable(diff_col))
                    .width(Length::FillPortion(2))
                    .height(Length::Fill)
                    .style(theme::editor_bg),
            );

        column![prompt_bar, split]
            .width(Length::Fill)
            .height(Length::FillPortion(1))
            .into()
    }

    fn view_command_palette(&self) -> Element<'_, DesktopMessage> {
        let placeholder = match self.palette_mode {
            CommandPaletteMode::QuickOpenFile => "Search files by name (Ctrl+P)...",
            CommandPaletteMode::CommandRunner => "Type a command to run (Ctrl+Shift+P)...",
            CommandPaletteMode::Closed => "",
        };

        let mut col = Column::new().spacing(4).padding(10);
        col = col.push(
            text_input(placeholder, &self.palette_query)
                .on_input(DesktopMessage::CommandPaletteQueryChanged)
                .on_submit(DesktopMessage::ExecutePaletteSelection(0))
                .size(13),
        );

        match self.palette_mode {
            CommandPaletteMode::QuickOpenFile => {
                for (idx, file) in self.filtered_palette_files().into_iter().enumerate() {
                    let is_first = idx == 0;
                    col = col.push(
                        button(text(file).font(theme::MONO).size(12))
                            .width(Length::Fill)
                            .style(move |_theme, status| sidebar_btn_style(is_first, status))
                            .on_press(DesktopMessage::ExecutePaletteSelection(idx)),
                    );
                }
            }
            CommandPaletteMode::CommandRunner => {
                for (idx, cmd) in self.filtered_palette_commands().into_iter().enumerate() {
                    let is_first = idx == 0;
                    col = col.push(
                        button(text(cmd.label()).size(12))
                            .width(Length::Fill)
                            .style(move |_theme, status| sidebar_btn_style(is_first, status))
                            .on_press(DesktopMessage::ExecutePaletteSelection(idx)),
                    );
                }
            }
            CommandPaletteMode::Closed => {}
        }

        container(col)
            .width(Length::Fill)
            .style(theme::overlay)
            .into()
    }

    fn view_status_bar(&self) -> Element<'_, DesktopMessage> {
        let primary_cursor = self.viewport.cursors.first();
        let ln = primary_cursor.map(|c| c.line + 1).unwrap_or(1);
        let col = primary_cursor.map(|c| c.column + 1).unwrap_or(1);
        let cursor_count = self.viewport.cursors.len();
        let lang = self
            .workspace
            .buffer_language(self.viewport.buffer_id)
            .unwrap_or(SyntaxLanguage::Rust);
        let mode_str = match self.view_mode {
            DesktopViewMode::Editor => "EDITOR",
            DesktopViewMode::AgentManager => "AGENT MANAGER",
        };

        container(
            row![
                text(format!(" {mode_str} "))
                    .size(11)
                    .color(theme::ACCENT_BRIGHT),
                text(format!("git: {}", self.git_branch))
                    .size(11)
                    .color(theme::TEXT_SECONDARY),
                text(&self.lsp_status_text)
                    .size(11)
                    .color(theme::TEXT_MUTED),
                text(format!("Diagnostics: {}", self.viewport.diagnostics.len()))
                    .size(11)
                    .color(theme::TEXT_MUTED),
                horizontal_space(),
                text(format!(
                    "Ln {ln}, Col {col} ({cursor_count} cursor{})",
                    if cursor_count > 1 { "s" } else { "" }
                ))
                .size(11)
                .color(theme::TEXT_SECONDARY),
                text(format!("{lang:?}")).size(11).color(theme::TEXT_MUTED),
                text("UTF-8 | 120fps").size(11).color(theme::SUCCESS),
            ]
            .spacing(16)
            .padding(4),
        )
        .width(Length::Fill)
        .style(theme::status_bar)
        .into()
    }
}

fn nav_btn_style(is_active: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let bg = if is_active {
        if hovered {
            Color::from_rgb8(0x38, 0x8B, 0xFD)
        } else {
            Color::from_rgb8(0x1F, 0x6F, 0xEB)
        }
    } else if hovered {
        Color::from_rgb8(0x30, 0x36, 0x3D)
    } else {
        Color::from_rgb8(0x21, 0x26, 0x2D)
    };
    let border_color = if is_active {
        Color::from_rgb8(0x58, 0xA6, 0xFF)
    } else {
        Color::from_rgb8(0x30, 0x36, 0x3D)
    };
    button::Style {
        background: Some(bg.into()),
        text_color: if is_active {
            Color::WHITE
        } else {
            Color::from_rgb8(0xC9, 0xD1, 0xD9)
        },
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

fn sidebar_btn_style(is_active: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let bg = if is_active {
        Color::from_rgb8(0x1F, 0x29, 0x37)
    } else if hovered {
        Color::from_rgb8(0x21, 0x26, 0x2D)
    } else {
        Color::from_rgb8(0x16, 0x1B, 0x22)
    };
    button::Style {
        background: Some(bg.into()),
        text_color: if is_active {
            Color::from_rgb8(0x58, 0xA6, 0xFF)
        } else {
            Color::from_rgb8(0xC9, 0xD1, 0xD9)
        },
        border: Border {
            color: if is_active {
                Color::from_rgb8(0x38, 0x8B, 0xFD)
            } else {
                Color::TRANSPARENT
            },
            width: if is_active { 1.0 } else { 0.0 },
            radius: 3.0.into(),
        },
        ..Default::default()
    }
}

fn tab_btn_style(is_active: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let bg = if is_active {
        Color::from_rgb8(0x0D, 0x11, 0x17)
    } else if hovered {
        Color::from_rgb8(0x21, 0x26, 0x2D)
    } else {
        Color::from_rgb8(0x16, 0x1B, 0x22)
    };
    button::Style {
        background: Some(bg.into()),
        text_color: if is_active {
            Color::from_rgb8(0x58, 0xA6, 0xFF)
        } else {
            Color::from_rgb8(0x8B, 0x94, 0x9E)
        },
        border: Border {
            color: if is_active {
                Color::from_rgb8(0x58, 0xA6, 0xFF)
            } else {
                Color::from_rgb8(0x30, 0x36, 0x3D)
            },
            width: 1.0,
            radius: 3.0.into(),
        },
        ..Default::default()
    }
}

fn accept_btn_style(status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(
            if hovered {
                Color::from_rgb8(0x2E, 0xA0, 0x43)
            } else {
                Color::from_rgb8(0x23, 0x86, 0x36)
            }
            .into(),
        ),
        text_color: Color::WHITE,
        border: Border {
            color: Color::from_rgb8(0x3F, 0xB9, 0x50),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

fn reject_btn_style(status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: Some(
            if hovered {
                Color::from_rgb8(0xF8, 0x51, 0x49)
            } else {
                Color::from_rgb8(0xDA, 0x36, 0x33)
            }
            .into(),
        ),
        text_color: Color::WHITE,
        border: Border {
            color: Color::from_rgb8(0xF8, 0x51, 0x49),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

pub fn rgba_u32_to_iced_color(rgba: u32) -> Color {
    editor::rgba_to_color(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dual_view_switching_and_command_palette() {
        let mut app = IdeDesktopApp::new(".");
        assert_eq!(app.view_mode, DesktopViewMode::Editor);

        let _ = app.update(DesktopMessage::ToggleCommandPalette(
            CommandPaletteMode::CommandRunner,
        ));
        assert_eq!(app.palette_mode, CommandPaletteMode::CommandRunner);

        let _ = app.update(DesktopMessage::CommandPaletteQueryChanged(
            "Agent Manager".to_string(),
        ));
        let cmds = app.filtered_palette_commands();
        assert_eq!(cmds, vec![PaletteCommand::SwitchToAgentManagerView]);

        let _ = app.update(DesktopMessage::ExecutePaletteSelection(0));
        assert_eq!(app.view_mode, DesktopViewMode::AgentManager);
        assert_eq!(app.palette_mode, CommandPaletteMode::Closed);
    }

    #[test]
    fn test_agent_manager_view_diff_preview_accept_and_reject() {
        let mut app = IdeDesktopApp::new(".");
        let buf_id = app.viewport.buffer_id;
        let _ = app.update(DesktopMessage::DispatchEditorAction(
            EditorAction::InsertText {
                buffer_id: buf_id,
                text: "fn banner() { println!(\"IDE Levity\"); }\n".to_string(),
            },
        ));
        let _ = app.update(DesktopMessage::SwitchViewMode(DesktopViewMode::AgentManager));
        let _ = app.update(DesktopMessage::AgentPromptChanged(
            "IDE Levity -> IDE Levity Pro".to_string(),
        ));
        let _ = app.update(DesktopMessage::SubmitAgentPrompt);

        assert_eq!(app.viewport.agent_tasks.len(), 1);
        let task_id = app.viewport.agent_tasks[0].task_id;
        assert!(app.viewport.agent_tasks[0]
            .unified_diff_preview
            .as_ref()
            .unwrap()
            .contains("IDE Levity Pro"));

        let _ = app.update(DesktopMessage::AcceptAgentTask(task_id));
        assert!(app
            .viewport
            .visible_lines
            .iter()
            .any(|l| l.text.contains("IDE Levity Pro")));

        let _ = app.update(DesktopMessage::RejectAgentTask(task_id));
        assert!(!app
            .viewport
            .visible_lines
            .iter()
            .any(|l| l.text.contains("IDE Levity Pro")));
    }

    #[test]
    fn test_search_bar_workspace_search_and_real_terminal_execution() {
        let mut app = IdeDesktopApp::new(".");
        let buf_id = app.viewport.buffer_id;
        let _ = app.update(DesktopMessage::DispatchEditorAction(
            EditorAction::InsertText {
                buffer_id: buf_id,
                text: "struct ProjectConfig;\nimpl ProjectConfig {}\n".to_string(),
            },
        ));
        let _ = app.update(DesktopMessage::ToggleSearchBar);
        assert!(app.search_bar_open);
        let _ = app.update(DesktopMessage::SearchQueryChanged("ProjectConfig".to_string()));
        assert_eq!(app.search_matches.len(), 2);
        let _ = app.update(DesktopMessage::JumpToNextSearchMatch);
        assert_eq!(app.viewport.cursors[0].line, 0);

        // Execute a real shell command in the terminal emulator
        let _ = app.update(DesktopMessage::TerminalCommandChanged(
            "echo LEVITY_TERM_OK".to_string(),
        ));
        let _ = app.update(DesktopMessage::ExecuteTerminalCommand);
        let term_lines = app.terminal.lock().visible_lines();
        assert!(term_lines.iter().any(|l| l.contains("LEVITY_TERM_OK")));

        // Verify hierarchical tree explorer entries
        let tree = app.visible_tree_entries();
        assert!(!tree.is_empty());
    }

    #[test]
    fn test_cosmic_text_shaper_and_alacritty_terminal() {
        let mut shaper = CosmicTextShaper::default();
        let w = shaper.measure_line_width("fn main() {}");
        assert!(w > 0.0);
        let col_x = shaper.column_x_offset("fn main() {}", 3);
        assert!(col_x > 0.0);

        let mut term = TerminalEmulator::default();
        term.feed_bytes(b"cargo test --workspace\r\ntest result: ok.\r\n");
        let lines = term.visible_lines();
        assert!(lines.iter().any(|l| l.contains("test result: ok.")));
    }

    #[test]
    fn test_workspace_search_git_sidebar_and_ai_edit_completion() {
        let mut app = IdeDesktopApp::new(".");
        let _ = app.view();
        let _ = app.update(DesktopMessage::SelectSidebarTab(SidebarTab::Search));
        assert_eq!(app.sidebar_tab, SidebarTab::Search);
        let _ = app.view();
        let _ = app.update(DesktopMessage::WorkspaceSearchQueryChanged(
            "IdeWorkspace".to_string(),
        ));
        let _ = app.update(DesktopMessage::RunWorkspaceSearch);
        assert!(!app.workspace_search_results.is_empty());
        let _ = app.view();

        let _ = app.update(DesktopMessage::SelectSidebarTab(SidebarTab::SourceControl));
        assert_eq!(app.sidebar_tab, SidebarTab::SourceControl);
        let _ = app.view();

        // Simulate async AI EditProposal completion
        let buf_id = app.viewport.buffer_id;
        let orig = app.workspace.buffer_text(buf_id).unwrap_or_default();
        let task_id = app
            .workspace
            .agent_manager()
            .start_running_task("Upgrade version".to_string(), "Cargo.toml".to_string());
        app.workspace.bind_agent_task(task_id, buf_id);
        let proposal = EditProposal {
            new_text: format!("{orig}\n# AI Reviewed\n"),
            explanation: "Added AI Reviewed footer".to_string(),
            raw_response: String::new(),
            blocks_applied: 1,
        };
        let _ = app.update(DesktopMessage::AiEditCompleted {
            task_id,
            buffer_id: buf_id,
            file_path: "Cargo.toml".to_string(),
            original_text: orig,
            result: Ok(proposal),
        });
        assert!(app
            .viewport
            .agent_tasks
            .iter()
            .any(|t| t.task_id == task_id && t.unified_diff_preview.is_some()));
        let _ = app.update(DesktopMessage::SwitchViewMode(DesktopViewMode::AgentManager));
        let _ = app.view();
    }
}
