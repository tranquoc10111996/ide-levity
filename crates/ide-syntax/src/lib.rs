//! `ide-syntax`: Incremental syntax parsing and highlighting powered by `tree-sitter`.
//! Built-in grammar support for Rust, TypeScript/TSX, JavaScript, Python, JSON, Markdown, Kotlin, and Swift.

use ide_buffer::{BufferEdit, BytePoint, Rope, TextBuffer};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
pub use tree_sitter::Node;
use tree_sitter::{InputEdit, Language, Parser, Point, Tree};

#[derive(Debug, Error)]
pub enum SyntaxError {
    #[error("Failed to configure tree-sitter language: {0}")]
    LanguageError(#[from] tree_sitter::LanguageError),
    #[error("Parser failed to produce a syntax tree")]
    ParseFailed,
}

/// Font style flags for `HighlightSpan.font_style`.
pub const FONT_STYLE_NORMAL: u8 = 0;
pub const FONT_STYLE_BOLD: u8 = 1;
pub const FONT_STYLE_ITALIC: u8 = 2;
pub const FONT_STYLE_BOLD_ITALIC: u8 = 3;

/// Default syntax theme colors in `0xRRGGBBAA` format (GitHub Dark / Antigravity inspired).
pub mod theme {
    pub const KEYWORD: u32 = 0xFF7B_72FF;
    pub const FUNCTION: u32 = 0xD2A8_FFFF;
    pub const TYPE: u32 = 0xFFA6_57FF;
    pub const STRING: u32 = 0xA5D6_FFFF;
    pub const NUMBER: u32 = 0x79C0_FFFF;
    pub const COMMENT: u32 = 0x8B94_9EFF;
    pub const OPERATOR: u32 = 0xFF7B_72FF;
    pub const PUNCTUATION: u32 = 0xC9D1_D9FF;
    pub const PROPERTY: u32 = 0x79C0_FFFF;
    pub const MACRO: u32 = 0xD2A8_FFFF;
    pub const HEADING: u32 = 0x58A6_FFFF;
    pub const IDENTIFIER: u32 = 0xE6ED_F3FF;
}

/// A highlighted span within a single `RenderedLine`.
/// Columns (`start_col`, `end_col`) are 0-indexed character offsets within the line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpan {
    pub start_col: u32,
    pub end_col: u32,
    pub color_rgba: u32,
    pub font_style: u8,
}

/// Supported programming and markup languages in `ide-syntax`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyntaxLanguage {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Json,
    Markdown,
    Kotlin,
    Swift,
    PlainText,
}

impl SyntaxLanguage {
    /// Detects the syntax language from a file path extension.
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        let ext = path
            .as_ref()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Self::from_extension(&ext)
    }

    /// Detects the syntax language from a file extension string.
    pub fn from_extension(ext: &str) -> Self {
        match ext.trim_start_matches('.') {
            "rs" => Self::Rust,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" | "pyi" | "pyw" => Self::Python,
            "json" | "jsonc" => Self::Json,
            "md" | "markdown" => Self::Markdown,
            "kt" | "kts" => Self::Kotlin,
            "swift" => Self::Swift,
            _ => Self::PlainText,
        }
    }

    /// Returns the corresponding `tree_sitter::Language` if supported.
    pub fn tree_sitter_language(self) -> Option<Language> {
        match self {
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Self::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
            Self::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Self::Json => Some(tree_sitter_json::LANGUAGE.into()),
            Self::Markdown => Some(tree_sitter_md::LANGUAGE.into()),
            Self::Kotlin => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
            Self::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
            Self::PlainText => None,
        }
    }
}

fn to_ts_point(bp: BytePoint) -> Point {
    Point {
        row: bp.row,
        column: bp.column,
    }
}

/// Converts a `BufferEdit` from `ide-buffer` into a `tree_sitter::InputEdit`.
pub fn buffer_edit_to_input_edit(edit: &BufferEdit) -> InputEdit {
    InputEdit {
        start_byte: edit.start_byte,
        old_end_byte: edit.old_end_byte,
        new_end_byte: edit.new_end_byte,
        start_position: to_ts_point(edit.start_point),
        old_end_position: to_ts_point(edit.old_end_point),
        new_end_position: to_ts_point(edit.new_end_point),
    }
}

