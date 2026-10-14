//! `ide-ffi`: Mozilla UniFFI cross-language bridge exposing `IdeWorkspace`, `EditorAction`,
//! `ViewportViewModel`, and `RenderedLine` to Android (Kotlin) and iOS (Swift).

use ide_core as core;
use thiserror::Error;

uniffi::setup_scaffolding!("ide_ffi");

#[derive(Debug, Error, uniffi::Error)]
pub enum FfiError {
    #[error("IDE workspace error: {message}")]
    Workspace { message: String },
}

impl From<core::WorkspaceError> for FfiError {
    fn from(err: core::WorkspaceError) -> Self {
        Self::Workspace {
            message: err.to_string(),
        }
    }
}

/// Cursor movement directions exposed across FFI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CursorDirection {
    Up,
    Down,
    Left,
    Right,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    WordLeft,
    WordRight,
}

impl From<CursorDirection> for core::CursorDirection {
    fn from(d: CursorDirection) -> Self {
        match d {
            CursorDirection::Up => Self::Up,
            CursorDirection::Down => Self::Down,
            CursorDirection::Left => Self::Left,
            CursorDirection::Right => Self::Right,
            CursorDirection::LineStart => Self::LineStart,
            CursorDirection::LineEnd => Self::LineEnd,
            CursorDirection::DocumentStart => Self::DocumentStart,
            CursorDirection::DocumentEnd => Self::DocumentEnd,
            CursorDirection::WordLeft => Self::WordLeft,
            CursorDirection::WordRight => Self::WordRight,
        }
    }
}

/// Unidirectional Data Flow action enum exposed via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
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
    InsertNewline {
        buffer_id: u64,
    },
    Indent {
        buffer_id: u64,
    },
    Outdent {
        buffer_id: u64,
    },
    ToggleLineComment {
        buffer_id: u64,
    },
    SelectAll {
        buffer_id: u64,
    },
    AddNextOccurrence {
        buffer_id: u64,
    },
    DeleteWordBackward {
        buffer_id: u64,
    },
    DeleteWordForward {
        buffer_id: u64,
    },
    DeleteSelectionOrLine {
        buffer_id: u64,
    },
    MovePage {
        buffer_id: u64,
        pages: i32,
        select: bool,
    },
    CollapseCursors {
        buffer_id: u64,
    },
}

impl From<EditorAction> for core::EditorAction {
    fn from(action: EditorAction) -> Self {
        match action {
            EditorAction::OpenFile { path } => Self::OpenFile { path },
            EditorAction::SaveFile { buffer_id } => Self::SaveFile { buffer_id },
            EditorAction::InsertText { buffer_id, text } => Self::InsertText { buffer_id, text },
            EditorAction::DeleteBackward { buffer_id } => Self::DeleteBackward { buffer_id },
            EditorAction::DeleteForward { buffer_id } => Self::DeleteForward { buffer_id },
            EditorAction::MoveCursor {
                buffer_id,
                direction,
                select,
            } => Self::MoveCursor {
                buffer_id,
                direction: direction.into(),
                select,
            },
            EditorAction::SetCursorPosition {
                buffer_id,
                line,
                column,
                select,
            } => Self::SetCursorPosition {
                buffer_id,
                line,
                column,
                select,
            },
            EditorAction::ScrollViewport {
                buffer_id,
                first_visible_line,
                visible_line_count,
            } => Self::ScrollViewport {
                buffer_id,
                first_visible_line,
                visible_line_count,
            },
            EditorAction::Undo { buffer_id } => Self::Undo { buffer_id },
            EditorAction::Redo { buffer_id } => Self::Redo { buffer_id },
            EditorAction::TriggerLspCompletion { buffer_id } => {
                Self::TriggerLspCompletion { buffer_id }
            }
            EditorAction::SubmitAgentPrompt { prompt } => Self::SubmitAgentPrompt { prompt },
            EditorAction::InsertNewline { buffer_id } => Self::InsertNewline { buffer_id },
            EditorAction::Indent { buffer_id } => Self::Indent { buffer_id },
            EditorAction::Outdent { buffer_id } => Self::Outdent { buffer_id },
            EditorAction::ToggleLineComment { buffer_id } => Self::ToggleLineComment { buffer_id },
            EditorAction::SelectAll { buffer_id } => Self::SelectAll { buffer_id },
            EditorAction::AddNextOccurrence { buffer_id } => Self::AddNextOccurrence { buffer_id },
            EditorAction::DeleteWordBackward { buffer_id } => {
                Self::DeleteWordBackward { buffer_id }
            }
            EditorAction::DeleteWordForward { buffer_id } => Self::DeleteWordForward { buffer_id },
            EditorAction::DeleteSelectionOrLine { buffer_id } => {
                Self::DeleteSelectionOrLine { buffer_id }
            }
            EditorAction::MovePage {
                buffer_id,
                pages,
                select,
            } => Self::MovePage {
                buffer_id,
                pages,
                select,
            },
            EditorAction::CollapseCursors { buffer_id } => Self::CollapseCursors { buffer_id },
        }
    }
}

