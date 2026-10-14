//! GUI-friendly value types and conversions from raw `lsp-types` responses.
//!
//! All positions are zero-based lines and UTF-16 code-unit columns, exactly as LSP
//! transmits them (this crate negotiates the `utf-16` position encoding).

use crate::LspError;
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Diagnostic,
    DiagnosticSeverity, Documentation, GotoDefinitionResponse, Hover, HoverContents,
    InsertTextFormat, MarkedString, NumberOrString, Position, Range, Uri,
};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// A position: zero-based line and UTF-16 column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LspPosition {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based UTF-16 code-unit column.
    pub character: u32,
}

/// A half-open range of [`LspPosition`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LspRange {
    /// Inclusive start.
    pub start: LspPosition,
    /// Exclusive end.
    pub end: LspPosition,
}

impl From<Position> for LspPosition {
    fn from(p: Position) -> Self {
        Self {
            line: p.line,
            character: p.character,
        }
    }
}

impl From<LspPosition> for Position {
    fn from(p: LspPosition) -> Self {
        Position {
            line: p.line,
            character: p.character,
        }
    }
}

impl From<Range> for LspRange {
    fn from(r: Range) -> Self {
        Self {
            start: r.start.into(),
            end: r.end.into(),
        }
    }
}

impl From<LspRange> for Range {
    fn from(r: LspRange) -> Self {
        Range {
            start: r.start.into(),
            end: r.end.into(),
        }
    }
}

/// A location in a file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Location {
    /// Absolute file path.
    pub path: PathBuf,
    /// Target range (for `LocationLink`s, the selection range of the target).
    pub range: LspRange,
}

/// A text replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTextEdit {
    /// Range to replace.
    pub range: LspRange,
    /// Replacement text.
    pub new_text: String,
}

/// Completion item kind (mirrors `CompletionItemKind`, values 1..=25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum CompletionKind {
    Text,
    Method,
    Function,
    Constructor,
    Field,
    Variable,
    Class,
    Interface,
    Module,
    Property,
    Unit,
    Value,
    Enum,
    Keyword,
    Snippet,
    Color,
    File,
    Reference,
    Folder,
    EnumMember,
    Constant,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

impl CompletionKind {
    /// Maps the protocol kind; unknown values yield `None`.
    pub fn from_lsp(kind: CompletionItemKind) -> Option<Self> {
        use CompletionKind::*;
        const ALL: [CompletionKind; 25] = [
            Text,
            Method,
            Function,
            Constructor,
            Field,
            Variable,
            Class,
            Interface,
            Module,
            Property,
            Unit,
            Value,
            Enum,
            Keyword,
            Snippet,
            Color,
            File,
            Reference,
            Folder,
            EnumMember,
            Constant,
            Struct,
            Event,
            Operator,
            TypeParameter,
        ];
        let raw: i32 = serde_json::to_value(kind).ok()?.as_i64()? as i32;
        ALL.get(usize::try_from(raw - 1).ok()?).copied()
    }
}

/// A completion proposal ready to be shown and applied by an editor.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionEntry {
    /// Text shown in the list.
    pub label: String,
    /// Item kind, for icons.
    pub kind: Option<CompletionKind>,
    /// Extra detail (signature, type).
    pub detail: Option<String>,
    /// Text to insert (snippet syntax already converted to plain text).
    pub insert_text: String,
    /// Range to replace with `insert_text`; `None` means "replace the word before the cursor".
    pub edit_range: Option<LspRange>,
    /// Documentation (markdown or plain text).
    pub documentation: Option<String>,
    /// Text used for filtering, defaults to the label.
    pub filter_text: Option<String>,
    /// Text used for sorting, defaults to the label.
    pub sort_text: Option<String>,
    /// Whether the server suggests preselecting this item.
    pub preselect: bool,
    /// Whether the item is deprecated.
    pub deprecated: bool,
    /// Extra edits to apply on acceptance (e.g. auto-imports).
    pub additional_edits: Vec<LspTextEdit>,
}

