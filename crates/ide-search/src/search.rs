//! Parallel, gitignore-aware workspace search.

use crate::matcher::{read_text_file, split_lines_with_eol, FileText, Matcher};
use crate::{
    FileMatches, LineMatch, SearchError, SearchOutcome, SearchQuery, MAX_PREVIEW_CHARS,
    PREVIEW_CONTEXT_CHARS,
};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::{WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

/// Summary of a finished streaming search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSummary {
    /// `true` when [`SearchQuery::max_results`] was reached and the search stopped early.
    pub truncated: bool,
    /// Number of text files that were actually read and searched.
    pub files_scanned: usize,
    /// `true` when the search was stopped through the cancellation flag.
    pub cancelled: bool,
}

/// Event delivered by a search started with [`spawn_search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEvent {
    /// Matches for one file. Files arrive in completion order, not sorted.
    File(FileMatches),
    /// The search ended (always the last event).
    Finished(Result<SearchSummary, SearchError>),
}

/// Handle to a background search started with [`spawn_search`].
///
/// Dropping the handle (and therefore the receiver) also stops the search: the worker
/// notices the closed channel on the next file and quits.
pub struct SearchHandle {
    cancel: Arc<AtomicBool>,
    events: Receiver<SearchEvent>,
}

impl SearchHandle {
    /// Requests cancellation. A [`SearchEvent::Finished`] event with
    /// `cancelled: true` follows shortly.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Shared cancellation flag, e.g. to cancel from another thread.
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Receiver of search events. Use `try_recv`/`try_iter` from a GUI thread.
    pub fn events(&self) -> &Receiver<SearchEvent> {
        &self.events
    }

    /// Splits the handle into its cancellation flag and event receiver, e.g. to move
    /// the receiver into an async stream adapter.
    pub fn into_parts(self) -> (Arc<AtomicBool>, Receiver<SearchEvent>) {
        (self.cancel, self.events)
    }
}

/// Starts a streaming search of `root` on a dedicated background thread.
///
/// Results are delivered as [`SearchEvent::File`] events as soon as each file has been
/// searched, followed by exactly one [`SearchEvent::Finished`].
pub fn spawn_search(root: impl Into<PathBuf>, query: SearchQuery) -> SearchHandle {
    let root = root.into();
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, events) = mpsc::channel();
    let worker_cancel = Arc::clone(&cancel);
    std::thread::Builder::new()
        .name("ide-search".into())
        .spawn(move || {
            let result = search_workspace_with(&root, &query, &worker_cancel, |file| {
                if tx.send(SearchEvent::File(file)).is_err() {
                    worker_cancel.store(true, Ordering::Relaxed);
                }
            });
            let _ = tx.send(SearchEvent::Finished(result));
        })
        .expect("failed to spawn the ide-search worker thread");
    SearchHandle { cancel, events }
}

/// Searches every non-ignored text file under `root` and returns matches sorted by path.
///
/// Respects `.gitignore` (also outside git repositories), `.ignore`, `.git/info/exclude`
/// and the global git excludes file; skips hidden files, binary files and files larger
/// than [`SearchQuery::max_file_size`]. Set `cancel` from another thread to stop early.
pub fn search_workspace(
    root: &Path,
    query: &SearchQuery,
    cancel: &AtomicBool,
) -> Result<SearchOutcome, SearchError> {
    let collected = Mutex::new(Vec::new());
    let summary = search_workspace_with(root, query, cancel, |file| {
        collected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(file);
    })?;
    let mut files = collected
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(SearchOutcome {
        files,
        truncated: summary.truncated,
        files_scanned: summary.files_scanned,
        cancelled: summary.cancelled,
    })
}

