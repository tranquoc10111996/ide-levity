//! `ide-desktop`: Cross-platform Desktop IDE (macOS, Windows, Linux) built with `iced`,
//! `cosmic-text`, and `alacritty_terminal`.
//! Features Antigravity-inspired Dual-View architecture:
//! 1. **Editor View**: File tree sidebar, multi-buffer tab bar, toggleable Find bar (`Ctrl+F`),
//!    custom 120fps Canvas code editor (active-line highlight, selection rectangles, search highlights,
//!    diagnostic underlines, line numbers, Git gutter, syntax spans, multi-cursors, minimap),
//!    bottom Terminal/Diagnostics/Completions panel, status bar, and Command Palette (`Ctrl+P` / `Ctrl+Shift+P`).
//! 2. **Agent Manager View**: Parallel AI Agent task orchestration with visual Unified Diff
//!    preview and one-click Accept / Reject controls.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column as TermColumn, Line as TermLine, Point as TermPoint};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor as AnsiProcessor;
use cosmic_text::{Attrs, Buffer as CosmicBuffer, Family, FontSystem, Metrics, Shaping};
use iced::keyboard::{self, key::Named, Key, Modifiers};
use iced::mouse;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke, Text as CanvasText};
use iced::widget::{
    button, column, container, horizontal_space, row, scrollable, text, text_input, Column, Row,
};
use iced::{
    Border, Color, Element, Event, Font, Length, Point, Rectangle, Renderer, Size, Subscription,
    Task, Theme,
};
use ide_core::{
    CursorDirection, DiffMarkerKind, EditorAction, IdeWorkspace, SearchMatch, SyntaxLanguage,
    ViewportViewModel,
};
use parking_lot::Mutex;
use std::process::Command as ProcessCommand;
use std::sync::Arc;

/// Active top-level view in the Dual-View Desktop IDE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopViewMode {
    Editor,
    AgentManager,
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
    ToggleFindBar,
    SaveActiveFile,
    UndoEdit,
    RedoEdit,
    TriggerCompletion,
}

impl PaletteCommand {
    pub fn all() -> &'static [PaletteCommand] {
        &[
            Self::SwitchToEditorView,
            Self::SwitchToAgentManagerView,
            Self::ToggleFindBar,
            Self::SaveActiveFile,
            Self::UndoEdit,
            Self::RedoEdit,
            Self::TriggerCompletion,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SwitchToEditorView => "View: Switch to Editor View",
            Self::SwitchToAgentManagerView => "View: Switch to Agent Manager View",
            Self::ToggleFindBar => "Edit: Find in Current Buffer (Ctrl+F)",
            Self::SaveActiveFile => "File: Save Active Buffer (Ctrl+S)",
            Self::UndoEdit => "Edit: Undo Last Transaction (Ctrl+Z)",
            Self::RedoEdit => "Edit: Redo Last Transaction (Ctrl+Y)",
            Self::TriggerCompletion => "LSP: Trigger Code Completion (Ctrl+Space)",
        }
    }
}

/// Messages handled by the `IdeDesktopApp` Unidirectional state machine.
#[derive(Debug, Clone)]
pub enum DesktopMessage {
    SwitchViewMode(DesktopViewMode),
    SelectBottomTab(BottomPanelTab),
    OpenFileFromSidebar(String),
    SelectBufferTab(u64),
    CloseBufferTab(u64),
    DispatchEditorAction(EditorAction),
    AddMultiCursor(u32, u32),
    ToggleSearchBar,
    SearchQueryChanged(String),
    JumpToNextSearchMatch,
    ToggleCommandPalette(CommandPaletteMode),
    CommandPaletteQueryChanged(String),
    ExecutePaletteSelection(usize),
    InsertCompletionItem(String),
    AgentPromptChanged(String),
    SubmitAgentPrompt,
    SelectAgentTask(u64),
    AcceptAgentTask(u64),
    RejectAgentTask(u64),
    TerminalCommandChanged(String),
    ExecuteTerminalCommand,
    KeyboardEvent(Key, Modifiers, Option<String>),
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
        emu.feed_bytes(b"IDE Levity Terminal (alacritty_terminal 0.24) - Ready\r\n$ ");
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
        Self::new(14.0, 20.0)
    }
}

