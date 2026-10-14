//! `ide-buffer`: Rope-based text buffer supporting UTF-8/UTF-16 offset conversion,
//! multi-cursor editing, transaction-grouped Undo/Redo history, and fast string search.

pub use ropey::Rope;
use serde::{Deserialize, Serialize};
use std::cmp::{ max, min };
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BufferError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Out of bounds character index: {0} (len: {1})")]
    CharOutOfBounds(usize, usize),
}

/// Direction for cursor movement operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorDirection {
    Up,
    Down,
    Left,
    Right,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    /// Moves to the start of the previous word (Ctrl+Left / Alt+Left).
    WordLeft,
    /// Moves to the end of the next word (Ctrl+Right / Alt+Right).
    WordRight,
}

/// Character class used for word-wise cursor movement and undo coalescing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Newline,
    Punctuation,
}

fn char_class(c: char) -> CharClass {
    if c == '\n' || c == '\r' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

/// 0-indexed row and 0-indexed byte column within the row, matching `tree_sitter::Point`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytePoint {
    pub row: usize,
    pub column: usize,
}

/// Describes a single contiguous edit in byte & row/byte-column coordinates
/// so incremental parsers (`tree-sitter`) can update their syntax trees directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start_point: BytePoint,
    pub old_end_point: BytePoint,
    pub new_end_point: BytePoint,
}

/// Represents a single cursor and optional selection (`anchor != head`).
/// Indices are character offsets (`usize`) within the `Rope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub anchor: usize,
    pub head: usize,
    pub preferred_column: Option<u32>,
}

impl Cursor {
    pub fn new(pos: usize) -> Self {
        Self {
            anchor: pos,
            head: pos,
            preferred_column: None,
        }
    }

    pub fn with_selection(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            preferred_column: None,
        }
    }

    pub fn has_selection(&self) -> bool {
        self.anchor != self.head
    }

    pub fn selection_range(&self) -> (usize, usize) {
        (min(self.anchor, self.head), max(self.anchor, self.head))
    }
}

/// A match found during buffer text search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub start_char: usize,
    pub end_char: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: u32,
    pub column: u32,
}

/// An atomic edit recorded inside a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AtomicEdit {
    start_char: usize,
    deleted_text: String,
    inserted_text: String,
}

/// A transaction grouping one or more atomic edits into a single Undo/Redo step.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Transaction {
    id: u64,
    edits: Vec<AtomicEdit>,
    cursors_before: Vec<Cursor>,
    cursors_after: Vec<Cursor>,
}

/// High-performance text buffer backed by `ropey::Rope`.
#[derive(Debug, Clone)]
pub struct TextBuffer {
    id: u64,
    file_path: Option<PathBuf>,
    rope: Rope,
    cursors: Vec<Cursor>,
    /// Id of the transaction on top of the undo stack when the buffer was last saved or loaded
    /// (`None` means "the empty history"). The buffer is dirty whenever the current top differs,
    /// so undoing back to the saved state makes the buffer clean again.
    saved_tx_id: Option<u64>,
    /// Character class of the last single-character typing transaction, used to coalesce
    /// consecutive keystrokes of the same class into one undo step. `None` seals the history.
    typing_coalesce: Option<CharClass>,
    version: i32,
    undo_stack: Vec<Transaction>,
    redo_stack: Vec<Transaction>,
    active_transaction: Option<Transaction>,
    next_tx_id: u64,
}

impl TextBuffer {
    /// Creates a new buffer with the given ID and initial text content.
    pub fn new(id: u64, text: &str) -> Self {
        Self {
            id,
            file_path: None,
            rope: Rope::from_str(text),
            cursors: vec![Cursor::new(0)],
            saved_tx_id: None,
            typing_coalesce: None,
            version: 1,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            active_transaction: None,
            next_tx_id: 1,
        }
    }

    /// Loads a `TextBuffer` from a file path on disk.
    pub fn from_file(id: u64, path: impl AsRef<Path>) -> Result<Self, BufferError> {
        let path_ref = path.as_ref();
        let content = fs::read_to_string(path_ref)?;
        let mut buf = Self::new(id, &content);
        buf.file_path = Some(path_ref.to_path_buf());
        buf.mark_clean();
        Ok(buf)
    }

