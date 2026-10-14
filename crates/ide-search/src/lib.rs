//! `ide-search`: gitignore-aware, project-wide search and replace for IDE Levity.
//!
//! This crate powers the "Search in files" panel (VS Code `Ctrl+Shift+F`).
//!
//! * [`search_workspace`] runs a parallel, `.gitignore`-aware walk and returns all
//!   matches sorted by path.
//! * [`search_workspace_with`] is the streaming core: it invokes a callback for every
//!   file as soon as that file has been searched, so a GUI can render results as they
//!   arrive.
//! * [`spawn_search`] runs a streaming search on a background thread and delivers
//!   [`SearchEvent`]s over a channel, which is the most convenient entry point for a
//!   GUI event loop that must never block.
//! * [`replace_in_files`] rewrites matching files on disk, supporting regex capture
//!   groups (`$1`, `${name}`) and preserving every line ending byte-for-byte.
//!
//! Matching is line based: a match never spans a line break, and zero-length matches
//! (for example `^` or `a*` on an empty position) are ignored by both search and
//! replace so that the counts reported by search and replace always agree.

mod matcher;
mod replace;
mod search;

pub use replace::{replace_in_files, ReplaceFailure, ReplaceOutcome};
pub use search::{
    search_workspace, search_workspace_with, spawn_search, SearchEvent, SearchHandle, SearchSummary,
};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Default cap on the total number of match ranges returned by a search.
pub const DEFAULT_MAX_RESULTS: usize = 20_000;

/// Default size above which files are skipped (8 MiB).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// Maximum number of characters kept in [`LineMatch::line_text`].
pub const MAX_PREVIEW_CHARS: usize = 250;

/// Number of characters of context kept before the first match when a long line is
/// trimmed for preview.
pub const PREVIEW_CONTEXT_CHARS: usize = 40;

/// Errors produced by search and replace operations.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum SearchError {
    /// The search pattern is not a valid regular expression.
    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),
    /// An include or exclude glob could not be parsed.
    #[error("invalid glob `{glob}`: {message}")]
    InvalidGlob {
        /// The offending glob.
        glob: String,
        /// Parser error message.
        message: String,
    },
    /// The search root does not exist or is not a directory.
    #[error("search root `{0}` is not a directory")]
    InvalidRoot(PathBuf),
}

/// A project-wide search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Text or regular expression to search for. An empty pattern matches nothing.
    pub pattern: String,
    /// Interpret [`pattern`](Self::pattern) as a Rust `regex` expression instead of literal text.
    pub is_regex: bool,
    /// Match case exactly. When `false`, matching is Unicode case-insensitive.
    pub case_sensitive: bool,
    /// Only accept matches whose word-character edges are not adjacent to other
    /// word characters (Unicode letters, digits, `_`).
    pub whole_word: bool,
    /// Gitignore-style globs; when non-empty only files matching at least one are searched.
    /// Each entry may itself be a comma-separated list (as typed in a GUI field).
    pub include_globs: Vec<String>,
    /// Gitignore-style globs of files and directories to skip.
    /// Each entry may itself be a comma-separated list.
    pub exclude_globs: Vec<String>,
    /// Maximum number of match ranges to report; `0` means unlimited.
    pub max_results: usize,
    /// Files larger than this many bytes are skipped.
    pub max_file_size: u64,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            is_regex: false,
            case_sensitive: false,
            whole_word: false,
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
            max_results: DEFAULT_MAX_RESULTS,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
        }
    }
}

impl SearchQuery {
    /// Creates a literal, case-insensitive query with default limits.
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            ..Self::default()
        }
    }

    /// Splits a comma-separated glob field (e.g. `"*.rs, src/**"`) into trimmed globs.
    pub fn parse_glob_list(field: &str) -> Vec<String> {
        field
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// One line containing at least one match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineMatch {
    /// Zero-based line number in the file.
    pub line: u32,
    /// Preview of the line: leading whitespace removed, trailing `\r` removed and, for
    /// very long lines, trimmed to [`MAX_PREVIEW_CHARS`] characters around the first match.
    pub line_text: String,
    /// Character column in the original line at which [`line_text`](Self::line_text) starts.
    /// Subtract it from a range to highlight inside the preview.
    pub preview_offset: u32,
    /// Match ranges as `(start, end)` character (Unicode scalar) columns in the
    /// original, untrimmed line; `end` is exclusive.
    pub ranges: Vec<(u32, u32)>,
}

/// All matches found in one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatches {
    /// Path relative to the search root.
    pub path: PathBuf,
    /// Matching lines in ascending line order.
    pub matches: Vec<LineMatch>,
}

impl FileMatches {
    /// Total number of match ranges in this file.
    pub fn match_count(&self) -> usize {
        self.matches.iter().map(|m| m.ranges.len()).sum()
    }
}

/// Result of a completed [`search_workspace`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOutcome {
    /// Files with matches, sorted by path.
    pub files: Vec<FileMatches>,
    /// `true` when [`SearchQuery::max_results`] was reached and the search stopped early.
    pub truncated: bool,
    /// Number of text files that were actually read and searched.
    pub files_scanned: usize,
    /// `true` when the search was stopped through the cancellation flag.
    pub cancelled: bool,
}

impl SearchOutcome {
    /// Total number of match ranges across all files.
    pub fn match_count(&self) -> usize {
        self.files.iter().map(FileMatches::match_count).sum()
    }
}