impl CosmicTextShaper {
    pub fn new(font_size: f32, line_height: f32) -> Self {
        let mut font_system = FontSystem::new();
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
            Attrs::new().family(Family::Monospace),
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
    pub bottom_tab: BottomPanelTab,
    pub palette_mode: CommandPaletteMode,
    pub palette_query: String,
    pub search_bar_open: bool,
    pub search_query: String,
    pub search_matches: Vec<SearchMatch>,
    pub current_match_idx: usize,
    pub agent_prompt_input: String,
    pub selected_agent_task_id: Option<u64>,
    pub terminal_input: String,
    pub terminal: Arc<Mutex<TerminalEmulator>>,
    pub shaper: Arc<Mutex<CosmicTextShaper>>,
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
        let workspace = Arc::new(IdeWorkspace::new(root_dir));
        let initial_id = workspace.active_buffer_id();
        let workspace_files = workspace.list_workspace_files();
        if workspace_files.iter().any(|f| f == "Cargo.toml") {
            let _ = workspace.dispatch(EditorAction::OpenFile {
                path: "Cargo.toml".to_string(),
            });
            let _ = workspace.set_active_buffer_id(initial_id);
        }
        let viewport = workspace
            .active_viewport()
            .expect("Default buffer viewport must exist");
        let git_branch = workspace
            .current_git_branch()
            .unwrap_or_else(|| "main".to_string());

        Self {
            workspace,
            viewport,
            view_mode: DesktopViewMode::Editor,
            bottom_tab: BottomPanelTab::Terminal,
            palette_mode: CommandPaletteMode::Closed,
            palette_query: String::new(),
            search_bar_open: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            current_match_idx: 0,
            agent_prompt_input: String::new(),
            selected_agent_task_id: None,
            terminal_input: String::new(),
            terminal: Arc::new(Mutex::new(TerminalEmulator::default())),
            shaper: Arc::new(Mutex::new(CosmicTextShaper::default())),
            workspace_files,
            git_branch,
        }
    }