/// Streaming search core: calls `on_file` from worker threads for every file with at
/// least one match, as soon as that file has been searched.
pub fn search_workspace_with<F>(
    root: &Path,
    query: &SearchQuery,
    cancel: &AtomicBool,
    on_file: F,
) -> Result<SearchSummary, SearchError>
where
    F: Fn(FileMatches) + Sync,
{
    let walker = build_walker(root, query)?;
    let include = build_include(root, query)?;
    let Some(matcher) = Matcher::new(query)? else {
        return Ok(SearchSummary::default());
    };
    let budget = Budget::new(query.max_results);
    let files_scanned = AtomicUsize::new(0);

    walker.build_parallel().run(|| {
        let matcher = &matcher;
        let budget = &budget;
        let include = &include;
        let files_scanned = &files_scanned;
        let on_file = &on_file;
        let mut ranges = Vec::new();
        Box::new(move |entry| {
            if cancel.load(Ordering::Relaxed) || budget.exhausted() {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            if include.matched(entry.path(), false).is_ignore() {
                return WalkState::Continue;
            }
            let text = match read_text_file(entry.path(), query.max_file_size) {
                FileText::Utf8(text) | FileText::Lossy(text) => text,
                FileText::Skipped => return WalkState::Continue,
            };
            files_scanned.fetch_add(1, Ordering::Relaxed);
            let matches = search_text(&text, matcher, budget, &mut ranges);
            if !matches.is_empty() {
                let path = entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_path_buf();
                on_file(FileMatches { path, matches });
            }
            if budget.exhausted() {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    Ok(SearchSummary {
        truncated: budget.exhausted(),
        files_scanned: files_scanned.load(Ordering::Relaxed),
        cancelled: cancel.load(Ordering::Relaxed),
    })
}

/// Builds the gitignore-aware walker with the query's exclude globs applied.
pub(crate) fn build_walker(root: &Path, query: &SearchQuery) -> Result<WalkBuilder, SearchError> {
    if !root.is_dir() {
        return Err(SearchError::InvalidRoot(root.to_path_buf()));
    }
    let mut excludes = OverrideBuilder::new(root);
    for glob in expand_globs(&query.exclude_globs) {
        excludes
            .add(&format!("!{glob}"))
            .map_err(|e| invalid_glob(&glob, e))?;
    }
    let excludes = excludes.build().map_err(|e| invalid_glob("", e))?;
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .follow_links(false)
        .overrides(excludes);
    Ok(walker)
}

/// Builds the include filter. It is applied separately from the walker so that
/// include globs narrow the search without re-including gitignored or hidden files
/// (which is what `ignore` overrides would do).
pub(crate) fn build_include(root: &Path, query: &SearchQuery) -> Result<Override, SearchError> {
    let mut includes = OverrideBuilder::new(root);
    for glob in expand_globs(&query.include_globs) {
        includes.add(&glob).map_err(|e| invalid_glob(&glob, e))?;
    }
    includes.build().map_err(|e| invalid_glob("", e))
}

fn expand_globs(globs: &[String]) -> Vec<String> {
    globs
        .iter()
        .flat_map(|g| SearchQuery::parse_glob_list(g))
        .collect()
}

fn invalid_glob(glob: &str, err: ignore::Error) -> SearchError {
    SearchError::InvalidGlob {
        glob: glob.to_owned(),
        message: err.to_string(),
    }
}

/// Searches `text` line by line, consuming result budget.
fn search_text(
    text: &str,
    matcher: &Matcher,
    budget: &Budget,
    ranges: &mut Vec<Range<usize>>,
) -> Vec<LineMatch> {
    let mut out = Vec::new();
    if !matcher.may_match(text) {
        return out;
    }
    for (index, (body, _eol)) in split_lines_with_eol(text).enumerate() {
        ranges.clear();
        matcher.find_in_line(body, ranges);
        if ranges.is_empty() {
            continue;
        }
        let allowed = budget.take(ranges.len());
        ranges.truncate(allowed);
        if ranges.is_empty() {
            break;
        }
        let char_ranges = to_char_ranges(body, ranges);
        let (line_text, preview_offset) = make_preview(body, char_ranges[0].0 as usize);
        out.push(LineMatch {
            line: u32::try_from(index).unwrap_or(u32::MAX),
            line_text,
            preview_offset,
            ranges: char_ranges,
        });
        if budget.exhausted() {
            break;
        }
    }
    out
}

/// Converts sorted, non-overlapping byte ranges into character column ranges.
fn to_char_ranges(line: &str, ranges: &[Range<usize>]) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(ranges.len());
    let mut byte_pos = 0;
    let mut char_pos = 0usize;
    for range in ranges {
        char_pos += line[byte_pos..range.start].chars().count();
        let start = char_pos;
        char_pos += line[range.start..range.end].chars().count();
        byte_pos = range.end;
        out.push((to_u32(start), to_u32(char_pos)));
    }
    out
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Builds the preview text for a line and returns it with its starting char column.
fn make_preview(line: &str, first_match_col: usize) -> (String, u32) {
    let total = line.chars().count();
    let leading_ws = line.chars().take_while(|c| c.is_whitespace()).count();
    let mut start = leading_ws.min(first_match_col);
    if total - start > MAX_PREVIEW_CHARS && first_match_col - start > PREVIEW_CONTEXT_CHARS {
        start = first_match_col - PREVIEW_CONTEXT_CHARS;
    }
    let text = line.chars().skip(start).take(MAX_PREVIEW_CHARS).collect();
    (text, to_u32(start))
}

/// Shared, thread-safe result budget.
struct Budget {
    limit: Option<usize>,
    used: AtomicUsize,
    exhausted: AtomicBool,
}

impl Budget {
    fn new(max_results: usize) -> Self {
        Self {
            limit: (max_results > 0).then_some(max_results),
            used: AtomicUsize::new(0),
            exhausted: AtomicBool::new(false),
        }
    }

    /// Reserves up to `wanted` results and returns how many were granted.
    fn take(&self, wanted: usize) -> usize {
        let Some(limit) = self.limit else {
            return wanted;
        };
        let before = self.used.fetch_add(wanted, Ordering::Relaxed);
        let granted = limit.saturating_sub(before).min(wanted);
        if before + wanted >= limit {
            self.exhausted.store(true, Ordering::Relaxed);
        }
        granted
    }

    fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keeps_short_lines_without_leading_whitespace() {
        let (text, offset) = make_preview("    let x = 1;", 8);
        assert_eq!(text, "let x = 1;");
        assert_eq!(offset, 4);
    }

    #[test]
    fn preview_trims_long_lines_around_first_match() {
        let line = format!("{}needle{}", "a".repeat(1000), "b".repeat(1000));
        let (text, offset) = make_preview(&line, 1000);
        assert_eq!(offset as usize, 1000 - PREVIEW_CONTEXT_CHARS);
        assert_eq!(text.chars().count(), MAX_PREVIEW_CHARS);
        let rel = 1000 - offset as usize;
        assert_eq!(&text[rel..rel + 6], "needle");
    }

    #[test]
    fn char_ranges_count_unicode_scalars() {
        let line = "Xin chào thế giới, chào!";
        let ranges: Vec<_> = line
            .match_indices("chào")
            .map(|(i, s)| i..i + s.len())
            .collect();
        assert_eq!(ranges, vec![4..9, 24..29]);
        assert_eq!(to_char_ranges(line, &ranges), vec![(4, 8), (19, 23)]);
    }

    #[test]
    fn budget_grants_partial_and_marks_exhausted() {
        let b = Budget::new(5);
        assert_eq!(b.take(3), 3);
        assert!(!b.exhausted());
        assert_eq!(b.take(3), 2);
        assert!(b.exhausted());
        assert_eq!(b.take(1), 0);
        let unlimited = Budget::new(0);
        assert_eq!(unlimited.take(1_000_000), 1_000_000);
        assert!(!unlimited.exhausted());
    }
}