/// Incremental syntax analyzer maintaining a `tree-sitter` parser and syntax tree for a buffer.
pub struct SyntaxEngine {
    language: SyntaxLanguage,
    parser: Parser,
    tree: Option<Tree>,
    edit_count: u64,
}

impl SyntaxEngine {
    /// Creates a new `SyntaxEngine` for the given `SyntaxLanguage`.
    pub fn new(language: SyntaxLanguage) -> Result<Self, SyntaxError> {
        let mut parser = Parser::new();
        if let Some(ts_lang) = language.tree_sitter_language() {
            parser.set_language(&ts_lang)?;
        }
        Ok(Self {
            language,
            parser,
            tree: None,
            edit_count: 0,
        })
    }

    pub fn language(&self) -> SyntaxLanguage {
        self.language
    }

    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    pub fn edit_count(&self) -> u64 {
        self.edit_count
    }

    /// Switches the language and performs a full parse of `buffer`.
    pub fn set_language(
        &mut self,
        language: SyntaxLanguage,
        buffer: &TextBuffer,
    ) -> Result<(), SyntaxError> {
        self.language = language;
        self.tree = None;
        if let Some(ts_lang) = language.tree_sitter_language() {
            self.parser.set_language(&ts_lang)?;
            self.parse_full(buffer)?;
        }
        Ok(())
    }

    /// Performs a full syntax parse of the given `TextBuffer`.
    pub fn parse_full(&mut self, buffer: &TextBuffer) -> Result<(), SyntaxError> {
        if self.language == SyntaxLanguage::PlainText {
            self.tree = None;
            return Ok(());
        }
        let rope = buffer.rope();
        let tree = self
            .parser
            .parse_with(
                &mut |byte_idx, _point| {
                    if byte_idx >= rope.len_bytes() {
                        ""
                    } else {
                        let (chunk, chunk_byte_idx, _, _) = rope.chunk_at_byte(byte_idx);
                        &chunk[byte_idx - chunk_byte_idx..]
                    }
                },
                None,
            )
            .ok_or(SyntaxError::ParseFailed)?;
        self.tree = Some(tree);
        Ok(())
    }

    /// Updates the syntax tree incrementally using the `BufferEdit`s produced by `TextBuffer`,
    /// then re-parses only the changed regions via `tree-sitter`.
    pub fn apply_edits(
        &mut self,
        buffer: &TextBuffer,
        edits: &[BufferEdit],
    ) -> Result<(), SyntaxError> {
        if self.language == SyntaxLanguage::PlainText {
            return Ok(());
        }
        if edits.is_empty() && self.tree.is_some() {
            return Ok(());
        }

        if let Some(tree) = self.tree.as_mut() {
            for edit in edits {
                let input_edit = buffer_edit_to_input_edit(edit);
                tree.edit(&input_edit);
                self.edit_count += 1;
            }
        }

        let rope = buffer.rope();
        let new_tree = self
            .parser
            .parse_with(
                &mut |byte_idx, _point| {
                    if byte_idx >= rope.len_bytes() {
                        ""
                    } else {
                        let (chunk, chunk_byte_idx, _, _) = rope.chunk_at_byte(byte_idx);
                        &chunk[byte_idx - chunk_byte_idx..]
                    }
                },
                self.tree.as_ref(),
            )
            .ok_or(SyntaxError::ParseFailed)?;
        self.tree = Some(new_tree);
        Ok(())
    }