    pub fn title(&self) -> String {
        format!(
            "IDE Levity - {} [{}]",
            self.viewport.file_path, self.git_branch
        )
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

    /// Returns filtered file paths when the Command Palette is in `QuickOpenFile` mode.
    pub fn filtered_palette_files(&self) -> Vec<String> {
        let q = self.palette_query.trim().to_lowercase();
        self.workspace_files
            .iter()
            .filter(|f| q.is_empty() || f.to_lowercase().contains(&q))
            .take(12)
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
        match message {
            DesktopMessage::SwitchViewMode(mode) => {
                self.view_mode = mode;
            }
            DesktopMessage::SelectBottomTab(tab) => {
                self.bottom_tab = tab;
            }
            DesktopMessage::OpenFileFromSidebar(path) => {
                if let Ok(vm) = self.workspace.dispatch(EditorAction::OpenFile { path }) {
                    self.viewport = vm;
                    self.refresh_search_matches();
                }
                self.palette_mode = CommandPaletteMode::Closed;
            }
            DesktopMessage::SelectBufferTab(buffer_id) => {
                if let Ok(vm) = self.workspace.set_active_buffer_id(buffer_id) {
                    self.viewport = vm;
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::CloseBufferTab(buffer_id) => {
                if let Ok(vm) = self.workspace.close_buffer(buffer_id) {
                    self.viewport = vm;
                    self.refresh_search_matches();
                }
            }
            DesktopMessage::DispatchEditorAction(action) => {
                let is_completion = matches!(action, EditorAction::TriggerLspCompletion { .. });
                if let Ok(vm) = self.workspace.dispatch(action) {
                    self.viewport = vm;
                    self.refresh_search_matches();
                    if is_completion {
                        self.bottom_tab = BottomPanelTab::Completions;
                    }
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
            DesktopMessage::JumpToNextSearchMatch => {
                if !self.search_matches.is_empty() {
                    let m = &self.search_matches[self.current_match_idx % self.search_matches.len()];
                    let line = m.line;
                    let column = m.column;
                    self.current_match_idx = (self.current_match_idx + 1) % self.search_matches.len();
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
                match self.palette_mode {
                    CommandPaletteMode::QuickOpenFile => {
                        let files = self.filtered_palette_files();
                        if let Some(chosen) = files.get(idx) {
                            if let Ok(vm) = self.workspace.dispatch(EditorAction::OpenFile {
                                path: chosen.clone(),
                            }) {
                                self.viewport = vm;
                                self.refresh_search_matches();
                            }
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
                                PaletteCommand::ToggleFindBar => {
                                    self.search_bar_open = !self.search_bar_open;
                                }
                                PaletteCommand::SaveActiveFile => {
                                    if let Ok(vm) = self
                                        .workspace
                                        .dispatch(EditorAction::SaveFile { buffer_id: buf_id })
                                    {
                                        self.viewport = vm;
                                    }
                                }
                                PaletteCommand::UndoEdit => {
                                    if let Ok(vm) = self
                                        .workspace
                                        .dispatch(EditorAction::Undo { buffer_id: buf_id })
                                    {
                                        self.viewport = vm;
                                    }
                                }
                                PaletteCommand::RedoEdit => {
                                    if let Ok(vm) = self
                                        .workspace
                                        .dispatch(EditorAction::Redo { buffer_id: buf_id })
                                    {
                                        self.viewport = vm;
                                    }
                                }
                                PaletteCommand::TriggerCompletion => {
                                    if let Ok(vm) = self.workspace.dispatch(
                                        EditorAction::TriggerLspCompletion { buffer_id: buf_id },
                                    ) {
                                        self.viewport = vm;
                                        self.bottom_tab = BottomPanelTab::Completions;
                                    }
                                }
                            }
                        }
                    }
                    CommandPaletteMode::Closed => {}
                }
                self.palette_mode = CommandPaletteMode::Closed;
            }
            DesktopMessage::InsertCompletionItem(item) => {
                if let Ok(vm) = self.workspace.dispatch(EditorAction::InsertText {
                    buffer_id: self.viewport.buffer_id,
                    text: item,
                }) {
                    self.viewport = vm;
                }
            }
            DesktopMessage::AgentPromptChanged(val) => {
                self.agent_prompt_input = val;
            }
            DesktopMessage::SubmitAgentPrompt => {
                let prompt = self.agent_prompt_input.trim().to_string();
                if !prompt.is_empty() {
                    if let Ok(vm) = self
                        .workspace
                        .dispatch(EditorAction::SubmitAgentPrompt { prompt })
                    {
                        self.selected_agent_task_id = vm.agent_tasks.last().map(|t| t.task_id);
                        self.viewport = vm;
                    }
                    self.agent_prompt_input.clear();
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
                    let banner = format!("{cmd}\r\n");
                    self.terminal.lock().feed_bytes(banner.as_bytes());
                    let shell_out = ProcessCommand::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .current_dir(self.workspace.root_dir())
                        .output();
                    match shell_out {
                        Ok(out) => {
                            for line in String::from_utf8_lossy(&out.stdout).lines().take(12) {
                                let formatted = format!("{line}\r\n");
                                self.terminal.lock().feed_bytes(formatted.as_bytes());
                            }
                            for line in String::from_utf8_lossy(&out.stderr).lines().take(6) {
                                let formatted = format!("{line}\r\n");
                                self.terminal.lock().feed_bytes(formatted.as_bytes());
                            }
                            let done = format!("[completed: {cmd} (exit {})]\r\n$ ", out.status.code().unwrap_or(0));
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
                if cmd_or_ctrl {
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
                            return self.update(DesktopMessage::ToggleSearchBar);
                        } else if c.eq_ignore_ascii_case("z") {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::Undo {
                                    buffer_id: self.viewport.buffer_id,
                                },
                            ));
                        } else if c.eq_ignore_ascii_case("y") {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::Redo {
                                    buffer_id: self.viewport.buffer_id,
                                },
                            ));
                        } else if c.eq_ignore_ascii_case("s") {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::SaveFile {
                                    buffer_id: self.viewport.buffer_id,
                                },
                            ));
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
                    let buffer_id = self.viewport.buffer_id;
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
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction: CursorDirection::Left,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::ArrowRight) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction: CursorDirection::Right,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::Home) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction: CursorDirection::LineStart,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::End) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::MoveCursor {
                                    buffer_id,
                                    direction: CursorDirection::LineEnd,
                                    select,
                                },
                            ));
                        }
                        Key::Named(Named::Backspace) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::DeleteBackward { buffer_id },
                            ));
                        }
                        Key::Named(Named::Delete) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::DeleteForward { buffer_id },
                            ));
                        }
                        Key::Named(Named::Enter) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::InsertText {
                                    buffer_id,
                                    text: "\n".to_string(),
                                },
                            ));
                        }
                        Key::Named(Named::Tab) => {
                            return self.update(DesktopMessage::DispatchEditorAction(
                                EditorAction::InsertText {
                                    buffer_id,
                                    text: "    ".to_string(),
                                },
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

    pub fn subscription(&self) -> Subscription<DesktopMessage> {
        iced::event::listen_with(|event, _status, _window| match event {
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
        })
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
                    .size(14)
                    .shaping(text::Shaping::Advanced)
                    .color(Color::from_rgb8(0x58, 0xA6, 0xFF)),
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
        .style(|_theme| container::Style {
            background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
            border: Border {
                color: Color::from_rgb8(0x30, 0x36, 0x3D),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

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
            .style(|_theme| container::Style {
                background: Some(Color::from_rgb8(0x0D, 0x11, 0x17).into()),
                ..Default::default()
            })
            .into()
    }

    fn view_editor_mode(&self) -> Element<'_, DesktopMessage> {
        // 1. Left File Explorer Sidebar
        let mut file_col = Column::new().spacing(2).padding(8);
        file_col = file_col.push(
            text(format!("EXPLORER ({})", self.workspace_files.len()))
                .size(11)
                .color(Color::from_rgb8(0x8B, 0x94, 0x9E)),
        );
        for file_path in self.workspace_files.iter().take(45) {
            let p = file_path.clone();
            let is_active = &self.viewport.file_path == file_path;
            let label = if is_active {
                format!("> {file_path}")
            } else {
                format!("  {file_path}")
            };
            file_col = file_col.push(
                button(
                    text(label)
                        .font(Font::MONOSPACE)
                        .size(12)
                        .shaping(text::Shaping::Advanced),
                )
                .width(Length::Fill)
                .style(move |_theme, status| sidebar_btn_style(is_active, status))
                .on_press(DesktopMessage::OpenFileFromSidebar(p)),
            );
        }
        let sidebar = container(scrollable(file_col))
            .width(Length::Fixed(240.0))
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
                border: Border {
                    color: Color::from_rgb8(0x30, 0x36, 0x3D),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            });

        // 2. Open Buffer Tabs Bar
        let mut tabs_row = Row::new().spacing(4).padding(4);
        for (buf_id, path, dirty) in self.workspace.open_buffers() {
            let short_name = std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&path)
                .to_string();
            let tab_title = if dirty {
                format!("{short_name} *")
            } else {
                short_name
            };
            let is_current = buf_id == self.viewport.buffer_id;
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
                .on_press(DesktopMessage::SelectBufferTab(buf_id)),
                button(text("×").size(12))
                    .style(move |_theme, status| tab_btn_style(false, status))
                    .on_press(DesktopMessage::CloseBufferTab(buf_id)),
            ]
            .spacing(2);
            tabs_row = tabs_row.push(tab_item);
        }
        let tab_bar = container(tabs_row)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
                border: Border {
                    color: Color::from_rgb8(0x30, 0x36, 0x3D),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            });

        // 3. Center Code Editor Canvas (with Line Numbers, Git Gutter, Syntax Spans, Cursors, Selections, Search Highlights, Minimap)
        let editor_canvas = Canvas::new(EditorCanvasProgram {
            viewport: &self.viewport,
            shaper: &self.shaper,
            search_matches: &self.search_matches,
        })
        .width(Length::Fill)
        .height(Length::FillPortion(3));

        // 4. Bottom Terminal / Diagnostics / Completions Panel
        let completions_list = self.workspace.completions(self.viewport.buffer_id);
        let is_term = self.bottom_tab == BottomPanelTab::Terminal;
        let is_diag = self.bottom_tab == BottomPanelTab::Diagnostics;
        let is_comp = self.bottom_tab == BottomPanelTab::Completions;
        let bottom_header = container(
            row![
                button(
                    text(if is_term {
                        "• TERMINAL"
                    } else {
                        "TERMINAL"
                    })
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
                        format!("• COMPLETIONS ({})", completions_list.len())
                    } else {
                        format!("COMPLETIONS ({})", completions_list.len())
                    })
                    .size(12)
                    .shaping(text::Shaping::Advanced)
                )
                .style(move |_theme, status| nav_btn_style(is_comp, status))
                .on_press(DesktopMessage::SelectBottomTab(BottomPanelTab::Completions)),
            ]
            .spacing(8)
            .padding(4),
        )
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
            border: Border {
                color: Color::from_rgb8(0x30, 0x36, 0x3D),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

        let bottom_content: Element<'_, DesktopMessage> = match self.bottom_tab {
            BottomPanelTab::Terminal => {
                let mut term_lines_col = Column::new().spacing(2);
                for line in self.terminal.lock().visible_lines() {
                    term_lines_col = term_lines_col.push(
                        text(line)
                            .font(Font::MONOSPACE)
                            .size(12)
                            .shaping(text::Shaping::Advanced)
                            .color(Color::from_rgb8(0x3F, 0xB9, 0x50)),
                    );
                }
                let input_row = row![
                    text("$").font(Font::MONOSPACE).size(13),
                    text_input("Run shell command in workspace...", &self.terminal_input)
                        .on_input(DesktopMessage::TerminalCommandChanged)
                        .on_submit(DesktopMessage::ExecuteTerminalCommand)
                        .font(Font::MONOSPACE)
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
                            .color(Color::from_rgb8(0x3F, 0xB9, 0x50)),
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
                            .font(Font::MONOSPACE)
                            .size(13)
                            .shaping(text::Shaping::Advanced)
                            .color(Color::from_rgb8(0xF8, 0x51, 0x49)),
                        );
                    }
                }
                scrollable(diag_col).height(Length::Fill).into()
            }
            BottomPanelTab::Completions => {
                let mut comp_row = Row::new().spacing(6).padding(6);
                if completions_list.is_empty() {
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
                    for item in completions_list.into_iter().take(12) {
                        let insert_val = item.clone();
                        comp_row = comp_row.push(
                            button(text(item).font(Font::MONOSPACE).size(12))
                                .style(|_theme, status| nav_btn_style(false, status))
                                .on_press(DesktopMessage::InsertCompletionItem(insert_val)),
                        );
                    }
                }
                scrollable(comp_row).height(Length::Fill).into()
            }
        };

        let bottom_panel = container(column![bottom_header, bottom_content])
            .width(Length::Fill)
            .height(Length::FillPortion(1))
            .style(|_theme| container::Style {
                background: Some(Color::from_rgb8(0x0D, 0x11, 0x17).into()),
                border: Border {
                    color: Color::from_rgb8(0x30, 0x36, 0x3D),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            });

        let mut center_col = Column::new().push(tab_bar);
        if self.search_bar_open {
            let search_bar = container(
                row![
                    text("Find:").size(13),
                    text_input("Search in buffer...", &self.search_query)
                        .on_input(DesktopMessage::SearchQueryChanged)
                        .on_submit(DesktopMessage::JumpToNextSearchMatch)
                        .size(13),
                    text(format!("{} matches", self.search_matches.len())).size(12),
                    button(text("Next Match").size(12))
                        .style(|_theme, status| nav_btn_style(true, status))
                        .on_press(DesktopMessage::JumpToNextSearchMatch),
                    button(text("Close").size(12))
                        .style(|_theme, status| nav_btn_style(false, status))
                        .on_press(DesktopMessage::ToggleSearchBar),
                ]
                .spacing(8)
                .padding(6),
            )
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Color::from_rgb8(0x21, 0x26, 0x2D).into()),
                ..Default::default()
            });
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

    fn view_agent_manager_mode(&self) -> Element<'_, DesktopMessage> {
        let prompt_bar = container(
            row![
                text_input(
                    "Dispatch parallel AI Agent task (e.g. `120 -> 240`, `doc: Core module`, or natural language prompt)...",
                    &self.agent_prompt_input
                )
                .on_input(DesktopMessage::AgentPromptChanged)
                .on_submit(DesktopMessage::SubmitAgentPrompt)
                .size(13),
                button(text("Spawn Agent Task").size(13))
                    .style(|_theme, status| nav_btn_style(true, status))
                    .on_press(DesktopMessage::SubmitAgentPrompt),
            ]
            .spacing(8)
            .padding(10),
        )
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
            border: Border {
                color: Color::from_rgb8(0x30, 0x36, 0x3D),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        });

        let mut task_list_col = Column::new().spacing(8).padding(10);
        task_list_col = task_list_col.push(
            text("PARALLEL AGENT TASKS")
                .size(13)
                .color(Color::from_rgb8(0x58, 0xA6, 0xFF)),
        );
        if self.viewport.agent_tasks.is_empty() {
            task_list_col = task_list_col.push(
                text("No agent tasks submitted yet. Enter a prompt above to generate a Unified Diff patch.")
                    .size(13)
                    .color(Color::from_rgb8(0x8B, 0x94, 0x9E)),
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
                .color(Color::from_rgb8(0x58, 0xA6, 0xFF)),
        );
        if let Some(task) = active_task {
            if let Some(diff_text) = &task.unified_diff_preview {
                for line in diff_text.lines() {
                    let color = if line.starts_with('+') && !line.starts_with("+++") {
                        Color::from_rgb8(0x3F, 0xB9, 0x50)
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        Color::from_rgb8(0xF8, 0x51, 0x49)
                    } else if line.starts_with("@@") {
                        Color::from_rgb8(0x58, 0xA6, 0xFF)
                    } else {
                        Color::from_rgb8(0xC9, 0xD1, 0xD9)
                    };
                    diff_col = diff_col.push(
                        text(line)
                            .font(Font::MONOSPACE)
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
                    .color(Color::from_rgb8(0x8B, 0x94, 0x9E)),
            );
        }

        let split = Row::new()
            .push(
                container(scrollable(task_list_col))
                    .width(Length::FillPortion(1))
                    .height(Length::Fill)
                    .style(|_theme| container::Style {
                        background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
                        border: Border {
                            color: Color::from_rgb8(0x30, 0x36, 0x3D),
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
            )
            .push(
                container(scrollable(diff_col))
                    .width(Length::FillPortion(2))
                    .height(Length::Fill)
                    .style(|_theme| container::Style {
                        background: Some(Color::from_rgb8(0x0D, 0x11, 0x17).into()),
                        ..Default::default()
                    }),
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
                        button(text(file).font(Font::MONOSPACE).size(12))
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
            .style(|_theme| container::Style {
                background: Some(Color::from_rgb8(0x21, 0x26, 0x2D).into()),
                border: Border {
                    color: Color::from_rgb8(0x58, 0xA6, 0xFF),
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            })
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
                    .color(Color::from_rgb8(0x58, 0xA6, 0xFF)),
                text(format!("git: {}", self.git_branch))
                    .size(11)
                    .color(Color::from_rgb8(0xC9, 0xD1, 0xD9)),
                text(format!("Diagnostics: {}", self.viewport.diagnostics.len()))
                    .size(11)
                    .color(Color::from_rgb8(0x8B, 0x94, 0x9E)),
                horizontal_space(),
                text(format!(
                    "Ln {ln}, Col {col} ({cursor_count} cursor{})",
                    if cursor_count > 1 { "s" } else { "" }
                ))
                .size(11)
                .color(Color::from_rgb8(0xC9, 0xD1, 0xD9)),
                text(format!("{lang:?}"))
                    .size(11)
                    .color(Color::from_rgb8(0x8B, 0x94, 0x9E)),
                text("UTF-8 | 120fps")
                    .size(11)
                    .color(Color::from_rgb8(0x3F, 0xB9, 0x50)),
            ]
            .spacing(16)
            .padding(4),
        )
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Color::from_rgb8(0x16, 0x1B, 0x22).into()),
            border: Border {
                color: Color::from_rgb8(0x30, 0x36, 0x3D),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
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

/// Interactive state for the `EditorCanvasProgram` tracking mouse drag selection and modifiers.
#[derive(Default)]
struct EditorCanvasState {
    is_dragging: bool,
    modifiers: Modifiers,
}

/// Custom `iced` Canvas program that draws the code editor surface at 120fps:
/// active line highlight, selection rectangles, search highlights, diagnostic underlines,
/// line numbers, Git diff gutter, syntax spans, cursors, and right-side minimap.
struct EditorCanvasProgram<'a> {
    viewport: &'a ViewportViewModel,
    shaper: &'a Arc<Mutex<CosmicTextShaper>>,
    search_matches: &'a [SearchMatch],
}

impl<'a> canvas::Program<DesktopMessage> for EditorCanvasProgram<'a> {
    type State = EditorCanvasState;

    fn update(
        &self,
        state: &mut Self::State,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<DesktopMessage>) {
        if let canvas::Event::Keyboard(keyboard::Event::ModifiersChanged(mods)) = event {
            state.modifiers = mods;
            return (canvas::event::Status::Ignored, None);
        }

        let Some(pos) = cursor.position_in(bounds) else {
            state.is_dragging = false;
            return (canvas::event::Status::Ignored, None);
        };

        let gutter_width = 56.0_f32;
        let line_height = 20.0_f32;
        let char_width = 8.4_f32;

        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                state.is_dragging = true;
                let clicked_line = self.viewport.first_visible_line
                    + (pos.y / line_height).floor().max(0.0) as u32;
                let clicked_col = ((pos.x - gutter_width - 8.0).max(0.0) / char_width).round() as u32;

                if state.modifiers.alt() {
                    return (
                        canvas::event::Status::Captured,
                        Some(DesktopMessage::AddMultiCursor(clicked_line, clicked_col)),
                    );
                }

                return (
                    canvas::event::Status::Captured,
                    Some(DesktopMessage::DispatchEditorAction(
                        EditorAction::SetCursorPosition {
                            buffer_id: self.viewport.buffer_id,
                            line: clicked_line,
                            column: clicked_col,
                            select: state.modifiers.shift(),
                        },
                    )),
                );
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.is_dragging => {
                let drag_line = self.viewport.first_visible_line
                    + (pos.y / line_height).floor().max(0.0) as u32;
                let drag_col = ((pos.x - gutter_width - 8.0).max(0.0) / char_width).round() as u32;
                return (
                    canvas::event::Status::Captured,
                    Some(DesktopMessage::DispatchEditorAction(
                        EditorAction::SetCursorPosition {
                            buffer_id: self.viewport.buffer_id,
                            line: drag_line,
                            column: drag_col,
                            select: true,
                        },
                    )),
                );
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.is_dragging = false;
            }
            canvas::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let lines_delta = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => -y as i32 * 3,
                    mouse::ScrollDelta::Pixels { y, .. } => -(y / 20.0) as i32,
                };
                let new_first =
                    (self.viewport.first_visible_line as i32 + lines_delta).max(0) as u32;
                let visible_count = (bounds.height / 20.0).ceil().max(10.0) as u32;
                return (
                    canvas::event::Status::Captured,
                    Some(DesktopMessage::DispatchEditorAction(
                        EditorAction::ScrollViewport {
                            buffer_id: self.viewport.buffer_id,
                            first_visible_line: new_first,
                            visible_line_count: visible_count,
                        },
                    )),
                );
            }
            _ => {}
        }

        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let gutter_width = 56.0_f32;
        let minimap_width = 72.0_f32;
        let line_height = 20.0_f32;
        let char_width = 8.4_f32;

        // 1. Editor background & Gutter background
        frame.fill_rectangle(
            Point::ORIGIN,
            bounds.size(),
            Color::from_rgb8(0x0D, 0x11, 0x17),
        );
        frame.fill_rectangle(
            Point::ORIGIN,
            Size::new(gutter_width, bounds.height),
            Color::from_rgb8(0x16, 0x1B, 0x22),
        );

        // 2. Active cursor line highlight
        if let Some(primary) = self.viewport.cursors.first() {
            if primary.line >= self.viewport.first_visible_line {
                let rel_row = primary.line - self.viewport.first_visible_line;
                let y = rel_row as f32 * line_height;
                if y < bounds.height {
                    frame.fill_rectangle(
                        Point::new(gutter_width, y),
                        Size::new(bounds.width - gutter_width, line_height),
                        Color::from_rgba8(0x21, 0x26, 0x2D, 0.65),
                    );
                }
            }
        }

        // 3. Selection range highlights
        for sel in &self.viewport.selections {
            for rendered in &self.viewport.visible_lines {
                let ln = rendered.line_number;
                if ln < sel.start_line || ln > sel.end_line {
                    continue;
                }
                let rel_row = ln - self.viewport.first_visible_line;
                let y = rel_row as f32 * line_height;
                let line_chars = rendered.text.chars().count() as u32;
                let s_col = if ln == sel.start_line { sel.start_col } else { 0 };
                let e_col = if ln == sel.end_line {
                    sel.end_col
                } else {
                    line_chars.max(1)
                };
                if s_col < e_col {
                    let sx = gutter_width + 8.0 + s_col as f32 * char_width;
                    let sw = (e_col - s_col) as f32 * char_width;
                    frame.fill_rectangle(
                        Point::new(sx, y),
                        Size::new(sw, line_height),
                        Color::from_rgba8(0x26, 0x4F, 0x78, 0.65),
                    );
                }
            }
        }

        // 4. Search match highlights
        for m in self.search_matches {
            if m.line >= self.viewport.first_visible_line {
                let rel_row = m.line - self.viewport.first_visible_line;
                let y = rel_row as f32 * line_height;
                if y < bounds.height {
                    let match_len = (m.end_char.saturating_sub(m.start_char)).max(1) as f32;
                    let mx = gutter_width + 8.0 + m.column as f32 * char_width;
                    frame.fill_rectangle(
                        Point::new(mx, y + 1.0),
                        Size::new(match_len * char_width, line_height - 2.0),
                        Color::from_rgba8(0xD2, 0x99, 0x22, 0.45),
                    );
                }
            }
        }

        // 5. Draw visible lines, line numbers, Git diff markers, and syntax spans
        for (row_idx, rendered_line) in self.viewport.visible_lines.iter().enumerate() {
            let y = row_idx as f32 * line_height;
            if y > bounds.height {
                break;
            }

            frame.fill_text(CanvasText {
                content: format!("{:>4}", rendered_line.line_number + 1),
                position: Point::new(6.0, y + 2.0),
                color: Color::from_rgb8(0x6E, 0x76, 0x81),
                size: 13.0.into(),
                font: Font::MONOSPACE,
                ..CanvasText::default()
            });

            if let Some(marker) = self
                .viewport
                .git_diff_markers
                .iter()
                .find(|m| m.line_number == rendered_line.line_number)
            {
                let marker_color = match marker.status {
                    DiffMarkerKind::Added => Color::from_rgb8(0x2E, 0xA0, 0x43),
                    DiffMarkerKind::Modified => Color::from_rgb8(0x58, 0xA6, 0xFF),
                    DiffMarkerKind::Deleted => Color::from_rgb8(0xF8, 0x51, 0x49),
                    DiffMarkerKind::Unchanged => Color::TRANSPARENT,
                };
                frame.fill_rectangle(
                    Point::new(gutter_width - 4.0, y),
                    Size::new(3.0, line_height),
                    marker_color,
                );
            }

            if rendered_line.spans.is_empty() {
                frame.fill_text(CanvasText {
                    content: rendered_line.text.clone(),
                    position: Point::new(gutter_width + 8.0, y + 2.0),
                    color: Color::from_rgb8(0xE6, 0xED, 0xF3),
                    size: 14.0.into(),
                    font: Font::MONOSPACE,
                    shaping: text::Shaping::Advanced,
                    ..CanvasText::default()
                });
            } else {
                let chars: Vec<char> = rendered_line.text.chars().collect();
                let mut cursor_col = 0usize;
                for span in &rendered_line.spans {
                    let s = (span.start_col as usize).min(chars.len());
                    let e = (span.end_col as usize).min(chars.len());
                    if cursor_col < s {
                        let gap: String = chars[cursor_col..s].iter().collect();
                        frame.fill_text(CanvasText {
                            content: gap,
                            position: Point::new(
                                gutter_width + 8.0 + cursor_col as f32 * char_width,
                                y + 2.0,
                            ),
                            color: Color::from_rgb8(0xE6, 0xED, 0xF3),
                            size: 14.0.into(),
                            font: Font::MONOSPACE,
                            shaping: text::Shaping::Advanced,
                            ..CanvasText::default()
                        });
                    }
                    if s < e {
                        let seg: String = chars[s..e].iter().collect();
                        frame.fill_text(CanvasText {
                            content: seg,
                            position: Point::new(
                                gutter_width + 8.0 + s as f32 * char_width,
                                y + 2.0,
                            ),
                            color: rgba_u32_to_iced_color(span.color_rgba),
                            size: 14.0.into(),
                            font: Font::MONOSPACE,
                            shaping: text::Shaping::Advanced,
                            ..CanvasText::default()
                        });
                        cursor_col = e;
                    }
                }
                if cursor_col < chars.len() {
                    let tail: String = chars[cursor_col..].iter().collect();
                    frame.fill_text(CanvasText {
                        content: tail,
                        position: Point::new(
                            gutter_width + 8.0 + cursor_col as f32 * char_width,
                            y + 2.0,
                        ),
                        color: Color::from_rgb8(0xE6, 0xED, 0xF3),
                        size: 14.0.into(),
                        font: Font::MONOSPACE,
                        shaping: text::Shaping::Advanced,
                        ..CanvasText::default()
                    });
                }
            }
        }

        // 6. Diagnostic underlines
        for diag in &self.viewport.diagnostics {
            if diag.line >= self.viewport.first_visible_line {
                let rel_row = diag.line - self.viewport.first_visible_line;
                let y = (rel_row + 1) as f32 * line_height - 2.0;
                if y < bounds.height {
                    let dx = gutter_width + 8.0 + diag.start_col as f32 * char_width;
                    let dw = (diag.end_col.saturating_sub(diag.start_col).max(1)) as f32 * char_width;
                    frame.fill_rectangle(
                        Point::new(dx, y),
                        Size::new(dw, 2.0),
                        Color::from_rgb8(0xF8, 0x51, 0x49),
                    );
                }
            }
        }

        // 7. Draw active cursors using cosmic-text measurement
        let mut shaper = self.shaper.lock();
        for c in &self.viewport.cursors {
            if c.line >= self.viewport.first_visible_line {
                let rel_line = c.line - self.viewport.first_visible_line;
                let y = rel_line as f32 * line_height;
                if y < bounds.height {
                    let line_str = self
                        .viewport
                        .visible_lines
                        .get(rel_line as usize)
                        .map(|l| l.text.as_str())
                        .unwrap_or("");
                    let x_off = shaper.column_x_offset(line_str, c.column);
                    let cursor_x = gutter_width + 8.0 + x_off.max(c.column as f32 * char_width);
                    frame.fill_rectangle(
                        Point::new(cursor_x, y + 2.0),
                        Size::new(2.0, line_height - 4.0),
                        if c.is_primary {
                            Color::from_rgb8(0x58, 0xA6, 0xFF)
                        } else {
                            Color::from_rgb8(0xD2, 0xA8, 0xFF)
                        },
                    );
                }
            }
        }

        // 8. Right-hand Minimap
        if bounds.width > gutter_width + minimap_width + 100.0 {
            let minimap_x = bounds.width - minimap_width;
            frame.fill_rectangle(
                Point::new(minimap_x, 0.0),
                Size::new(minimap_width, bounds.height),
                Color::from_rgba8(0x16, 0x1B, 0x22, 0.85),
            );
            for (idx, line) in self.viewport.visible_lines.iter().enumerate() {
                let my = idx as f32 * 3.0 + 4.0;
                if my > bounds.height - 4.0 {
                    break;
                }
                let bar_w = (line.text.len() as f32 * 0.8).min(minimap_width - 12.0);
                if bar_w > 1.0 {
                    let bar_color = line
                        .spans
                        .first()
                        .map(|s| rgba_u32_to_iced_color(s.color_rgba))
                        .unwrap_or(Color::from_rgba8(0x8B, 0x94, 0x9E, 0.6));
                    frame.fill_rectangle(
                        Point::new(minimap_x + 6.0, my),
                        Size::new(bar_w, 1.5),
                        bar_color,
                    );
                }
            }
            let viewport_box = Path::rectangle(
                Point::new(minimap_x + 2.0, 2.0),
                Size::new(
                    minimap_width - 4.0,
                    (self.viewport.visible_lines.len() as f32 * 3.0).min(bounds.height - 4.0),
                ),
            );
            frame.stroke(
                &viewport_box,
                Stroke::default()
                    .with_width(1.0)
                    .with_color(Color::from_rgba8(0x58, 0xA6, 0xFF, 0.5)),
            );
        }

        vec![frame.into_geometry()]
    }
}

pub fn rgba_u32_to_iced_color(rgba: u32) -> Color {
    let r = ((rgba >> 24) & 0xFF) as u8;
    let g = ((rgba >> 16) & 0xFF) as u8;
    let b = ((rgba >> 8) & 0xFF) as u8;
    let a = (rgba & 0xFF) as f32 / 255.0;
    Color::from_rgba8(r, g, b, a)
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
    fn test_search_bar_multi_tab_and_real_terminal_execution() {
        let mut app = IdeDesktopApp::new(".");
        let _ = app.update(DesktopMessage::ToggleSearchBar);
        assert!(app.search_bar_open);
        let _ = app.update(DesktopMessage::SearchQueryChanged("ProjectConfig".to_string()));
        assert_eq!(app.search_matches.len(), 2);
        let _ = app.update(DesktopMessage::JumpToNextSearchMatch);
        assert_eq!(app.viewport.cursors[0].line, 3);

        // Execute a real shell command in the terminal emulator
        let _ = app.update(DesktopMessage::TerminalCommandChanged(
            "echo LEVITY_TERM_OK".to_string(),
        ));
        let _ = app.update(DesktopMessage::ExecuteTerminalCommand);
        let term_lines = app.terminal.lock().visible_lines();
        assert!(term_lines.iter().any(|l| l.contains("LEVITY_TERM_OK")));
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
}