/// Syntax highlight span within a `RenderedLine`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HighlightSpan {
    pub start_col: u32,
    pub end_col: u32,
    pub color_rgba: u32,
    pub font_style: u8,
}

impl From<core::HighlightSpan> for HighlightSpan {
    fn from(s: core::HighlightSpan) -> Self {
        Self {
            start_col: s.start_col,
            end_col: s.end_col,
            color_rgba: s.color_rgba,
            font_style: s.font_style,
        }
    }
}

/// A single rendered line exported via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RenderedLine {
    pub line_number: u32,
    pub text: String,
    pub spans: Vec<HighlightSpan>,
}

impl From<core::RenderedLine> for RenderedLine {
    fn from(l: core::RenderedLine) -> Self {
        Self {
            line_number: l.line_number,
            text: l.text,
            spans: l.spans.into_iter().map(HighlightSpan::from).collect(),
        }
    }
}

/// Visual cursor coordinates exported via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CursorRect {
    pub line: u32,
    pub column: u32,
    pub is_primary: bool,
}

impl From<core::CursorRect> for CursorRect {
    fn from(c: core::CursorRect) -> Self {
        Self {
            line: c.line,
            column: c.column,
            is_primary: c.is_primary,
        }
    }
}

/// Visual selection range exported via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SelectionRange {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl From<core::SelectionRange> for SelectionRange {
    fn from(s: core::SelectionRange) -> Self {
        Self {
            start_line: s.start_line,
            start_col: s.start_col,
            end_line: s.end_line,
            end_col: s.end_col,
        }
    }
}

/// Severity of a diagnostic item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DiagnosticSeverityLevel {
    Error,
    Warning,
    Information,
    Hint,
}

impl From<core::DiagnosticSeverityLevel> for DiagnosticSeverityLevel {
    fn from(s: core::DiagnosticSeverityLevel) -> Self {
        match s {
            core::DiagnosticSeverityLevel::Error => Self::Error,
            core::DiagnosticSeverityLevel::Warning => Self::Warning,
            core::DiagnosticSeverityLevel::Information => Self::Information,
            core::DiagnosticSeverityLevel::Hint => Self::Hint,
        }
    }
}

/// Diagnostic item exported via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DiagnosticItem {
    pub line: u32,
    pub start_col: u32,
    pub end_col: u32,
    pub severity: DiagnosticSeverityLevel,
    pub message: String,
}

impl From<core::DiagnosticItem> for DiagnosticItem {
    fn from(d: core::DiagnosticItem) -> Self {
        Self {
            line: d.line,
            start_col: d.start_col,
            end_col: d.end_col,
            severity: d.severity.into(),
            message: d.message,
        }
    }
}

/// Kind of Git gutter diff marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DiffMarkerKind {
    Unchanged,
    Added,
    Modified,
    Deleted,
}

impl From<core::DiffMarkerKind> for DiffMarkerKind {
    fn from(k: core::DiffMarkerKind) -> Self {
        match k {
            core::DiffMarkerKind::Unchanged => Self::Unchanged,
            core::DiffMarkerKind::Added => Self::Added,
            core::DiffMarkerKind::Modified => Self::Modified,
            core::DiffMarkerKind::Deleted => Self::Deleted,
        }
    }
}

/// Per-line Git gutter diff marker exported via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LineDiffStatus {
    pub line_number: u32,
    pub status: DiffMarkerKind,
}