    /// Saves the buffer contents to its associated file path, or returns an error if unset.
    pub fn save(&mut self) -> Result<(), BufferError> {
        let path = self.file_path.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Buffer has no file path")
        })?;
        let path = path.clone();
        write_file_atomically(&path, &self.text())?;
        self.mark_clean();
        Ok(())
    }

    /// Saves the buffer contents to the specified file path and updates `file_path`.
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<(), BufferError> {
        let path_buf = path.as_ref().to_path_buf();
        write_file_atomically(&path_buf, &self.text())?;
        self.file_path = Some(path_buf);
        self.mark_clean();
        Ok(())
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn file_path(&self) -> Option<&Path> {
        self.file_path.as_deref()
    }

    pub fn set_file_path(&mut self, path: impl Into<PathBuf>) {
        self.file_path = Some(path.into());
    }

    /// Returns true when the content differs from the last saved/loaded state.
    pub fn is_dirty(&self) -> bool {
        let pending = self
            .active_transaction
            .as_ref()
            .is_some_and(|tx| !tx.edits.is_empty());
        pending || self.undo_stack.last().map(|t| t.id) != self.saved_tx_id
    }

    /// Marks the current content as the saved state (e.g. after saving or reloading from disk).
    pub fn mark_clean(&mut self) {
        if self.active_transaction.is_some() {
            self.commit_transaction();
        }
        self.saved_tx_id = self.undo_stack.last().map(|t| t.id);
        self.typing_coalesce = None;
    }

    /// Replaces the whole content (e.g. file changed on disk) as a single undoable step,
    /// keeping the primary cursor at the same line/column where possible.
    pub fn replace_all_text(&mut self, new_text: &str) -> Vec<BufferEdit> {
        let (line, col) = self
            .cursors
            .first()
            .map(|c| self.char_to_line_col(c.head))
            .unwrap_or((0, 0));
        self.typing_coalesce = None;
        let len = self.rope.len_chars();
        let edit = self.replace_range(0, len, new_text);
        self.set_cursor_position(line, col, false);
        vec![edit]
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn len_utf16_cu(&self) -> usize {
        self.rope.len_utf16_cu()
    }

    pub fn len_lines(&self) -> u32 {
        self.rope.len_lines() as u32
    }

    /// Returns the text of a 0-indexed line without trailing newline characters (`\n` or `\r\n`).
    pub fn line_text(&self, line_idx: u32) -> Option<String> {
        let idx = line_idx as usize;
        if idx >= self.rope.len_lines() {
            return None;
        }
        let mut s = self.rope.line(idx).to_string();
        if s.ends_with('\n') {
            s.pop();
            if s.ends_with('\r') {
                s.pop();
            }
        }
        Some(s)
    }

    /// Returns the number of characters on a given 0-indexed line (excluding trailing `\r`/`\n`).
    pub fn line_len_chars(&self, line_idx: u32) -> usize {
        let idx = line_idx as usize;
        if idx >= self.rope.len_lines() {
            return 0;
        }
        let slice = self.rope.line(idx);
        let mut len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && slice.char(len - 1) == '\r' {
                len -= 1;
            }
        }
        len
    }

    // =========================================================================
    // UTF-8 / UTF-16 / Char / Line-Col Offset Conversions
    // =========================================================================

    pub fn char_to_byte(&self, char_idx: usize) -> usize {
        let clamped = char_idx.min(self.rope.len_chars());
        self.rope.char_to_byte(clamped)
    }

    pub fn byte_to_char(&self, byte_idx: usize) -> usize {
        let clamped = byte_idx.min(self.rope.len_bytes());
        self.rope.byte_to_char(clamped)
    }

    pub fn char_to_utf16_cu(&self, char_idx: usize) -> usize {
        let clamped = char_idx.min(self.rope.len_chars());
        self.rope.char_to_utf16_cu(clamped)
    }

    pub fn utf16_cu_to_char(&self, utf16_idx: usize) -> usize {
        let clamped = utf16_idx.min(self.rope.len_utf16_cu());
        self.rope.utf16_cu_to_char(clamped)
    }

    pub fn byte_to_utf16_cu(&self, byte_idx: usize) -> usize {
        let char_idx = self.byte_to_char(byte_idx);
        self.char_to_utf16_cu(char_idx)
    }

    pub fn utf16_cu_to_byte(&self, utf16_idx: usize) -> usize {
        let char_idx = self.utf16_cu_to_char(utf16_idx);
        self.char_to_byte(char_idx)
    }

    /// Converts a (0-indexed line, 0-indexed char column) into a buffer char index.
    pub fn line_col_to_char(&self, line: u32, column: u32) -> usize {
        let total_lines = self.rope.len_lines();
        if total_lines == 0 {
            return 0;
        }
        let clamped_line = (line as usize).min(total_lines - 1);
        let line_start = self.rope.line_to_char(clamped_line);
        let max_col = self.line_len_chars(clamped_line as u32);
        line_start + (column as usize).min(max_col)
    }

    /// Converts a buffer char index into (0-indexed line, 0-indexed char column).
    pub fn char_to_line_col(&self, char_idx: usize) -> (u32, u32) {
        let clamped = char_idx.min(self.rope.len_chars());
        let line = self.rope.char_to_line(clamped);
        let line_start = self.rope.line_to_char(line);
        (line as u32, (clamped - line_start) as u32)
    }

    /// Converts an LSP-style (0-indexed line, 0-indexed UTF-16 code unit column) to a char index.
    pub fn utf16_line_col_to_char(&self, line: u32, utf16_col: u32) -> usize {
        let total_lines = self.rope.len_lines();
        if total_lines == 0 {
            return 0;
        }
        let clamped_line = (line as usize).min(total_lines - 1);
        let line_slice = self.rope.line(clamped_line);
        let line_char_len = self.line_len_chars(clamped_line as u32);
        let line_content_slice = line_slice.slice(..line_char_len);
        let clamped_utf16 = (utf16_col as usize).min(line_content_slice.len_utf16_cu());
        let col_chars = line_content_slice.utf16_cu_to_char(clamped_utf16);
        self.rope.line_to_char(clamped_line) + col_chars
    }

    /// Converts a char index to an LSP-style (0-indexed line, 0-indexed UTF-16 code unit column).
    pub fn char_to_utf16_line_col(&self, char_idx: usize) -> (u32, u32) {
        let clamped = char_idx.min(self.rope.len_chars());
        let line = self.rope.char_to_line(clamped);
        let line_start = self.rope.line_to_char(line);
        let col_chars = clamped - line_start;
        let line_slice = self.rope.line(line);
        let utf16_col = line_slice.slice(..col_chars).len_utf16_cu();
        (line as u32, utf16_col as u32)
    }

    /// Converts a char index to a `BytePoint` (0-indexed row, byte offset within row) for `tree-sitter`.
    pub fn char_to_byte_point(&self, char_idx: usize) -> BytePoint {
        let clamped = char_idx.min(self.rope.len_chars());
        let row = self.rope.char_to_line(clamped);
        let line_start_byte = self.rope.line_to_byte(row);
        let target_byte = self.rope.char_to_byte(clamped);
        BytePoint {
            row,
            column: target_byte - line_start_byte,
        }
    }

    // =========================================================================
    // Multi-Cursor Management
    // =========================================================================

    pub fn cursors(&self) -> &[Cursor] {
        &self.cursors
    }

    /// Sets the primary cursor to `(line, column)` and clears secondary cursors.
    /// If `select` is true, extends the primary cursor's selection from its existing anchor.
    pub fn set_cursor_position(&mut self, line: u32, column: u32, select: bool) {
        let pos = self.line_col_to_char(line, column);
        let anchor = if select {
            self.cursors.first().map(|c| c.anchor).unwrap_or(pos)
        } else {
            pos
        };
        self.cursors = vec![Cursor {
            anchor,
            head: pos,
            preferred_column: Some(column),
        }];
    }

    /// Adds a new cursor at `(line, column)` for multi-cursor editing.
    pub fn add_cursor(&mut self, line: u32, column: u32) {
        let pos = self.line_col_to_char(line, column);
        self.cursors.push(Cursor {
            anchor: pos,
            head: pos,
            preferred_column: Some(column),
        });
        self.normalize_cursors();
    }

    /// Adds a cursor with an explicit selection range.
    pub fn add_cursor_selection(
        &mut self,
        anchor_line: u32,
        anchor_col: u32,
        head_line: u32,
        head_col: u32,
    ) {
        let anchor = self.line_col_to_char(anchor_line, anchor_col);
        let head = self.line_col_to_char(head_line, head_col);
        self.cursors.push(Cursor::with_selection(anchor, head));
        self.normalize_cursors();
    }

    /// Moves all active cursors in `direction`. If `select` is true, preserves anchors.
    pub fn move_cursors(&mut self, direction: CursorDirection, select: bool) {
        let len_chars = self.rope.len_chars();
        let total_lines = self.len_lines();

        let mut updated = Vec::with_capacity(self.cursors.len());
        for mut cursor in self.cursors.iter().copied() {
            let (line, col) = self.char_to_line_col(cursor.head);
            match direction {
                CursorDirection::Left => {
                    if !select && cursor.has_selection() {
                        let (start, _) = cursor.selection_range();
                        cursor.head = start;
                    } else if cursor.head > 0 {
                        cursor.head -= 1;
                    }
                    cursor.preferred_column = None;
                }
                CursorDirection::Right => {
                    if !select && cursor.has_selection() {
                        let (_, end) = cursor.selection_range();
                        cursor.head = end;
                    } else if cursor.head < len_chars {
                        cursor.head += 1;
                    }
                    cursor.preferred_column = None;
                }
                CursorDirection::Up => {
                    let pref_col = cursor.preferred_column.unwrap_or(col);
                    if line > 0 {
                        cursor.head = self.line_col_to_char(line - 1, pref_col);
                    } else {
                        cursor.head = 0;
                    }
                    cursor.preferred_column = Some(pref_col);
                }
                CursorDirection::Down => {
                    let pref_col = cursor.preferred_column.unwrap_or(col);
                    if line + 1 < total_lines {
                        cursor.head = self.line_col_to_char(line + 1, pref_col);
                    } else {
                        cursor.head = len_chars;
                    }
                    cursor.preferred_column = Some(pref_col);
                }
                CursorDirection::LineStart => {
                    cursor.head = self.line_col_to_char(line, 0);
                    cursor.preferred_column = Some(0);
                }
                CursorDirection::LineEnd => {
                    let end_col = self.line_len_chars(line) as u32;
                    cursor.head = self.line_col_to_char(line, end_col);
                    cursor.preferred_column = Some(end_col);
                }
                CursorDirection::DocumentStart => {
                    cursor.head = 0;
                    cursor.preferred_column = Some(0);
                }
                CursorDirection::DocumentEnd => {
                    cursor.head = len_chars;
                    cursor.preferred_column = None;
                }
                CursorDirection::WordLeft => {
                    cursor.head = self.word_boundary_left(cursor.head);
                    cursor.preferred_column = None;
                }
                CursorDirection::WordRight => {
                    cursor.head = self.word_boundary_right(cursor.head);
                    cursor.preferred_column = None;
                }
            }
            if !select {
                cursor.anchor = cursor.head;
            }
            updated.push(cursor);
        }
        self.cursors = updated;
        self.normalize_cursors();
    }

    /// Sorts cursors by position and merges any overlapping or duplicate cursors.
    fn normalize_cursors(&mut self) {
        if self.cursors.is_empty() {
            self.cursors.push(Cursor::new(0));
            return;
        }
        self.cursors
            .sort_by_key(|c| (min(c.anchor, c.head), max(c.anchor, c.head)));

        let mut merged: Vec<Cursor> = Vec::with_capacity(self.cursors.len());
        for c in self.cursors.drain(..) {
            if let Some(prev) = merged.last_mut() {
                let (p_start, p_end) = prev.selection_range();
                let (c_start, c_end) = c.selection_range();
                let overlaps = if prev.has_selection() || c.has_selection() {
                    c_start < p_end || (c_start == p_start && c_end == p_end)
                } else {
                    c_start == p_start
                };
                if overlaps {
                    let new_start = min(p_start, c_start);
                    let new_end = max(p_end, c_end);
                    if c.head >= c.anchor {
                        prev.anchor = new_start;
                        prev.head = new_end;
                    } else {
                        prev.anchor = new_end;
                        prev.head = new_start;
                    }
                    continue;
                }
            }
            merged.push(c);
        }
        self.cursors = merged;
    }

    // =========================================================================
    // Transaction Grouping & Undo / Redo History
    // =========================================================================

    /// Begins an explicit transaction group so multiple edits are undone/redone together.
    pub fn begin_transaction(&mut self) {
        self.typing_coalesce = None;
        if self.active_transaction.is_none() {
            let id = self.next_tx_id;
            self.next_tx_id += 1;
            self.active_transaction = Some(Transaction {
                id,
                edits: Vec::new(),
                cursors_before: self.cursors.clone(),
                cursors_after: self.cursors.clone(),
            });
        }
    }

    /// Commits the current active transaction group to the undo stack.
    pub fn commit_transaction(&mut self) {
        if let Some(mut tx) = self.active_transaction.take() {
            if !tx.edits.is_empty() {
                tx.cursors_after = self.cursors.clone();
                self.undo_stack.push(tx);
                self.redo_stack.clear();
            }
        }
    }

    // =========================================================================
    // Core Editing Operations
    // =========================================================================

    /// Internal primitive that replaces `[start_char..end_char]` with `new_text`,
    /// returning the exact `BufferEdit` for `tree-sitter` and the `AtomicEdit` for Undo/Redo.
    fn apply_raw_edit(
        &mut self,
        start_char: usize,
        end_char: usize,
        new_text: &str,
    ) -> (BufferEdit, AtomicEdit) {
        let s_char = start_char.min(self.rope.len_chars());
        let e_char = end_char.max(s_char).min(self.rope.len_chars());

        let start_byte = self.rope.char_to_byte(s_char);
        let old_end_byte = self.rope.char_to_byte(e_char);
        let start_point = self.char_to_byte_point(s_char);
        let old_end_point = self.char_to_byte_point(e_char);

        let deleted_text = self.rope.slice(s_char..e_char).to_string();

        if s_char < e_char {
            self.rope.remove(s_char..e_char);
        }
        if !new_text.is_empty() {
            self.rope.insert(s_char, new_text);
        }

        let inserted_chars = new_text.chars().count();
        let new_end_char = s_char + inserted_chars;
        let new_end_byte = self.rope.char_to_byte(new_end_char);
        let new_end_point = self.char_to_byte_point(new_end_char);

        self.version += 1;

        let buffer_edit = BufferEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_point,
            old_end_point,
            new_end_point,
        };

        let atomic_edit = AtomicEdit {
            start_char: s_char,
            deleted_text,
            inserted_text: new_text.to_string(),
        };

        (buffer_edit, atomic_edit)
    }

    /// Replaces an explicit character range `[start_char..end_char]` with `new_text`.
    pub fn replace_range(
        &mut self,
        start_char: usize,
        end_char: usize,
        new_text: &str,
    ) -> BufferEdit {
        let auto_tx = self.active_transaction.is_none();
        if auto_tx {
            self.begin_transaction();
        }

        let (buf_edit, atomic) = self.apply_raw_edit(start_char, end_char, new_text);
        if let Some(tx) = self.active_transaction.as_mut() {
            tx.edits.push(atomic);
        }

        let new_pos = start_char.min(self.rope.len_chars()) + new_text.chars().count();
        self.cursors = vec![Cursor::new(new_pos.min(self.rope.len_chars()))];

        if auto_tx {
            self.commit_transaction();
        }
        buf_edit
    }

    /// Inserts `text` at all active cursors (replacing any non-empty selections).
    /// Returns the list of `BufferEdit`s in the exact sequence applied.
    pub fn insert_text(&mut self, text: &str) -> Vec<BufferEdit> {
        let auto_tx = self.active_transaction.is_none();
        let typing_class = {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if char_class(c) != CharClass::Newline => Some(char_class(c)),
                _ => None,
            }
        };
        let no_selection = self.cursors.iter().all(|c| !c.has_selection());
        let coalesce = auto_tx
            && no_selection
            && typing_class.is_some()
            && self.typing_coalesce == typing_class
            && self.redo_stack.is_empty()
            && self
                .undo_stack
                .last()
                .is_some_and(|tx| tx.cursors_after == self.cursors);
        if coalesce {
            // Re-open the previous typing transaction so this keystroke joins it.
            self.active_transaction = self.undo_stack.pop();
        } else if auto_tx {
            self.begin_transaction();
        }

        self.normalize_cursors();
        let inserted_char_len = text.chars().count();
        let mut edits = Vec::with_capacity(self.cursors.len());
        let mut new_cursors = Vec::with_capacity(self.cursors.len());
        let mut char_delta: isize = 0;

        let current_cursors = self.cursors.clone();
        for cursor in current_cursors {
            let (raw_start, raw_end) = cursor.selection_range();
            let start = (raw_start as isize + char_delta).max(0) as usize;
            let end = (raw_end as isize + char_delta).max(start as isize) as usize;

            let (buf_edit, atomic) = self.apply_raw_edit(start, end, text);
            if let Some(tx) = self.active_transaction.as_mut() {
                tx.edits.push(atomic);
            }
            edits.push(buf_edit);

            let removed_len = end - start;
            char_delta += inserted_char_len as isize - removed_len as isize;
            let new_head = start + inserted_char_len;
            new_cursors.push(Cursor::new(new_head));
        }

        self.cursors = new_cursors;
        self.normalize_cursors();

        if auto_tx {
            self.commit_transaction();
            self.typing_coalesce = typing_class;
        }
        edits
    }

    /// Deletes backward (Backspace) at all active cursors.
    pub fn delete_backward(&mut self) -> Vec<BufferEdit> {
        let auto_tx = self.active_transaction.is_none();
        if auto_tx {
            self.begin_transaction();
        }

        self.normalize_cursors();
        let mut edits = Vec::with_capacity(self.cursors.len());
        let mut new_cursors = Vec::with_capacity(self.cursors.len());
        let mut char_delta: isize = 0;

        let current_cursors = self.cursors.clone();
        for cursor in current_cursors {
            let (raw_start, raw_end) = cursor.selection_range();
            let start = (raw_start as isize + char_delta).max(0) as usize;
            let end = (raw_end as isize + char_delta).max(start as isize) as usize;

            let (del_start, del_end) = if start < end {
                (start, end)
            } else if start > 0 {
                (start - 1, start)
            } else {
                new_cursors.push(Cursor::new(0));
                continue;
            };

            let (buf_edit, atomic) = self.apply_raw_edit(del_start, del_end, "");
            if let Some(tx) = self.active_transaction.as_mut() {
                tx.edits.push(atomic);
            }
            edits.push(buf_edit);

            let removed = del_end - del_start;
            char_delta -= removed as isize;
            new_cursors.push(Cursor::new(del_start));
        }

        self.cursors = new_cursors;
        self.normalize_cursors();

        if auto_tx {
            self.commit_transaction();
        }
        edits
    }

    /// Deletes forward (Delete key) at all active cursors.
    pub fn delete_forward(&mut self) -> Vec<BufferEdit> {
        let auto_tx = self.active_transaction.is_none();
        if auto_tx {
            self.begin_transaction();
        }

        self.normalize_cursors();
        let mut edits = Vec::with_capacity(self.cursors.len());
        let mut new_cursors = Vec::with_capacity(self.cursors.len());
        let mut char_delta: isize = 0;

        let current_cursors = self.cursors.clone();
        for cursor in current_cursors {
            let (raw_start, raw_end) = cursor.selection_range();
            let start = (raw_start as isize + char_delta).max(0) as usize;
            let end = (raw_end as isize + char_delta).max(start as isize) as usize;

            let len_chars = self.rope.len_chars();
            let (del_start, del_end) = if start < end {
                (start, end)
            } else if start < len_chars {
                (start, start + 1)
            } else {
                new_cursors.push(Cursor::new(start.min(len_chars)));
                continue;
            };

            let (buf_edit, atomic) = self.apply_raw_edit(del_start, del_end, "");
            if let Some(tx) = self.active_transaction.as_mut() {
                tx.edits.push(atomic);
            }
            edits.push(buf_edit);

            let removed = del_end - del_start;
            char_delta -= removed as isize;
            new_cursors.push(Cursor::new(del_start));
        }

        self.cursors = new_cursors;
        self.normalize_cursors();

        if auto_tx {
            self.commit_transaction();
        }
        edits
    }

    /// Undoes the most recent transaction and returns the resulting `BufferEdit`s.
    pub fn undo(&mut self) -> Vec<BufferEdit> {
        self.typing_coalesce = None;
        if self.active_transaction.is_some() {
            self.commit_transaction();
        }
        let Some(tx) = self.undo_stack.pop() else {
            return Vec::new();
        };

        let mut edits = Vec::with_capacity(tx.edits.len());
        for atomic in tx.edits.iter().rev() {
            let inserted_len = atomic.inserted_text.chars().count();
            let (buf_edit, _) = self.apply_raw_edit(
                atomic.start_char,
                atomic.start_char + inserted_len,
                &atomic.deleted_text,
            );
            edits.push(buf_edit);
        }

        self.cursors = tx.cursors_before.clone();
        self.normalize_cursors();
        self.redo_stack.push(tx);
        edits
    }

    /// Redoes the most recently undone transaction and returns the resulting `BufferEdit`s.
    pub fn redo(&mut self) -> Vec<BufferEdit> {
        self.typing_coalesce = None;
        if self.active_transaction.is_some() {
            self.commit_transaction();
        }
        let Some(tx) = self.redo_stack.pop() else {
            return Vec::new();
        };

        let mut edits = Vec::with_capacity(tx.edits.len());
        for atomic in &tx.edits {
            let deleted_len = atomic.deleted_text.chars().count();
            let (buf_edit, _) = self.apply_raw_edit(
                atomic.start_char,
                atomic.start_char + deleted_len,
                &atomic.inserted_text,
            );
            edits.push(buf_edit);
        }

        self.cursors = tx.cursors_after.clone();
        self.normalize_cursors();
        self.undo_stack.push(tx);
        edits
    }


    // =========================================================================
    // Selection, Clipboard & Block Editing Helpers
    // =========================================================================

    /// Returns the char index of the previous word boundary (VS Code style Ctrl+Left).
    pub fn word_boundary_left(&self, pos: usize) -> usize {
        let mut i = pos.min(self.rope.len_chars());
        if i == 0 {
            return 0;
        }
        // A newline directly before the cursor is its own stop.
        if char_class(self.rope.char(i - 1)) == CharClass::Newline {
            return i - 1;
        }
        while i > 0 && char_class(self.rope.char(i - 1)) == CharClass::Whitespace {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let class = char_class(self.rope.char(i - 1));
        if class == CharClass::Newline {
            return i;
        }
        while i > 0 && char_class(self.rope.char(i - 1)) == class {
            i -= 1;
        }
        i
    }

    /// Returns the char index of the next word boundary (VS Code style Ctrl+Right).
    pub fn word_boundary_right(&self, pos: usize) -> usize {
        let len = self.rope.len_chars();
        let mut i = pos.min(len);
        if i >= len {
            return len;
        }
        if char_class(self.rope.char(i)) == CharClass::Newline {
            return i + 1;
        }
        while i < len && char_class(self.rope.char(i)) == CharClass::Whitespace {
            i += 1;
        }
        if i >= len {
            return len;
        }
        let class = char_class(self.rope.char(i));
        if class == CharClass::Newline {
            return i;
        }
        while i < len && char_class(self.rope.char(i)) == class {
            i += 1;
        }
        i
    }

    /// Moves all cursors vertically by `delta` lines (negative = up), e.g. for PageUp/PageDown.
    pub fn move_cursors_by_lines(&mut self, delta: i64, select: bool) {
        let total_lines = self.len_lines() as i64;
        let mut updated = Vec::with_capacity(self.cursors.len());
        for mut cursor in self.cursors.iter().copied() {
            let (line, col) = self.char_to_line_col(cursor.head);
            let pref_col = cursor.preferred_column.unwrap_or(col);
            let target = line as i64 + delta;
            cursor.head = if target < 0 {
                0
            } else if target >= total_lines {
                self.rope.len_chars()
            } else {
                self.line_col_to_char(target as u32, pref_col)
            };
            cursor.preferred_column = Some(pref_col);
            if !select {
                cursor.anchor = cursor.head;
            }
            updated.push(cursor);
        }
        self.cursors = updated;
        self.normalize_cursors();
    }

    /// Selects the whole document with a single cursor.
    pub fn select_all(&mut self) {
        self.cursors = vec![Cursor::with_selection(0, self.rope.len_chars())];
    }

    /// Returns the text covered by all non-empty selections joined with `\n`, or `None`
    /// when no cursor has a selection.
    pub fn selected_text(&self) -> Option<String> {
        let parts: Vec<String> = self
            .cursors
            .iter()
            .filter(|c| c.has_selection())
            .map(|c| {
                let (s, e) = c.selection_range();
                self.rope.slice(s..e).to_string()
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    /// Selects the full line(s) of every cursor (including the trailing newline), used by
    /// "cut line" when nothing is selected.
    pub fn select_cursor_lines(&mut self) {
        let total = self.rope.len_lines();
        let mut updated = Vec::with_capacity(self.cursors.len());
        for c in self.cursors.iter() {
            let (s, e) = c.selection_range();
            let (sl, _) = self.char_to_line_col(s);
            let (el, _) = self.char_to_line_col(e);
            let start = self.rope.line_to_char(sl as usize);
            let end = if (el as usize) + 1 < total {
                self.rope.line_to_char(el as usize + 1)
            } else {
                self.rope.len_chars()
            };
            updated.push(Cursor::with_selection(start, end));
        }
        self.cursors = updated;
        self.normalize_cursors();
    }

    /// Returns the sorted, de-duplicated set of line indices touched by any cursor or selection.
    /// A selection ending at column 0 of a line does not include that line (VS Code behavior).
    pub fn cursor_line_set(&self) -> Vec<u32> {
        let mut lines = std::collections::BTreeSet::new();
        for c in &self.cursors {
            let (s, e) = c.selection_range();
            let (sl, _) = self.char_to_line_col(s);
            let (mut el, ec) = self.char_to_line_col(e);
            if c.has_selection() && ec == 0 && el > sl {
                el -= 1;
            }
            for l in sl..=el {
                lines.insert(l);
            }
        }
        lines.into_iter().collect()
    }

    /// Returns the leading whitespace of a line.
    pub fn line_indent(&self, line_idx: u32) -> String {
        self.line_text(line_idx)
            .unwrap_or_default()
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// Inserts a newline at every cursor, carrying over the current line's indentation and
    /// adding one extra indent level after an opening bracket (`{`, `(`, `[`) or a `:` line end.
    pub fn insert_newline_with_indent(&mut self, indent_unit: &str) -> Vec<BufferEdit> {
        let cursor_count = self.cursors.len();
        if cursor_count == 1 {
            let c = self.cursors[0];
            let (s, _) = c.selection_range();
            let (line, col) = self.char_to_line_col(s);
            let line_text = self.line_text(line).unwrap_or_default();
            let indent = self.line_indent(line);
            let before: String = line_text.chars().take(col as usize).collect();
            let after: String = line_text.chars().skip(col as usize).collect();
            let opens = matches!(before.trim_end().chars().last(), Some('{' | '(' | '[' | ':'));
            let closes_next = matches!(after.trim_start().chars().next(), Some('}' | ')' | ']'));
            if opens && closes_next {
                // `{|}` -> "{\n    |\n}"
                self.begin_transaction();
                let mut edits = self.insert_text(&format!("\n{indent}{indent_unit}"));
                let resume = self.cursors[0].head;
                edits.extend(self.insert_text(&format!("\n{indent}")));
                self.cursors = vec![Cursor::new(resume)];
                self.commit_transaction();
                return edits;
            }
            let extra = if opens { indent_unit } else { "" };
            return self.insert_text(&format!("\n{indent}{extra}"));
        }
        self.insert_text("\n")
    }

    /// Indents every line touched by the cursors by `indent_unit` (Tab with a selection).
    pub fn indent_lines(&mut self, indent_unit: &str) -> Vec<BufferEdit> {
        let lines = self.cursor_line_set();
        let saved: Vec<((u32, u32), (u32, u32))> = self
            .cursors
            .iter()
            .map(|c| (self.char_to_line_col(c.anchor), self.char_to_line_col(c.head)))
            .collect();
        let unit_len = indent_unit.chars().count() as u32;
        self.begin_transaction();
        let mut edits = Vec::new();
        for &l in lines.iter().rev() {
            let start = self.rope.line_to_char(l as usize);
            let (e, a) = self.apply_raw_edit(start, start, indent_unit);
            if let Some(tx) = self.active_transaction.as_mut() {
                tx.edits.push(a);
            }
            edits.push(e);
        }
        // Like VS Code, a selection edge at column 0 stays at column 0 so whole-line
        // selections keep covering the inserted indentation.
        let shifted = |line: u32, col: u32| {
            if lines.contains(&line) && col > 0 {
                col + unit_len
            } else {
                col
            }
        };
        self.cursors = saved
            .into_iter()
            .map(|((al, ac), (hl, hc))| Cursor {
                anchor: self.line_col_to_char(al, shifted(al, ac)),
                head: self.line_col_to_char(hl, shifted(hl, hc)),
                preferred_column: None,
            })
            .collect();
        self.normalize_cursors();
        self.commit_transaction();
        edits
    }

    /// Removes up to one indent level (`indent_width` spaces or one tab) from every touched line.
    pub fn outdent_lines(&mut self, indent_width: usize) -> Vec<BufferEdit> {
        let lines = self.cursor_line_set();
        let primary_line_col: Vec<((u32, u32), (u32, u32))> = self
            .cursors
            .iter()
            .map(|c| (self.char_to_line_col(c.anchor), self.char_to_line_col(c.head)))
            .collect();
        self.begin_transaction();
        let mut edits = Vec::new();
        let mut removed_per_line = std::collections::BTreeMap::new();
        for &l in lines.iter().rev() {
            let text = self.line_text(l).unwrap_or_default();
            let remove = if text.starts_with('\t') {
                1
            } else {
                text.chars().take(indent_width).take_while(|c| *c == ' ').count()
            };
            if remove == 0 {
                continue;
            }
            let start = self.rope.line_to_char(l as usize);
            let (e, a) = self.apply_raw_edit(start, start + remove, "");
            if let Some(tx) = self.active_transaction.as_mut() {
                tx.edits.push(a);
            }
            edits.push(e);
            removed_per_line.insert(l, remove as u32);
        }
        self.cursors = primary_line_col
            .into_iter()
            .map(|((al, ac), (hl, hc))| {
                let ra = removed_per_line.get(&al).copied().unwrap_or(0);
                let rh = removed_per_line.get(&hl).copied().unwrap_or(0);
                let anchor = self.line_col_to_char(al, ac.saturating_sub(ra));
                let head = self.line_col_to_char(hl, hc.saturating_sub(rh));
                Cursor {
                    anchor,
                    head,
                    preferred_column: None,
                }
            })
            .collect();
        self.normalize_cursors();
        self.commit_transaction();
        edits
    }

    /// Toggles a line comment (`prefix`, e.g. `//` or `#`) on every touched line. If all
    /// non-blank lines are already commented, the comment is removed; otherwise it is added at
    /// the minimum indentation column.
    pub fn toggle_line_comment(&mut self, prefix: &str) -> Vec<BufferEdit> {
        let lines = self.cursor_line_set();
        let texts: Vec<(u32, String)> = lines
            .iter()
            .map(|&l| (l, self.line_text(l).unwrap_or_default()))
            .collect();
        let non_blank: Vec<&(u32, String)> =
            texts.iter().filter(|(_, t)| !t.trim().is_empty()).collect();
        if non_blank.is_empty() {
            return Vec::new();
        }
        let all_commented = non_blank
            .iter()
            .all(|(_, t)| t.trim_start().starts_with(prefix));
        let min_indent = non_blank
            .iter()
            .map(|(_, t)| t.chars().take_while(|c| *c == ' ' || *c == '\t').count())
            .min()
            .unwrap_or(0);
        let saved: Vec<((u32, u32), (u32, u32))> = self
            .cursors
            .iter()
            .map(|c| (self.char_to_line_col(c.anchor), self.char_to_line_col(c.head)))
            .collect();
        let mut delta_per_line: std::collections::BTreeMap<u32, (u32, i64)> =
            std::collections::BTreeMap::new();
        self.begin_transaction();
        let mut edits = Vec::new();
        for (l, t) in texts.iter().rev() {
            if t.trim().is_empty() {
                continue;
            }
            let line_start = self.rope.line_to_char(*l as usize);
            if all_commented {
                let indent = t.chars().take_while(|c| *c == ' ' || *c == '\t').count();
                let after_prefix: String = t.chars().skip(indent + prefix.chars().count()).collect();
                let mut remove = prefix.chars().count();
                if after_prefix.starts_with(' ') {
                    remove += 1;
                }
                let s = line_start + indent;
                let (e, a) = self.apply_raw_edit(s, s + remove, "");
                if let Some(tx) = self.active_transaction.as_mut() {
                    tx.edits.push(a);
                }
                edits.push(e);
                delta_per_line.insert(*l, (indent as u32, -(remove as i64)));
            } else {
                let insert = format!("{prefix} ");
                let s = line_start + min_indent;
                let (e, a) = self.apply_raw_edit(s, s, &insert);
                if let Some(tx) = self.active_transaction.as_mut() {
                    tx.edits.push(a);
                }
                edits.push(e);
                delta_per_line.insert(*l, (min_indent as u32, insert.chars().count() as i64));
            }
        }
        let adjust = |line: u32, col: u32| -> u32 {
            match delta_per_line.get(&line) {
                Some(&(at, d)) if col >= at => (col as i64 + d).max(at as i64) as u32,
                _ => col,
            }
        };
        self.cursors = saved
            .into_iter()
            .map(|((al, ac), (hl, hc))| Cursor {
                anchor: self.line_col_to_char(al, adjust(al, ac)),
                head: self.line_col_to_char(hl, adjust(hl, hc)),
                preferred_column: None,
            })
            .collect();
        self.normalize_cursors();
        self.commit_transaction();
        edits
    }

    /// Selects the next occurrence of the primary selection (or the word under the cursor when
    /// nothing is selected) and adds it as a new cursor (VS Code Ctrl+D). Returns true if a new
    /// occurrence was added or the word was selected.
    pub fn add_next_occurrence(&mut self) -> bool {
        let Some(last) = self.cursors.iter().max_by_key(|c| c.selection_range().1).copied() else {
            return false;
        };
        if !last.has_selection() {
            let start = self.word_start_at(last.head);
            let end = self.word_end_at(last.head);
            if start == end {
                return false;
            }
            self.cursors = vec![Cursor::with_selection(start, end)];
            return true;
        }
        let (s, e) = last.selection_range();
        let needle = self.rope.slice(s..e).to_string();
        let hay = self.rope.to_string();
        let from_byte = self.rope.char_to_byte(e);
        let found = hay[from_byte..]
            .find(&needle)
            .map(|b| from_byte + b)
            .or_else(|| hay.find(&needle));
        let Some(byte) = found else {
            return false;
        };
        let ns = self.rope.byte_to_char(byte);
        let ne = ns + needle.chars().count();
        if self.cursors.iter().any(|c| c.selection_range() == (ns, ne)) {
            return false;
        }
        self.cursors.push(Cursor::with_selection(ns, ne));
        self.normalize_cursors();
        true
    }

    fn word_start_at(&self, pos: usize) -> usize {
        let mut i = pos.min(self.rope.len_chars());
        while i > 0 && char_class(self.rope.char(i - 1)) == CharClass::Word {
            i -= 1;
        }
        i
    }

    fn word_end_at(&self, pos: usize) -> usize {
        let len = self.rope.len_chars();
        let mut i = pos.min(len);
        while i < len && char_class(self.rope.char(i)) == CharClass::Word {
            i += 1;
        }
        i
    }

    // =========================================================================
    // Fast String Search
    // =========================================================================

    /// Searches the buffer for `query` and returns all non-overlapping matches.
    pub fn search(&self, query: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let haystack = self.rope.to_string();
        let mut results = Vec::new();

        if case_sensitive {
            let mut search_from = 0;
            while let Some(pos) = haystack[search_from..].find(query) {
                let start_byte = search_from + pos;
                let end_byte = start_byte + query.len();
                let start_char = self.rope.byte_to_char(start_byte);
                let end_char = self.rope.byte_to_char(end_byte);
                let (line, column) = self.char_to_line_col(start_char);
                results.push(SearchMatch {
                    start_char,
                    end_char,
                    start_byte,
                    end_byte,
                    line,
                    column,
                });
                search_from = end_byte;
            }
        } else {
            let query_lower: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
            if query_lower.is_empty() {
                return Vec::new();
            }
            let chars: Vec<(usize, char)> = haystack.char_indices().collect();
            let mut i = 0;
            while i < chars.len() {
                let mut matched_chars = 0;
                let mut q_idx = 0;
                while i + matched_chars < chars.len() && q_idx < query_lower.len() {
                    let c_lower: Vec<char> = chars[i + matched_chars]
                        .1
                        .to_lowercase()
                        .collect();
                    if q_idx + c_lower.len() > query_lower.len() {
                        break;
                    }
                    if query_lower[q_idx..q_idx + c_lower.len()] == c_lower[..] {
                        q_idx += c_lower.len();
                        matched_chars += 1;
                    } else {
                        break;
                    }
                }
                if q_idx == query_lower.len() {
                    let start_char = i;
                    let end_char = i + matched_chars;
                    let start_byte = chars[start_char].0;
                    let end_byte = if end_char < chars.len() {
                        chars[end_char].0
                    } else {
                        haystack.len()
                    };
                    let (line, column) = self.char_to_line_col(start_char);
                    results.push(SearchMatch {
                        start_char,
                        end_char,
                        start_byte,
                        end_byte,
                        line,
                        column,
                    });
                    i = end_char;
                } else {
                    i += 1;
                }
            }
        }

        results
    }
}

/// Writes `contents` to `path` atomically: a sibling temporary file is written, flushed and
/// then renamed over the destination, so a crash never leaves a half-written file. Existing
/// file permissions are preserved.
fn write_file_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "buffer".to_string());
    let tmp = dir.join(format!(".{file_name}.levity-save-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        if let Ok(meta) = fs::metadata(path) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_and_delete() {
        let mut buf = TextBuffer::new(1, "fn main() {}");
        buf.set_cursor_position(0, 11, false);
        buf.insert_text("println!(\"Hi\");");
        assert_eq!(buf.text(), "fn main() {println!(\"Hi\");}");
        buf.delete_backward();
        assert_eq!(buf.text(), "fn main() {println!(\"Hi\")}");
    }

    #[test]
    fn test_vietnamese_unicode_insert_and_delete() {
        let mut buf = TextBuffer::new(1, "Xin chào ");
        buf.set_cursor_position(0, 9, false);
        buf.insert_text("Việt Nam thân yêu");
        assert_eq!(buf.text(), "Xin chào Việt Nam thân yêu");
        assert_eq!(buf.len_chars(), 26);

        // Delete backward "u", "ê", "y", " "
        for _ in 0..4 {
            buf.delete_backward();
        }
        assert_eq!(buf.text(), "Xin chào Việt Nam thân");
    }

    #[test]
    fn test_emoji_surrogate_pairs_utf8_utf16_conversion() {
        // 🚀 (U+1F680) is 4 UTF-8 bytes, 2 UTF-16 code units, 1 Unicode scalar value (char).
        // Tiếng Việt: "Đ" (U+0110) is 2 UTF-8 bytes, 1 UTF-16 code unit, 1 char.
        let mut buf = TextBuffer::new(1, "A🚀BĐC");
        assert_eq!(buf.len_chars(), 5);
        assert_eq!(buf.len_bytes(), 1 + 4 + 1 + 2 + 1);
        assert_eq!(buf.len_utf16_cu(), 1 + 2 + 1 + 1 + 1);

        // Check char <-> byte conversions
        assert_eq!(buf.char_to_byte(0), 0);
        assert_eq!(buf.char_to_byte(1), 1); // 🚀 starts at byte 1
        assert_eq!(buf.char_to_byte(2), 5); // 'B' starts at byte 5
        assert_eq!(buf.char_to_byte(3), 6); // 'Đ' starts at byte 6
        assert_eq!(buf.char_to_byte(4), 8); // 'C' starts at byte 8

        // Check char <-> UTF-16 code unit conversions
        assert_eq!(buf.char_to_utf16_cu(0), 0);
        assert_eq!(buf.char_to_utf16_cu(1), 1);
        assert_eq!(buf.char_to_utf16_cu(2), 3); // after 🚀 (2 UTF-16 units)
        assert_eq!(buf.utf16_cu_to_char(3), 2);
        assert_eq!(buf.byte_to_utf16_cu(5), 3);
        assert_eq!(buf.utf16_cu_to_byte(3), 5);

        // Delete forward at '🚀' (char index 1)
        buf.set_cursor_position(0, 1, false);
        buf.delete_forward();
        assert_eq!(buf.text(), "ABĐC");
    }

    #[test]
    fn test_utf16_line_col_conversion_multiline() {
        let buf = TextBuffer::new(1, "Chào 🌍\nLập trình 🦀 Rust");
        // Line 1: "Lập trình 🦀 Rust"
        // "Lập trình " is 10 chars, 10 UTF-16 CUs.
        // "🦀" is 1 char, 2 UTF-16 CUs (cols 10..12).
        // " Rust" starts at char col 11, UTF-16 col 12.
        let char_idx = buf.utf16_line_col_to_char(1, 12);
        let (line, col) = buf.char_to_line_col(char_idx);
        assert_eq!((line, col), (1, 11));
        assert_eq!(buf.char_to_utf16_line_col(char_idx), (1, 12));
    }

    #[test]
    fn test_multi_cursor_insert_and_delete() {
        let mut buf = TextBuffer::new(1, "item1\nitem2\nitem3");
        buf.set_cursor_position(0, 0, false);
        buf.add_cursor(1, 0);
        buf.add_cursor(2, 0);

        let edits = buf.insert_text("let ");
        assert_eq!(edits.len(), 3);
        assert_eq!(buf.text(), "let item1\nlet item2\nlet item3");

        buf.delete_backward();
        assert_eq!(buf.text(), "letitem1\nletitem2\nletitem3");
    }

    #[test]
    fn test_multi_cursor_selection_replacement() {
        let mut buf = TextBuffer::new(1, "foo = 1;\nfoo = 2;\nfoo = 3;");
        buf.set_cursor_position(0, 0, false);
        buf.set_cursor_position(0, 3, true); // select "foo" on line 0
        buf.add_cursor_selection(1, 0, 1, 3); // select "foo" on line 1
        buf.add_cursor_selection(2, 0, 2, 3); // select "foo" on line 2

        buf.insert_text("biến_số");
        assert_eq!(buf.text(), "biến_số = 1;\nbiến_số = 2;\nbiến_số = 3;");
    }

    #[test]
    fn test_transaction_grouping_undo_redo() {
        let mut buf = TextBuffer::new(1, "Hello");
        buf.set_cursor_position(0, 5, false);

        buf.begin_transaction();
        buf.insert_text(" ");
        buf.insert_text("Thế");
        buf.insert_text(" Giới");
        buf.insert_text("!");
        buf.commit_transaction();

        assert_eq!(buf.text(), "Hello Thế Giới!");

        // Single undo should revert the entire transaction!
        buf.undo();
        assert_eq!(buf.text(), "Hello");

        // Single redo should reapply the entire transaction!
        buf.redo();
        assert_eq!(buf.text(), "Hello Thế Giới!");
    }

    #[test]
    fn test_multi_cursor_undo_redo() {
        let mut buf = TextBuffer::new(1, "a\nb\nc");
        buf.set_cursor_position(0, 1, false);
        buf.add_cursor(1, 1);
        buf.add_cursor(2, 1);

        buf.insert_text(" = 42;");
        assert_eq!(buf.text(), "a = 42;\nb = 42;\nc = 42;");

        buf.undo();
        assert_eq!(buf.text(), "a\nb\nc");
        assert_eq!(buf.cursors().len(), 3);

        buf.redo();
        assert_eq!(buf.text(), "a = 42;\nb = 42;\nc = 42;");
    }

    #[test]
    fn test_cursor_movement_and_selection() {
        let mut buf = TextBuffer::new(1, "dòng một\ndòng hai dài hơn\ndòng ba");
        buf.set_cursor_position(0, 4, false);
        buf.move_cursors(CursorDirection::Down, false);
        assert_eq!(buf.char_to_line_col(buf.cursors()[0].head), (1, 4));

        buf.move_cursors(CursorDirection::LineEnd, true);
        let (start, end) = buf.cursors()[0].selection_range();
        assert_eq!(buf.rope().slice(start..end).to_string(), " hai dài hơn");

        buf.move_cursors(CursorDirection::DocumentStart, false);
        assert_eq!(buf.cursors()[0].head, 0);
        assert!(!buf.cursors()[0].has_selection());
    }

    #[test]
    fn test_fast_search_case_sensitive_and_insensitive() {
        let buf = TextBuffer::new(
            1,
            "fn tính_toán() {\n    let Tính_Toán = 10;\n    tính_toán();\n}",
        );

        let exact = buf.search("tính_toán", true);
        assert_eq!(exact.len(), 2);
        assert_eq!((exact[0].line, exact[0].column), (0, 3));
        assert_eq!((exact[1].line, exact[1].column), (2, 4));

        let insensitive = buf.search("tính_toán", false);
        assert_eq!(insensitive.len(), 3);
        assert_eq!((insensitive[1].line, insensitive[1].column), (1, 8));
    }

    #[test]
    fn test_buffer_edit_coordinates_for_incremental_parsing() {
        let mut buf = TextBuffer::new(1, "let x = 1;\nlet y = 2;");
        buf.set_cursor_position(0, 8, false);
        buf.set_cursor_position(0, 9, true); // select '1'
        let edits = buf.insert_text("100");
        assert_eq!(edits.len(), 1);
        let edit = edits[0];
        assert_eq!(edit.start_byte, 8);
        assert_eq!(edit.old_end_byte, 9);
        assert_eq!(edit.new_end_byte, 11);
        assert_eq!(edit.start_point, BytePoint { row: 0, column: 8 });
        assert_eq!(edit.old_end_point, BytePoint { row: 0, column: 9 });
        assert_eq!(edit.new_end_point, BytePoint { row: 0, column: 11 });
    }

    #[test]
    fn test_word_movement_and_page_moves() {
        let mut buf = TextBuffer::new(1, "let foo_bar = baz(1);\nnext line\nthird\nfourth\n");
        buf.move_cursors(CursorDirection::WordRight, false);
        assert_eq!(buf.cursors()[0].head, 3);
        buf.move_cursors(CursorDirection::WordRight, false);
        assert_eq!(buf.cursors()[0].head, 11);
        buf.move_cursors(CursorDirection::WordLeft, false);
        assert_eq!(buf.cursors()[0].head, 4);
        buf.move_cursors(CursorDirection::WordRight, true);
        assert_eq!(buf.selected_text().as_deref(), Some("foo_bar"));
        buf.set_cursor_position(0, 2, false);
        buf.move_cursors_by_lines(2, false);
        assert_eq!(buf.char_to_line_col(buf.cursors()[0].head), (2, 2));
        buf.move_cursors_by_lines(-10, false);
        assert_eq!(buf.cursors()[0].head, 0);
        buf.move_cursors_by_lines(100, true);
        assert_eq!(buf.cursors()[0].head, buf.len_chars());
    }

    #[test]
    fn test_select_all_selected_text_and_cut_line_selection() {
        let mut buf = TextBuffer::new(1, "alpha\nbeta\ngamma");
        assert_eq!(buf.selected_text(), None);
        buf.select_all();
        assert_eq!(buf.selected_text().as_deref(), Some("alpha\nbeta\ngamma"));
        buf.set_cursor_position(1, 2, false);
        buf.select_cursor_lines();
        assert_eq!(buf.selected_text().as_deref(), Some("beta\n"));
        buf.delete_backward();
        assert_eq!(buf.text(), "alpha\ngamma");
    }

    #[test]
    fn test_dirty_tracking_returns_clean_after_undo_to_save_point() {
        let dir = std::env::temp_dir().join(format!("levity-buf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dirty.txt");
        fs::write(&path, "abc").unwrap();
        let mut buf = TextBuffer::from_file(1, &path).unwrap();
        assert!(!buf.is_dirty());
        buf.set_cursor_position(0, 3, false);
        buf.insert_text("d");
        assert!(buf.is_dirty());
        buf.undo();
        assert!(!buf.is_dirty(), "undo back to the saved content must be clean");
        buf.redo();
        assert!(buf.is_dirty());
        buf.save().unwrap();
        assert!(!buf.is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "abcd");
        // No temporary save files are left behind.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("levity-save"))
            .collect();
        assert!(leftovers.is_empty());
        buf.insert_text("e");
        assert!(buf.is_dirty());
        buf.undo();
        assert!(!buf.is_dirty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_typing_is_coalesced_into_word_sized_undo_steps() {
        let mut buf = TextBuffer::new(1, "");
        for ch in "hello world".chars() {
            buf.insert_text(&ch.to_string());
        }
        assert_eq!(buf.text(), "hello world");
        buf.undo();
        assert_eq!(buf.text(), "hello ");
        buf.undo();
        assert_eq!(buf.text(), "hello");
        buf.undo();
        assert_eq!(buf.text(), "");
        // Moving the cursor breaks the group.
        buf.insert_text("a");
        buf.insert_text("b");
        buf.move_cursors(CursorDirection::Left, false);
        buf.insert_text("c");
        assert_eq!(buf.text(), "acb");
        buf.undo();
        assert_eq!(buf.text(), "ab");
    }

    #[test]
    fn test_indent_outdent_and_toggle_comment_blocks() {
        let mut buf = TextBuffer::new(1, "fn a() {\nlet x = 1;\nlet y = 2;\n}\n");
        buf.set_cursor_position(1, 0, false);
        buf.set_cursor_position(2, 3, true);
        buf.indent_lines("    ");
        assert_eq!(buf.text(), "fn a() {\n    let x = 1;\n    let y = 2;\n}\n");
        assert_eq!(buf.selected_text().as_deref(), Some("    let x = 1;\n    let"));
        buf.outdent_lines(4);
        assert_eq!(buf.text(), "fn a() {\nlet x = 1;\nlet y = 2;\n}\n");
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "fn a() {\n// let x = 1;\n// let y = 2;\n}\n");
        buf.toggle_line_comment("//");
        assert_eq!(buf.text(), "fn a() {\nlet x = 1;\nlet y = 2;\n}\n");
        buf.undo();
        assert_eq!(buf.text(), "fn a() {\n// let x = 1;\n// let y = 2;\n}\n");
    }

    #[test]
    fn test_newline_auto_indent_and_bracket_expansion() {
        let mut buf = TextBuffer::new(1, "    fn a() {}");
        buf.set_cursor_position(0, 12, false);
        buf.insert_newline_with_indent("    ");
        assert_eq!(buf.text(), "    fn a() {\n        \n    }");
        assert_eq!(buf.char_to_line_col(buf.cursors()[0].head), (1, 8));
        buf.undo();
        assert_eq!(buf.text(), "    fn a() {}");
        let mut plain = TextBuffer::new(2, "  value");
        plain.set_cursor_position(0, 7, false);
        plain.insert_newline_with_indent("    ");
        assert_eq!(plain.text(), "  value\n  ");
    }

    #[test]
    fn test_add_next_occurrence_multi_cursor() {
        let mut buf = TextBuffer::new(1, "foo bar foo baz foo");
        buf.set_cursor_position(0, 1, false);
        assert!(buf.add_next_occurrence());
        assert_eq!(buf.selected_text().as_deref(), Some("foo"));
        assert!(buf.add_next_occurrence());
        assert!(buf.add_next_occurrence());
        assert_eq!(buf.cursors().len(), 3);
        assert!(!buf.add_next_occurrence());
        buf.insert_text("qux");
        assert_eq!(buf.text(), "qux bar qux baz qux");
    }
}
