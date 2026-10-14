//! Project-wide replace.

use crate::matcher::{read_text_file, split_lines_with_eol, FileText, Matcher, Replacement};
use crate::search::{build_include, build_walker};
use crate::{SearchError, SearchQuery};
use ignore::WalkState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A file that matched but could not be rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceFailure {
    /// Path relative to the root (or as given, for paths outside the root).
    pub path: PathBuf,
    /// Human readable error.
    pub message: String,
}

/// Result of [`replace_in_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceOutcome {
    /// Number of files that were rewritten.
    pub files_changed: usize,
    /// Total number of replaced matches.
    pub replacements: usize,
    /// Rewritten files relative to the root, sorted. Reload these in open editors.
    pub changed_paths: Vec<PathBuf>,
    /// Files that matched but failed to be written.
    pub failures: Vec<ReplaceFailure>,
}

/// Replaces every match of `query` with `replacement` and writes the files back.
///
/// * `files == None`: every file a [`search_workspace`](crate::search_workspace) with the
///   same query would visit (ignore rules and include/exclude globs apply).
/// * `files == Some(list)`: only the listed files (relative to `root` or absolute); ignore
///   rules and globs are not applied to an explicit list.
///
/// In regex mode `replacement` supports capture groups (`$1`, `$0`, `$&`, `${name}`),
/// `$$` for a literal dollar and the escapes `\n`, `\t`, `\\`; in literal mode it is
/// inserted verbatim. Line endings (`\n` / `\r\n`, even mixed) are preserved, files
/// without matches are never touched, binary / non-UTF-8 / oversized files are skipped,
/// and each changed file is replaced atomically (temporary file + rename) keeping its
/// permissions. [`SearchQuery::max_results`] does not limit replacements.
pub fn replace_in_files(
    root: &Path,
    query: &SearchQuery,
    replacement: &str,
    files: Option<&[PathBuf]>,
) -> Result<ReplaceOutcome, SearchError> {
    let walker = build_walker(root, query)?;
    let include = build_include(root, query)?;
    let Some(matcher) = Matcher::new(query)? else {
        return Ok(ReplaceOutcome::default());
    };
    let results: Mutex<Vec<(PathBuf, Result<usize, String>)>> = Mutex::new(Vec::new());
    let record = |rel: PathBuf, result: Result<usize, String>| {
        results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((rel, result));
    };

    match files {
        Some(list) => {
            let unique: BTreeSet<PathBuf> = list.iter().map(|p| root.join(p)).collect();
            for abs in unique {
                if let Some(result) = replace_file(&abs, &matcher, replacement, query) {
                    record(relative(root, &abs), result);
                }
            }
        }
        None => {
            walker.build_parallel().run(|| {
                let matcher = &matcher;
                let include = &include;
                let record = &record;
                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file())
                        || include.matched(entry.path(), false).is_ignore()
                    {
                        return WalkState::Continue;
                    }
                    if let Some(result) = replace_file(entry.path(), matcher, replacement, query) {
                        record(relative(root, entry.path()), result);
                    }
                    WalkState::Continue
                })
            });
        }
    }

    let mut results = results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    results.sort_by(|a, b| a.0.cmp(&b.0));
    let mut outcome = ReplaceOutcome::default();
    for (path, result) in results {
        match result {
            Ok(count) => {
                outcome.files_changed += 1;
                outcome.replacements += count;
                outcome.changed_paths.push(path);
            }
            Err(message) => outcome.failures.push(ReplaceFailure { path, message }),
        }
    }
    Ok(outcome)
}

fn relative(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Rewrites one file. Returns `None` when the file is skipped or has no match.
fn replace_file(
    path: &Path,
    matcher: &Matcher,
    replacement: &str,
    query: &SearchQuery,
) -> Option<Result<usize, String>> {
    let FileText::Utf8(text) = read_text_file(path, query.max_file_size) else {
        return None;
    };
    if !matcher.may_match(&text) {
        return None;
    }
    let file_eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let replacement = Replacement::new(replacement, query.is_regex, file_eol);
    let mut output = String::with_capacity(text.len() + text.len() / 8);
    let mut count = 0;
    for (body, eol) in split_lines_with_eol(&text) {
        count += matcher.replace_in_line(body, &replacement, &mut output);
        output.push_str(eol);
    }
    if count == 0 {
        return None;
    }
    Some(
        write_atomically(path, output.as_bytes())
            .map(|()| count)
            .map_err(|e| e.to_string()),
    )
}

/// Writes `data` to a temporary sibling file and renames it over `path`, preserving
/// the original permissions. Symlinks are resolved so the link target is updated.
fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    let target = if fs::symlink_metadata(path)?.file_type().is_symlink() {
        fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let permissions = fs::metadata(&target)?.permissions();
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.ide-search-{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::set_permissions(&tmp, permissions)?;
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