impl From<core::LineDiffStatus> for LineDiffStatus {
    fn from(s: core::LineDiffStatus) -> Self {
        Self {
            line_number: s.line_number,
            status: s.status.into(),
        }
    }
}

/// Summary of an AI Agent task exported via UniFFI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentTaskStatus {
    pub task_id: u64,
    pub prompt: String,
    pub state: String,
    pub verification_passed: bool,
    pub unified_diff_preview: Option<String>,
}

impl From<core::AgentTaskStatus> for AgentTaskStatus {
    fn from(a: core::AgentTaskStatus) -> Self {
        Self {
            task_id: a.task_id,
            prompt: a.prompt,
            state: a.state,
            verification_passed: a.verification_passed,
            unified_diff_preview: a.unified_diff_preview,
        }
    }
}

/// Complete viewport ViewModel exported via UniFFI for 120fps rendering on Android & iOS.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
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

impl From<core::ViewportViewModel> for ViewportViewModel {
    fn from(vm: core::ViewportViewModel) -> Self {
        Self {
            buffer_id: vm.buffer_id,
            file_path: vm.file_path,
            is_dirty: vm.is_dirty,
            total_lines: vm.total_lines,
            first_visible_line: vm.first_visible_line,
            visible_lines: vm.visible_lines.into_iter().map(RenderedLine::from).collect(),
            cursors: vm.cursors.into_iter().map(CursorRect::from).collect(),
            selections: vm.selections.into_iter().map(SelectionRange::from).collect(),
            diagnostics: vm.diagnostics.into_iter().map(DiagnosticItem::from).collect(),
            git_diff_markers: vm
                .git_diff_markers
                .into_iter()
                .map(LineDiffStatus::from)
                .collect(),
            agent_tasks: vm
                .agent_tasks
                .into_iter()
                .map(AgentTaskStatus::from)
                .collect(),
        }
    }
}

/// Open editor tab information.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OpenBufferInfo {
    pub buffer_id: u64,
    /// Absolute path, or `None` for untitled buffers.
    pub path: Option<String>,
    pub title: String,
    pub is_dirty: bool,
}

impl From<core::OpenBufferInfo> for OpenBufferInfo {
    fn from(b: core::OpenBufferInfo) -> Self {
        Self {
            buffer_id: b.buffer_id,
            path: b.path.map(|p| p.to_string_lossy().into_owned()),
            title: b.title,
            is_dirty: b.is_dirty,
        }
    }
}

/// One explorer entry (a single directory level).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DirEntryInfo {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_ignored: bool,
}

impl From<core::DirEntryInfo> for DirEntryInfo {
    fn from(e: core::DirEntryInfo) -> Self {
        Self {
            name: e.name,
            path: e.path.to_string_lossy().into_owned(),
            is_dir: e.is_dir,
            is_ignored: e.is_ignored,
        }
    }
}

/// Clipboard payload for Copy/Cut.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ClipboardPayload {
    pub text: String,
    /// True when nothing was selected and the whole line(s) were copied.
    pub is_whole_line: bool,
}

/// UniFFI Object wrapping `ide_core::IdeWorkspace` for thread-safe mobile/cross-platform calls.
#[derive(uniffi::Object)]
pub struct FfiWorkspace {
    inner: core::IdeWorkspace,
}

#[uniffi::export]
impl FfiWorkspace {
    #[uniffi::constructor]
    pub fn new(root_dir: String) -> Self {
        Self {
            inner: core::IdeWorkspace::new(root_dir),
        }
    }

    pub fn dispatch(&self, action: EditorAction) -> Result<ViewportViewModel, FfiError> {
        let vm = self.inner.dispatch(action.into())?;
        Ok(ViewportViewModel::from(vm))
    }

    pub fn active_viewport(&self) -> Result<ViewportViewModel, FfiError> {
        let vm = self.inner.active_viewport()?;
        Ok(ViewportViewModel::from(vm))
    }

    pub fn list_workspace_files(&self) -> Vec<String> {
        self.inner.list_workspace_files()
    }

    pub fn current_git_branch(&self) -> Option<String> {
        self.inner.current_git_branch()
    }