/// Diagnostic severity. Missing severities are treated as errors (like VS Code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticLevel {
    /// Error.
    Error,
    /// Warning.
    Warning,
    /// Information.
    Information,
    /// Hint.
    Hint,
}

/// A diagnostic published by a server.
#[derive(Debug, Clone, PartialEq)]
pub struct LspDiagnostic {
    /// Affected range.
    pub range: LspRange,
    /// Severity.
    pub severity: DiagnosticLevel,
    /// Message.
    pub message: String,
    /// Producer, e.g. `rustc`, `rust-analyzer`, `ts`, `Pyright`.
    pub source: Option<String>,
    /// Error code, e.g. `E0308`.
    pub code: Option<String>,
}

impl From<Diagnostic> for LspDiagnostic {
    fn from(d: Diagnostic) -> Self {
        let severity = match d.severity {
            Some(DiagnosticSeverity::WARNING) => DiagnosticLevel::Warning,
            Some(DiagnosticSeverity::INFORMATION) => DiagnosticLevel::Information,
            Some(DiagnosticSeverity::HINT) => DiagnosticLevel::Hint,
            _ => DiagnosticLevel::Error,
        };
        Self {
            range: d.range.into(),
            severity,
            message: d.message,
            source: d.source,
            code: d.code.map(|c| match c {
                NumberOrString::Number(n) => n.to_string(),
                NumberOrString::String(s) => s,
            }),
        }
    }
}

/// Converts an absolute filesystem path to a `file://` URI (percent-encoded).
pub fn path_to_uri(path: &Path) -> Result<Uri, LspError> {
    let absolute = std::path::absolute(path)
        .map_err(|e| LspError::InvalidUri(format!("{}: {e}", path.display())))?;
    let url = url::Url::from_file_path(&absolute)
        .map_err(|_| LspError::InvalidUri(absolute.display().to_string()))?;
    Uri::from_str(url.as_str()).map_err(|e| LspError::InvalidUri(format!("{url}: {e}")))
}

/// Converts a `file://` URI back to a filesystem path. Returns `None` for other schemes.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let url = url::Url::parse(uri.as_str()).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

/// Position just past the end of `text` (UTF-16 columns; `\n`, `\r\n` and `\r` all end lines).
pub fn text_end_position(text: &str) -> LspPosition {
    let mut line = 0u32;
    let mut character = 0u32;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                character = 0;
            }
            '\n' => {
                line += 1;
                character = 0;
            }
            other => character += other.len_utf16() as u32,
        }
    }
    LspPosition { line, character }
}

/// Flattens hover contents (`MarkedString`, arrays, `MarkupContent`) to markdown text.
pub fn hover_to_text(hover: &Hover) -> String {
    fn marked(m: &MarkedString) -> String {
        match m {
            MarkedString::String(s) => s.clone(),
            MarkedString::LanguageString(ls) => {
                format!("```{}\n{}\n```", ls.language, ls.value)
            }
        }
    }
    match &hover.contents {
        HoverContents::Scalar(m) => marked(m),
        HoverContents::Array(items) => items
            .iter()
            .map(marked)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(content) => content.value.clone(),
    }
}

/// Normalizes `Location`, `Location[]` and `LocationLink[]` definition responses.
pub fn definition_to_locations(resp: GotoDefinitionResponse) -> Vec<Location> {
    match resp {
        GotoDefinitionResponse::Scalar(loc) => lsp_location(&loc).into_iter().collect(),
        GotoDefinitionResponse::Array(locs) => locs.iter().filter_map(lsp_location).collect(),
        GotoDefinitionResponse::Link(links) => links
            .iter()
            .filter_map(|link| {
                Some(Location {
                    path: uri_to_path(&link.target_uri)?,
                    range: link.target_selection_range.into(),
                })
            })
            .collect(),
    }
}