    /// Extracts `Vec<HighlightSpan>` for a single 0-indexed line.
    pub fn highlight_line(&self, buffer: &TextBuffer, line: u32) -> Vec<HighlightSpan> {
        self.highlight_lines(buffer, line, 1)
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// Extracts `Vec<HighlightSpan>` for each line in `[first_line .. first_line + line_count)`.
    pub fn highlight_lines(
        &self,
        buffer: &TextBuffer,
        first_line: u32,
        line_count: u32,
    ) -> Vec<Vec<HighlightSpan>> {
        let total_lines = buffer.len_lines();
        if line_count == 0 || first_line >= total_lines {
            return Vec::new();
        }
        let end_line = (first_line + line_count).min(total_lines);
        let num_lines = (end_line - first_line) as usize;
        let mut line_spans: Vec<Vec<HighlightSpan>> = vec![Vec::new(); num_lines];

        let Some(tree) = self.tree.as_ref() else {
            return line_spans;
        };

        let rope = buffer.rope();
        let region_start_byte = rope.line_to_byte(first_line as usize);
        let region_end_byte = if (end_line as usize) < rope.len_lines() {
            rope.line_to_byte(end_line as usize)
        } else {
            rope.len_bytes()
        };

        if region_start_byte >= region_end_byte {
            return line_spans;
        }

        // Precompute byte and char bounds for each visible line
        let mut line_bounds: Vec<(usize, usize, usize)> = Vec::with_capacity(num_lines);
        for line_idx in first_line..end_line {
            let l_usize = line_idx as usize;
            let l_start_char = rope.line_to_char(l_usize);
            let l_len_chars = buffer.line_len_chars(line_idx);
            let l_end_char = l_start_char + l_len_chars;
            let l_start_byte = rope.char_to_byte(l_start_char);
            let l_end_byte = rope.char_to_byte(l_end_char);
            line_bounds.push((l_start_byte, l_end_byte, l_start_char));
        }

        // Traverse AST nodes overlapping `[region_start_byte .. region_end_byte]`
        let root = tree.root_node();
        collect_highlights_in_range(
            root,
            region_start_byte,
            region_end_byte,
            rope,
            first_line,
            end_line,
            &line_bounds,
            &mut line_spans,
        );

        line_spans
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_highlights_in_range(
    node: Node<'_>,
    region_start_byte: usize,
    region_end_byte: usize,
    rope: &Rope,
    first_line: u32,
    end_line: u32,
    line_bounds: &[(usize, usize, usize)],
    line_spans: &mut [Vec<HighlightSpan>],
) {
    let n_start = node.start_byte();
    let n_end = node.end_byte();
    if n_end <= region_start_byte || n_start >= region_end_byte {
        return;
    }

    // Check if this node is a terminal/atomic highlight node (e.g., string, comment, markdown heading)
    // or a leaf token.
    let kind = node.kind();
    if is_atomic_highlight_node(kind) || node.child_count() == 0 {
        if let Some((color_rgba, font_style)) = classify_node(node) {
            emit_node_spans(
                n_start,
                n_end,
                color_rgba,
                font_style,
                rope,
                first_line,
                end_line,
                line_bounds,
                line_spans,
            );
        }
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child_for_byte(region_start_byte).is_some() {
        loop {
            let child = cursor.node();
            if child.start_byte() >= region_end_byte {
                break;
            }
            collect_highlights_in_range(
                child,
                region_start_byte,
                region_end_byte,
                rope,
                first_line,
                end_line,
                line_bounds,
                line_spans,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_node_spans(
    n_start: usize,
    n_end: usize,
    color_rgba: u32,
    font_style: u8,
    rope: &Rope,
    first_line: u32,
    end_line: u32,
    line_bounds: &[(usize, usize, usize)],
    line_spans: &mut [Vec<HighlightSpan>],
) {
    if n_start >= n_end {
        return;
    }
    let start_line = (rope.byte_to_line(n_start.min(rope.len_bytes())) as u32).max(first_line);
    let last_line = (rope.byte_to_line(n_end.saturating_sub(1).min(rope.len_bytes())) as u32)
        .min(end_line.saturating_sub(1));

    for line_idx in start_line..=last_line {
        let rel_idx = (line_idx - first_line) as usize;
        if rel_idx >= line_bounds.len() {
            break;
        }
        let (l_start_byte, l_end_byte, l_start_char) = line_bounds[rel_idx];
        let seg_start_byte = n_start.max(l_start_byte);
        let seg_end_byte = n_end.min(l_end_byte);
        if seg_start_byte >= seg_end_byte {
            continue;
        }
        let start_col = (rope.byte_to_char(seg_start_byte) - l_start_char) as u32;
        let end_col = (rope.byte_to_char(seg_end_byte) - l_start_char) as u32;
        if start_col < end_col {
            let spans = &mut line_spans[rel_idx];
            if spans.last().is_none_or(|prev| prev.end_col <= start_col) {
                spans.push(HighlightSpan {
                    start_col,
                    end_col,
                    color_rgba,
                    font_style,
                });
            }
        }
    }
}

fn is_atomic_highlight_node(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "string"
            | "template_string"
            | "line_string_literal"
            | "multi_line_string_literal"
            | "comment"
            | "line_comment"
            | "block_comment"
            | "multiline_comment"
            | "atx_heading"
            | "setext_heading"
            | "fenced_code_block"
    )
}

fn classify_node(node: Node<'_>) -> Option<(u32, u8)> {
    let kind = node.kind();
    let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");

    // 1. Comments
    if kind.contains("comment") || parent_kind.contains("comment") {
        return Some((theme::COMMENT, FONT_STYLE_ITALIC));
    }

    // 2. Markdown headings & code blocks
    if kind == "atx_heading" || kind == "setext_heading" || parent_kind.contains("heading") {
        return Some((theme::HEADING, FONT_STYLE_BOLD));
    }
    if kind == "fenced_code_block" {
        return Some((theme::STRING, FONT_STYLE_NORMAL));
    }

    // 3. Strings & characters
    if kind.contains("string")
        || kind == "char_literal"
        || kind == "character_literal"
        || parent_kind.contains("string")
    {
        if parent_kind == "pair" && node.parent().and_then(|p| p.child_by_field_name("key")) == Some(node) {
            return Some((theme::PROPERTY, FONT_STYLE_NORMAL));
        }
        return Some((theme::STRING, FONT_STYLE_NORMAL));
    }

    // 4. Numbers & Booleans
    if kind.contains("integer")
        || kind.contains("float")
        || kind == "number"
        || kind == "boolean_literal"
        || matches!(kind, "true" | "false" | "null" | "nil" | "None" | "True" | "False")
    {
        return Some((theme::NUMBER, FONT_STYLE_NORMAL));
    }

    // 5. Types
    if kind.contains("type")
        || kind == "primitive_type"
        || matches!(
            parent_kind,
            "type_identifier"
                | "user_type"
                | "class_declaration"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "interface_declaration"
                | "type_alias_declaration"
        ) && (kind.contains("identifier") || kind == "simple_identifier")
    {
        return Some((theme::TYPE, FONT_STYLE_NORMAL));
    }

    // 6. Functions & Methods & Macros
    if kind == "macro_invocation" || parent_kind == "macro_invocation" {
        return Some((theme::MACRO, FONT_STYLE_BOLD));
    }
    if (kind.contains("identifier") || kind == "simple_identifier")
        && matches!(
            parent_kind,
            "function_item"
                | "function_declaration"
                | "function_definition"
                | "method_declaration"
                | "call_expression"
                | "call"
                | "generic_function"
        )
    {
        return Some((theme::FUNCTION, FONT_STYLE_NORMAL));
    }

    // 7. Properties & Fields
    if kind == "field_identifier" || kind == "property_identifier" {
        return Some((theme::PROPERTY, FONT_STYLE_NORMAL));
    }

    // 8. Keywords (unnamed nodes that are alphabetic or known keyword kinds)
    if !node.is_named() && kind.chars().any(|c| c.is_ascii_alphabetic()) {
        return Some((theme::KEYWORD, FONT_STYLE_BOLD));
    }
    if matches!(
        kind,
        "fn" | "let"
            | "mut"
            | "pub"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "use"
            | "mod"
            | "return"
            | "if"
            | "else"
            | "match"
            | "for"
            | "while"
            | "loop"
            | "async"
            | "await"
            | "const"
            | "static"
            | "def"
            | "class"
            | "import"
            | "from"
            | "function"
            | "var"
            | "val"
            | "fun"
            | "package"
            | "interface"
            | "func"
            | "guard"
            | "self"
            | "super"
    ) {
        return Some((theme::KEYWORD, FONT_STYLE_BOLD));
    }

    // 9. Operators & Punctuation
    if !node.is_named() {
        if matches!(
            kind,
            "+" | "-" | "*" | "/" | "=" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "->" | "=>"
                | "&&" | "||" | "!" | "&" | "|" | "^" | "+=" | "-="
        ) {
            return Some((theme::OPERATOR, FONT_STYLE_NORMAL));
        }
        return Some((theme::PUNCTUATION, FONT_STYLE_NORMAL));
    }

    // 10. General Identifiers
    if kind.contains("identifier") {
        return Some((theme::IDENTIFIER, FONT_STYLE_NORMAL));
    }

    None
}

/// Convenience coordinator pairing a `TextBuffer` with a `SyntaxEngine` so any buffer mutation
/// automatically updates `tree-sitter`'s syntax tree via `InputEdit`.
pub struct HighlightedBuffer {
    pub buffer: TextBuffer,
    pub syntax: SyntaxEngine,
}

impl HighlightedBuffer {
    pub fn new(id: u64, text: &str, language: SyntaxLanguage) -> Result<Self, SyntaxError> {
        let buffer = TextBuffer::new(id, text);
        let mut syntax = SyntaxEngine::new(language)?;
        syntax.parse_full(&buffer)?;
        Ok(Self { buffer, syntax })
    }

    pub fn insert_text(&mut self, text: &str) -> Result<(), SyntaxError> {
        let edits = self.buffer.insert_text(text);
        self.syntax.apply_edits(&self.buffer, &edits)
    }

    pub fn delete_backward(&mut self) -> Result<(), SyntaxError> {
        let edits = self.buffer.delete_backward();
        self.syntax.apply_edits(&self.buffer, &edits)
    }

    pub fn delete_forward(&mut self) -> Result<(), SyntaxError> {
        let edits = self.buffer.delete_forward();
        self.syntax.apply_edits(&self.buffer, &edits)
    }

    pub fn undo(&mut self) -> Result<(), SyntaxError> {
        let edits = self.buffer.undo();
        self.syntax.apply_edits(&self.buffer, &edits)
    }

    pub fn redo(&mut self) -> Result<(), SyntaxError> {
        let edits = self.buffer.redo();
        self.syntax.apply_edits(&self.buffer, &edits)
    }

    pub fn highlight_lines(&self, first_line: u32, line_count: u32) -> Vec<Vec<HighlightSpan>> {
        self.syntax.highlight_lines(&self.buffer, first_line, line_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_initial_parse_and_highlighting() {
        let code = "fn cộng(a: i32, b: i32) -> i32 {\n    // Cộng hai số nguyên\n    a + b\n}\n";
        let hb = HighlightedBuffer::new(1, code, SyntaxLanguage::Rust).unwrap();
        let tree = hb.syntax.tree().expect("tree should exist");
        assert!(!tree.root_node().has_error());

        let spans = hb.highlight_lines(0, 4);
        assert_eq!(spans.len(), 4);
        // Line 0 starts with keyword `fn` (cols 0..2, bold, KEYWORD color)
        assert!(!spans[0].is_empty());
        assert_eq!(spans[0][0].start_col, 0);
        assert_eq!(spans[0][0].end_col, 2);
        assert_eq!(spans[0][0].color_rgba, theme::KEYWORD);
        assert_eq!(spans[0][0].font_style, FONT_STYLE_BOLD);

        // Line 1 is a comment
        assert_eq!(spans[1].len(), 1);
        assert_eq!(spans[1][0].color_rgba, theme::COMMENT);
        assert_eq!(spans[1][0].font_style, FONT_STYLE_ITALIC);
    }

    #[test]
    fn test_incremental_parsing_on_buffer_edits_and_undo_redo() {
        let mut hb = HighlightedBuffer::new(
            1,
            "fn main() {\n    let x = 1;\n}\n",
            SyntaxLanguage::Rust,
        )
        .unwrap();
        assert_eq!(hb.syntax.edit_count(), 0);

        // Edit line 1: replace `1` with `"Xin chào 🚀"`
        hb.buffer.set_cursor_position(1, 12, false);
        hb.buffer.set_cursor_position(1, 13, true);
        hb.insert_text("\"Xin chào 🚀\"").unwrap();

        assert_eq!(hb.syntax.edit_count(), 1);
        assert!(!hb.syntax.tree().unwrap().root_node().has_error());

        let line1_spans = hb.syntax.highlight_line(&hb.buffer, 1);
        assert!(line1_spans.iter().any(|s| s.color_rgba == theme::STRING));

        // Undo and verify incremental tree remains valid
        hb.undo().unwrap();
        assert_eq!(hb.syntax.edit_count(), 2);
        assert!(!hb.syntax.tree().unwrap().root_node().has_error());
        let line1_spans_after_undo = hb.syntax.highlight_line(&hb.buffer, 1);
        assert!(line1_spans_after_undo
            .iter()
            .any(|s| s.color_rgba == theme::NUMBER));

        // Redo and verify incremental tree
        hb.redo().unwrap();
        assert_eq!(hb.syntax.edit_count(), 3);
        assert!(!hb.syntax.tree().unwrap().root_node().has_error());
    }

    #[test]
    fn test_typescript_and_javascript_grammars() {
        let ts_code = "interface NgườiDùng {\n  tên: string;\n  tuổi: number;\n}\n";
        let hb_ts = HighlightedBuffer::new(1, ts_code, SyntaxLanguage::TypeScript).unwrap();
        assert!(!hb_ts.syntax.tree().unwrap().root_node().has_error());
        let ts_spans = hb_ts.highlight_lines(0, 4);
        assert!(!ts_spans[0].is_empty());

        let js_code = "function xinChào() {\n  return \"Chào buổi sáng 🌟\";\n}\n";
        let hb_js = HighlightedBuffer::new(2, js_code, SyntaxLanguage::JavaScript).unwrap();
        assert!(!hb_js.syntax.tree().unwrap().root_node().has_error());
        let js_spans = hb_js.highlight_lines(0, 3);
        assert!(js_spans[1].iter().any(|s| s.color_rgba == theme::STRING));
    }

    #[test]
    fn test_python_and_json_grammars() {
        let py_code = "def tính_tổng(danh_sách):\n    # Trả về tổng\n    return sum(danh_sách)\n";
        let hb_py = HighlightedBuffer::new(1, py_code, SyntaxLanguage::Python).unwrap();
        assert!(!hb_py.syntax.tree().unwrap().root_node().has_error());
        let py_spans = hb_py.highlight_lines(0, 3);
        assert_eq!(py_spans[0][0].color_rgba, theme::KEYWORD);
        assert_eq!(py_spans[1][0].color_rgba, theme::COMMENT);

        let json_code = "{\n  \"tên\": \"IDE Levity\",\n  \"phiên_bản\": 1,\n  \"tự_hành\": true\n}";
        let hb_json = HighlightedBuffer::new(2, json_code, SyntaxLanguage::Json).unwrap();
        assert!(!hb_json.syntax.tree().unwrap().root_node().has_error());
        let json_spans = hb_json.highlight_lines(0, 5);
        assert!(json_spans[1].iter().any(|s| s.color_rgba == theme::PROPERTY));
        assert!(json_spans[1].iter().any(|s| s.color_rgba == theme::STRING));
    }

    #[test]
    fn test_markdown_kotlin_and_swift_grammars() {
        let md_code = "# Tiêu Đề Lớn\n\nNội dung đoạn văn bản tiếng Việt.\n";
        let hb_md = HighlightedBuffer::new(1, md_code, SyntaxLanguage::Markdown).unwrap();
        assert!(!hb_md.syntax.tree().unwrap().root_node().has_error());
        let md_spans = hb_md.highlight_lines(0, 3);
        assert_eq!(md_spans[0][0].color_rgba, theme::HEADING);

        let kt_code = "fun main() {\n    val thôngĐiệp = \"Xin chào Kotlin\"\n}\n";
        let hb_kt = HighlightedBuffer::new(2, kt_code, SyntaxLanguage::Kotlin).unwrap();
        assert!(!hb_kt.syntax.tree().unwrap().root_node().has_error());
        let kt_spans = hb_kt.highlight_lines(0, 3);
        assert!(!kt_spans[0].is_empty());

        let swift_code = "func chàoMừng() -> String {\n    return \"Xin chào Swift\"\n}\n";
        let hb_swift = HighlightedBuffer::new(3, swift_code, SyntaxLanguage::Swift).unwrap();
        assert!(!hb_swift.syntax.tree().unwrap().root_node().has_error());
        let swift_spans = hb_swift.highlight_lines(0, 3);
        assert!(!swift_spans[0].is_empty());
    }

    #[test]
    fn test_multi_cursor_incremental_syntax_update() {
        let mut hb = HighlightedBuffer::new(
            1,
            "let a = 1;\nlet b = 2;\nlet c = 3;\n",
            SyntaxLanguage::Rust,
        )
        .unwrap();
        hb.buffer.set_cursor_position(0, 9, false);
        hb.buffer.add_cursor(1, 9);
        hb.buffer.add_cursor(2, 9);

        hb.insert_text("00").unwrap();
        assert_eq!(hb.buffer.text(), "let a = 100;\nlet b = 200;\nlet c = 300;\n");
        assert_eq!(hb.syntax.edit_count(), 3);
        assert!(!hb.syntax.tree().unwrap().root_node().has_error());
    }

    #[test]
    fn test_language_detection_from_path() {
        assert_eq!(SyntaxLanguage::from_path("src/main.rs"), SyntaxLanguage::Rust);
        assert_eq!(SyntaxLanguage::from_path("app/index.tsx"), SyntaxLanguage::Tsx);
        assert_eq!(SyntaxLanguage::from_path("app/util.ts"), SyntaxLanguage::TypeScript);
        assert_eq!(SyntaxLanguage::from_path("app/index.js"), SyntaxLanguage::JavaScript);
        assert_eq!(SyntaxLanguage::from_path("script.py"), SyntaxLanguage::Python);
        assert_eq!(SyntaxLanguage::from_path("package.json"), SyntaxLanguage::Json);
        assert_eq!(SyntaxLanguage::from_path("README.md"), SyntaxLanguage::Markdown);
        assert_eq!(SyntaxLanguage::from_path("MainActivity.kt"), SyntaxLanguage::Kotlin);
        assert_eq!(SyntaxLanguage::from_path("ContentView.swift"), SyntaxLanguage::Swift);
    }

    #[test]
    fn test_validate_android_kotlin_and_ios_swift_platform_sources() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        let kotlin_files = [
            "platforms/android/app/src/main/java/dev/levity/ide/MainActivity.kt",
            "platforms/android/app/src/main/java/dev/levity/ide/ui/IdeLevityApp.kt",
            "platforms/android/app/src/main/java/dev/levity/ide/ui/CodeEditorComposable.kt",
            "platforms/android/app/src/main/java/dev/levity/ide/ui/AgentManagerScreen.kt",
            "platforms/android/build.gradle.kts",
            "platforms/android/settings.gradle.kts",
            "platforms/android/app/build.gradle.kts",
        ];
        for rel in kotlin_files {
            let full = workspace_root.join(rel);
            let content = std::fs::read_to_string(&full).expect(rel);
            let hb = HighlightedBuffer::new(1, &content, SyntaxLanguage::Kotlin).unwrap();
            assert!(
                !hb.syntax.tree().unwrap().root_node().has_error(),
                "Kotlin syntax error in {rel}"
            );
        }

        let swift_files = [
            "platforms/ios/Package.swift",
            "platforms/ios/Sources/IDELevityiOS/IDELevityApp.swift",
            "platforms/ios/Sources/IDELevityiOS/CodeEditorView.swift",
            "platforms/ios/Sources/IDELevityiOS/AgentManagerView.swift",
        ];
        for rel in swift_files {
            let full = workspace_root.join(rel);
            let content = std::fs::read_to_string(&full).expect(rel);
            let hb = HighlightedBuffer::new(2, &content, SyntaxLanguage::Swift).unwrap();
            assert!(
                !hb.syntax.tree().unwrap().root_node().has_error(),
                "Swift syntax error in {rel}"
            );
        }
    }
}