    pub fn accept_agent_task(
        &self,
        task_id: u64,
        buffer_id: u64,
    ) -> Result<ViewportViewModel, FfiError> {
        let vm = self.inner.accept_agent_task(task_id, buffer_id)?;
        Ok(ViewportViewModel::from(vm))
    }

    pub fn reject_agent_task(
        &self,
        task_id: u64,
        buffer_id: u64,
    ) -> Result<ViewportViewModel, FfiError> {
        let vm = self.inner.reject_agent_task(task_id, buffer_id)?;
        Ok(ViewportViewModel::from(vm))
    }

    /// Absolute workspace root folder.
    pub fn root_dir(&self) -> String {
        self.inner.root_dir().to_string_lossy().into_owned()
    }

    /// Open tabs in order.
    pub fn open_buffers(&self) -> Vec<OpenBufferInfo> {
        self.inner
            .open_buffer_infos()
            .into_iter()
            .map(OpenBufferInfo::from)
            .collect()
    }

    pub fn set_active_buffer(&self, buffer_id: u64) -> Result<ViewportViewModel, FfiError> {
        Ok(self.inner.set_active_buffer_id(buffer_id)?.into())
    }

    pub fn close_buffer(&self, buffer_id: u64) -> Result<ViewportViewModel, FfiError> {
        Ok(self.inner.close_buffer(buffer_id)?.into())
    }

    pub fn new_untitled_buffer(&self) -> Result<ViewportViewModel, FfiError> {
        self.inner.new_untitled_buffer()?;
        Ok(self.inner.active_viewport()?.into())
    }

    pub fn save_buffer_as(&self, buffer_id: u64, path: String) -> Result<ViewportViewModel, FfiError> {
        Ok(self.inner.save_buffer_as(buffer_id, path)?.into())
    }

    /// Text to copy (selection, or current line when nothing is selected).
    pub fn clipboard_text(&self, buffer_id: u64) -> Result<ClipboardPayload, FfiError> {
        let (text, is_whole_line) = self.inner.clipboard_text(buffer_id)?;
        Ok(ClipboardPayload {
            text,
            is_whole_line,
        })
    }

    /// Lists one directory level (folders first). Pass an empty string for the root.
    pub fn list_directory(&self, dir: String) -> Result<Vec<DirEntryInfo>, FfiError> {
        let target = if dir.is_empty() {
            self.inner.root_dir()
        } else {
            std::path::PathBuf::from(dir)
        };
        Ok(self
            .inner
            .list_directory(target)?
            .into_iter()
            .map(DirEntryInfo::from)
            .collect())
    }

    pub fn create_file(&self, path: String) -> Result<ViewportViewModel, FfiError> {
        Ok(self.inner.create_file(path)?.into())
    }

    pub fn create_dir(&self, path: String) -> Result<String, FfiError> {
        Ok(self.inner.create_dir(path)?.to_string_lossy().into_owned())
    }

    pub fn rename_path(&self, from: String, to: String) -> Result<String, FfiError> {
        Ok(self
            .inner
            .rename_path(from, to)?
            .to_string_lossy()
            .into_owned())
    }

    pub fn delete_path(&self, path: String) -> Result<(), FfiError> {
        Ok(self.inner.delete_path(path)?)
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.inner.has_unsaved_changes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffi_workspace_contract_roundtrip() {
        let ws = FfiWorkspace::new(".".to_string());
        let vm = ws.active_viewport().unwrap();
        assert_eq!(vm.buffer_id, 1);
        assert!(!vm.visible_lines.is_empty());

        let updated = ws
            .dispatch(EditorAction::InsertText {
                buffer_id: vm.buffer_id,
                text: "// FFI test\n".to_string(),
            })
            .unwrap();
        assert!(updated.is_dirty);
        assert!(updated.visible_lines[0].text.contains("FFI test"));
        let tabs = ws.open_buffers();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title, "Untitled-1");
        assert!(tabs[0].is_dirty);
        let clip = ws.clipboard_text(vm.buffer_id).unwrap();
        assert!(clip.is_whole_line);
        let root_listing = ws.list_directory(String::new()).unwrap();
        assert!(root_listing.iter().any(|e| e.name == "src" && e.is_dir));
    }
}