/// Converts an `lsp_types::Location` (only `file://` URIs are kept).
pub fn lsp_location(loc: &lsp_types::Location) -> Option<Location> {
    Some(Location {
        path: uri_to_path(&loc.uri)?,
        range: loc.range.into(),
    })
}

/// Converts snippet syntax (`${1:name}`, `$0`, `${2|a,b|}`, `\$`) to the plain text a
/// user would see after accepting the snippet without tab-stops.
pub fn snippet_to_plain_text(snippet: &str) -> String {
    fn parse(chars: &[char], mut i: usize, out: &mut String, in_placeholder: bool) -> usize {
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
            } else if in_placeholder && c == '}' {
                return i + 1;
            } else if c == '$' && i + 1 < chars.len() {
                let next = chars[i + 1];
                if next.is_ascii_digit() {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                } else if next == '{' {
                    i += 2;
                    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_')
                    {
                        i += 1;
                    }
                    if i < chars.len() && chars[i] == ':' {
                        i = parse(chars, i + 1, out, true);
                    } else if i < chars.len() && chars[i] == '|' {
                        // Choice: take the first option.
                        i += 1;
                        let mut first = true;
                        while i < chars.len() && chars[i] != '|' {
                            if chars[i] == ',' {
                                first = false;
                            } else if first {
                                out.push(chars[i]);
                            }
                            i += 1;
                        }
                        // Skip "|}".
                        i = (i + 2).min(chars.len());
                    } else {
                        // `${1}` or unsupported transform: skip to the closing brace.
                        while i < chars.len() && chars[i] != '}' {
                            i += 1;
                        }
                        i = (i + 1).min(chars.len());
                    }
                } else {
                    out.push(c);
                    i += 1;
                }
            } else {
                out.push(c);
                i += 1;
            }
        }
        i
    }
    let chars: Vec<char> = snippet.chars().collect();
    let mut out = String::with_capacity(snippet.len());
    parse(&chars, 0, &mut out, false);
    out
}

/// Converts one raw completion item.
pub fn completion_entry(item: CompletionItem) -> CompletionEntry {
    let is_snippet = item.insert_text_format == Some(InsertTextFormat::SNIPPET);
    let plain = |s: String| if is_snippet { snippet_to_plain_text(&s) } else { s };
    let (insert_text, edit_range) = match item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (plain(edit.new_text), Some(edit.range.into())),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => {
            (plain(edit.new_text), Some(edit.insert.into()))
        }
        None => (
            plain(item.insert_text.unwrap_or_else(|| item.label.clone())),
            None,
        ),
    };
    let documentation = item.documentation.map(|doc| match doc {
        Documentation::String(s) => s,
        Documentation::MarkupContent(m) => m.value,
    });
    #[allow(deprecated)]
    let deprecated = item.deprecated.unwrap_or(false)
        || item
            .tags
            .as_ref()
            .is_some_and(|t| t.contains(&lsp_types::CompletionItemTag::DEPRECATED));
    let detail = match (item.detail, item.label_details) {
        (Some(d), _) => Some(d),
        (None, Some(ld)) => ld.detail.or(ld.description),
        (None, None) => None,
    };
    CompletionEntry {
        label: item.label,
        kind: item.kind.and_then(CompletionKind::from_lsp),
        detail,
        insert_text,
        edit_range,
        documentation,
        filter_text: item.filter_text,
        sort_text: item.sort_text,
        preselect: item.preselect.unwrap_or(false),
        deprecated,
        additional_edits: item
            .additional_text_edits
            .unwrap_or_default()
            .into_iter()
            .map(|e| LspTextEdit {
                range: e.range.into(),
                new_text: e.new_text,
            })
            .collect(),
    }
}

/// Normalizes a completion response (`CompletionList` or array).
pub fn completion_entries(resp: Option<CompletionResponse>) -> Vec<CompletionEntry> {
    let items = match resp {
        Some(CompletionResponse::Array(items)) => items,
        Some(CompletionResponse::List(list)) => list.items,
        None => Vec::new(),
    };
    items.into_iter().map(completion_entry).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{LanguageString, LocationLink, MarkupContent, MarkupKind, TextEdit};

    #[test]
    fn utf16_end_position_handles_emoji_vietnamese_and_line_endings() {
        assert_eq!(
            text_end_position(""),
            LspPosition {
                line: 0,
                character: 0
            }
        );
        assert_eq!(text_end_position("a😀").character, 3);
        assert_eq!(text_end_position("Tiếng Việt").character, 10);
        assert_eq!(
            text_end_position("a\r\nb\rcc\n"),
            LspPosition {
                line: 3,
                character: 0
            }
        );
        assert_eq!(
            text_end_position("x\ny😀"),
            LspPosition {
                line: 1,
                character: 3
            }
        );
    }

    #[test]
    fn uri_roundtrip_with_spaces_and_unicode() {
        let path = Path::new("/tmp/dự án/my file#1.rs");
        let uri = path_to_uri(path).unwrap();
        assert!(uri.as_str().starts_with("file:///tmp/"));
        assert!(uri.as_str().contains("%20"));
        assert!(!uri.as_str().contains('#'));
        assert_eq!(uri_to_path(&uri).unwrap(), path);
        let http = Uri::from_str("https://example.com/x").unwrap();
        assert!(uri_to_path(&http).is_none());
    }

    #[test]
    fn hover_flattening() {
        let h = Hover {
            contents: HoverContents::Array(vec![
                MarkedString::LanguageString(LanguageString {
                    language: "rust".into(),
                    value: "fn f()".into(),
                }),
                MarkedString::String("Docs".into()),
            ]),
            range: None,
        };
        assert_eq!(hover_to_text(&h), "```rust\nfn f()\n```\n\nDocs");
        let m = Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::PlainText,
                value: "plain".into(),
            }),
            range: None,
        };
        assert_eq!(hover_to_text(&m), "plain");
    }

    #[test]
    fn definition_links_use_selection_range() {
        let target = Uri::from_str("file:///w/src/a.rs").unwrap();
        let range = |l| Range {
            start: Position {
                line: l,
                character: 0,
            },
            end: Position {
                line: l,
                character: 1,
            },
        };
        let resp = GotoDefinitionResponse::Link(vec![LocationLink {
            origin_selection_range: None,
            target_uri: target,
            target_range: range(1),
            target_selection_range: range(3),
        }]);
        let locs = definition_to_locations(resp);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].path, PathBuf::from("/w/src/a.rs"));
        assert_eq!(locs[0].range.start.line, 3);
    }

    #[test]
    fn snippets_become_plain_text() {
        assert_eq!(snippet_to_plain_text("foo(${1:a}, ${2:b})$0"), "foo(a, b)");
        assert_eq!(snippet_to_plain_text("${1|one,two|} \\$x"), "one $x");
        assert_eq!(snippet_to_plain_text("if ${1:${2:nested}} {}"), "if nested {}");
        assert_eq!(snippet_to_plain_text("price $"), "price $");
    }

    #[test]
    fn completion_entry_prefers_text_edit() {
        let item = CompletionItem {
            label: "push_str".into(),
            kind: Some(CompletionItemKind::METHOD),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: Range::default(),
                new_text: "push_str(${1:s})".into(),
            })),
            ..Default::default()
        };
        let entry = completion_entry(item);
        assert_eq!(entry.insert_text, "push_str(s)");
        assert_eq!(entry.kind, Some(CompletionKind::Method));
        assert!(entry.edit_range.is_some());
        assert_eq!(
            CompletionKind::from_lsp(CompletionItemKind::TYPE_PARAMETER),
            Some(CompletionKind::TypeParameter)
        );
    }
}
