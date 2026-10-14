//! Aider-style SEARCH/REPLACE block parsing and tolerant application.
//!
//! Format produced by the model:
//!
//! ```text
//! src/lib.rs
//! <<<<<<< SEARCH
//! old lines
//! =======
//! new lines
//! >>>>>>> REPLACE
//! ```

use crate::error::EditError;

/// One parsed SEARCH/REPLACE block. Texts use `\n` line endings and carry
/// no trailing newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditBlock {
    /// File path written by the model before the block, if any.
    pub path: Option<String>,
    /// Lines to find (empty means "insert into empty file / append").
    pub search: String,
    /// Replacement lines.
    pub replace: String,
}

fn marker(line: &str, ch: char, word: &str) -> bool {
    let t = line.trim();
    let count = t.chars().take_while(|&c| c == ch).count();
    if !(5..=9).contains(&count) {
        return false;
    }
    t[count..].trim() == word
}

fn is_search(line: &str) -> bool {
    marker(line, '<', "SEARCH")
}

fn is_divider(line: &str) -> bool {
    let t = line.trim();
    (5..=9).contains(&t.len()) && t.chars().all(|c| c == '=')
}

fn is_replace(line: &str) -> bool {
    marker(line, '>', "REPLACE")
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

fn clean_path(line: &str) -> Option<String> {
    let mut t = line.trim();
    for prefix in ["File:", "file:", "Path:", "path:", "Filename:", "filename:"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            t = rest.trim();
        }
    }
    let t = t
        .trim_start_matches(['#', '*', '`', '>', '-', ' '])
        .trim_end_matches(['*', '`', ':', ' ']);
    let looks_like_path = !t.is_empty()
        && t.len() < 260
        && !t.chars().any(char::is_whitespace)
        && t.chars().any(|c| c.is_alphanumeric());
    looks_like_path.then(|| t.to_string())
}

/// Parses every SEARCH/REPLACE block in a model response.
///
/// Returns an empty list when the response has no blocks, and
/// [`EditError::MalformedBlock`] when a block is opened but not closed.
pub fn parse_edit_blocks(response: &str) -> Result<Vec<EditBlock>, EditError> {
    let lines: Vec<&str> = response
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut blocks: Vec<EditBlock> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_search(lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let path = find_path(&lines, start, blocks.last());
        let mut search = Vec::new();
        i += 1;
        loop {
            let Some(line) = lines.get(i) else {
                return Err(EditError::MalformedBlock {
                    line: start + 1,
                    reason: "missing ======= divider".into(),
                });
            };
            if is_divider(line) {
                break;
            }
            if is_search(line) || is_replace(line) {
                return Err(EditError::MalformedBlock {
                    line: start + 1,
                    reason: format!("unexpected marker at line {} before =======", i + 1),
                });
            }
            search.push(*line);
            i += 1;
        }
        i += 1;
        let mut replace = Vec::new();
        loop {
            let Some(line) = lines.get(i) else {
                return Err(EditError::MalformedBlock {
                    line: start + 1,
                    reason: "missing >>>>>>> REPLACE marker (response cut off?)".into(),
                });
            };
            if is_replace(line) {
                break;
            }
            if is_search(line) {
                return Err(EditError::MalformedBlock {
                    line: start + 1,
                    reason: format!("new SEARCH marker at line {} before >>>>>>> REPLACE", i + 1),
                });
            }
            replace.push(*line);
            i += 1;
        }
        i += 1;
        blocks.push(EditBlock {
            path,
            search: search.join("\n"),
            replace: replace.join("\n"),
        });
    }
    Ok(blocks)
}

fn find_path(lines: &[&str], search_line: usize, previous: Option<&EditBlock>) -> Option<String> {
    let mut checked = 0;
    for line in lines[..search_line].iter().rev() {
        if line.trim().is_empty() || is_fence(line) {
            continue;
        }
        if is_replace(line) {
            return previous.and_then(|b| b.path.clone());
        }
        checked += 1;
        if let Some(path) = clean_path(line) {
            return Some(path);
        }
        if checked >= 2 {
            break;
        }
    }
    None
}

/// Normalizes a path for comparison (`\` to `/`, leading `./` removed).
fn norm_path(p: &str) -> String {
    let p = p.trim().replace('\\', "/");
    p.trim_start_matches("./").to_string()
}

/// Whether a path written by the model refers to `actual`: equal, a suffix
/// at a path boundary, or the same file name.
pub fn path_matches(written: &str, actual: &str) -> bool {
    let (w, a) = (norm_path(written), norm_path(actual));
    if w.is_empty() || a.is_empty() {
        return false;
    }
    if w == a || a.ends_with(&format!("/{w}")) || w.ends_with(&format!("/{a}")) {
        return true;
    }
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    base(&w) == base(&a)
}

struct Lines {
    lines: Vec<String>,
    trailing_newline: bool,
}

impl Lines {
    fn split(text: &str) -> Lines {
        if text.is_empty() {
            return Lines {
                lines: Vec::new(),
                trailing_newline: false,
            };
        }
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        let trailing_newline = lines.last().is_some_and(String::is_empty);
        if trailing_newline {
            lines.pop();
        }
        Lines {
            lines,
            trailing_newline,
        }
    }

    fn join(&self) -> String {
        let mut out = self.lines.join("\n");
        if self.trailing_newline && !self.lines.is_empty() {
            out.push('\n');
        }
        out
    }
}

fn block_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
            .collect()
    }
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

#[derive(Clone, Copy, PartialEq)]
enum Strategy {
    Exact,
    TrimEnd,
    Trim,
    IgnoreBlankLines,
}

/// Finds candidate `(start, end_exclusive)` spans of `search` in `file`.
fn find_spans(file: &[String], search: &[String], strategy: Strategy) -> Vec<(usize, usize)> {
    let eq: fn(&str, &str) -> bool = match strategy {
        Strategy::Exact => |a, b| a == b,
        Strategy::TrimEnd => |a, b| a.trim_end() == b.trim_end(),
        Strategy::Trim | Strategy::IgnoreBlankLines => |a, b| a.trim() == b.trim(),
    };
    if strategy == Strategy::IgnoreBlankLines {
        let file_idx: Vec<usize> = (0..file.len())
            .filter(|&i| !file[i].trim().is_empty())
            .collect();
        let needle: Vec<&String> = search.iter().filter(|l| !l.trim().is_empty()).collect();
        if needle.is_empty() || needle.len() > file_idx.len() {
            return Vec::new();
        }
        return (0..=file_idx.len() - needle.len())
            .filter(|&s| {
                needle
                    .iter()
                    .enumerate()
                    .all(|(k, n)| eq(&file[file_idx[s + k]], n))
            })
            .map(|s| (file_idx[s], file_idx[s + needle.len() - 1] + 1))
            .collect();
    }
    if search.is_empty() || search.len() > file.len() {
        return Vec::new();
    }
    (0..=file.len() - search.len())
        .filter(|&s| search.iter().enumerate().all(|(k, n)| eq(&file[s + k], n)))
        .map(|s| (s, s + search.len()))
        .collect()
}

/// Re-indents replacement lines when the match ignored indentation.
fn reindent(replace: &[String], search: &[String], matched_first: &str) -> Vec<String> {
    let Some(search_first) = search.iter().find(|l| !l.trim().is_empty()) else {
        return replace.to_vec();
    };
    let s_ind = indent_of(search_first);
    let f_ind = indent_of(matched_first);
    if s_ind == f_ind {
        return replace.to_vec();
    }
    replace
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else if let Some(rest) = line.strip_prefix(s_ind) {
                format!("{f_ind}{rest}")
            } else {
                line.clone()
            }
        })
        .collect()
}

fn closest_line(file: &[String], search: &[String]) -> Option<usize> {
    let needle: Vec<&str> = search
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if needle.is_empty() || file.is_empty() {
        return None;
    }
    let mut best = (0usize, 0usize);
    for start in 0..file.len() {
        let first_trimmed = file[start].trim();
        if !needle.contains(&first_trimmed) {
            continue;
        }
        let score = file[start..]
            .iter()
            .take(needle.len())
            .filter(|f| needle.contains(&f.trim()))
            .count();
        if score > best.0 {
            best = (score, start);
        }
    }
    (best.0 > 0).then_some(best.1 + 1)
}

fn preview(search: &[String]) -> String {
    let mut out: Vec<String> = search.iter().take(5).cloned().collect();
    if search.len() > 5 {
        out.push(format!("... ({} more lines)", search.len() - 5));
    }
    out.join("\n")
}

/// Applies `blocks` in order to `original`.
///
/// Matching tries, in order: exact lines, trailing-whitespace-insensitive,
/// indentation-insensitive (replacement is re-indented), and finally
/// ignoring blank lines. When several places match, the one closest to
/// `hint_line` (zero-based, e.g. the selection start) wins, otherwise the
/// first. An empty SEARCH writes into an empty file or appends to the end.
/// CRLF files keep CRLF line endings.
pub fn apply_edit_blocks(
    file_path: &str,
    original: &str,
    blocks: &[EditBlock],
    hint_line: Option<usize>,
) -> Result<String, EditError> {
    let crlf = original.contains("\r\n");
    let mut doc = Lines::split(&original.replace("\r\n", "\n"));
    for (index, block) in blocks.iter().enumerate() {
        let search = block_lines(&block.search);
        let replace = block_lines(&block.replace);
        if search.iter().all(|l| l.trim().is_empty()) {
            doc.lines.extend(replace);
            doc.trailing_newline = true;
            continue;
        }
        let mut applied = false;
        for strategy in [
            Strategy::Exact,
            Strategy::TrimEnd,
            Strategy::Trim,
            Strategy::IgnoreBlankLines,
        ] {
            let spans = find_spans(&doc.lines, &search, strategy);
            let chosen = match hint_line {
                Some(hint) => spans.iter().min_by_key(|(s, e)| {
                    if hint < *s {
                        s - hint
                    } else if hint >= *e {
                        hint - e + 1
                    } else {
                        0
                    }
                }),
                None => spans.first(),
            };
            if let Some(&(start, end)) = chosen {
                let new_lines = match strategy {
                    Strategy::Exact | Strategy::TrimEnd => replace.clone(),
                    Strategy::Trim | Strategy::IgnoreBlankLines => {
                        reindent(&replace, &search, &doc.lines[start])
                    }
                };
                doc.lines.splice(start..end, new_lines);
                applied = true;
                break;
            }
        }
        if !applied {
            return Err(EditError::SearchNotFound {
                file_path: file_path.to_string(),
                block_index: index,
                total: blocks.len(),
                search_preview: preview(&search),
                closest_line: closest_line(&doc.lines, &search),
            });
        }
    }
    let text = doc.join();
    Ok(if crlf {
        text.replace('\n', "\r\n")
    } else {
        text
    })
}

/// A fenced code block found in a response.
struct Fenced {
    info: String,
    body: String,
    lines: usize,
}

fn fenced_blocks(response: &str) -> Vec<Fenced> {
    let mut out = Vec::new();
    let mut current: Option<(String, String, Vec<String>)> = None;
    for raw in response.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim_start();
        match &mut current {
            None => {
                let ticks = trimmed.chars().take_while(|&c| c == '`' || c == '~').count();
                if ticks >= 3 {
                    let fence = trimmed[..ticks].to_string();
                    let info = trimmed[ticks..].trim().to_string();
                    current = Some((fence, info, Vec::new()));
                }
            }
            Some((fence, info, body)) => {
                if trimmed.trim_end() == fence.as_str() {
                    out.push(Fenced {
                        info: std::mem::take(info),
                        lines: body.len(),
                        body: body.join("\n"),
                    });
                    current = None;
                } else {
                    body.push(line.to_string());
                }
            }
        }
    }
    out
}

const LAZY_MARKERS: [&str; 7] = [
    "existing code",
    "rest of the file",
    "rest of the code",
    "remains unchanged",
    "unchanged code",
    "rest of file",
    "... other",
];

/// Fallback when the model returned the whole file in a fenced block:
/// picks the largest fenced block and rejects obvious partial snippets.
pub fn extract_full_file(response: &str, original: &str) -> Result<String, EditError> {
    let block = fenced_blocks(response)
        .into_iter()
        .filter(|b| !b.body.contains("<<<<<<<"))
        .max_by_key(|b| b.lines)
        .ok_or(EditError::NoEditFound)?;
    let _ = block.info;
    let original_lf = original.replace("\r\n", "\n");
    let original_lines: Vec<&str> = original_lf.lines().collect();
    for line in block.body.lines() {
        let lower = line.trim().to_ascii_lowercase();
        let is_ellipsis = matches!(lower.as_str(), "..." | "// ..." | "# ..." | "/* ... */");
        let lazy = is_ellipsis || LAZY_MARKERS.iter().any(|m| lower.contains(m));
        if lazy && !original_lines.iter().any(|o| o.trim() == line.trim()) {
            return Err(EditError::PartialFile(format!(
                "placeholder line \"{}\"",
                line.trim()
            )));
        }
    }
    let original_count = original_lines.iter().filter(|l| !l.trim().is_empty()).count();
    let new_count = block.body.lines().filter(|l| !l.trim().is_empty()).count();
    if original_count >= 30 && new_count * 10 < original_count * 4 {
        return Err(EditError::PartialFile(format!(
            "{new_count} non-blank lines returned for a {original_count}-line file"
        )));
    }
    let mut text = block.body;
    if original_lf.ends_with('\n') || original_lf.is_empty() {
        text.push('\n');
    }
    Ok(if original.contains("\r\n") {
        text.replace('\n', "\r\n")
    } else {
        text
    })
}

/// The prose part of a response: everything except SEARCH/REPLACE blocks
/// (with their path lines) and fenced code blocks.
pub fn explanation_text(response: &str) -> String {
    let lines: Vec<&str> = response
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut keep = vec![true; lines.len()];
    let mut i = 0;
    let mut in_fence = false;
    let mut in_block = false;
    while i < lines.len() {
        let line = lines[i];
        if is_search(line) {
            in_block = true;
            // Drop the path line (and fence opener) right before the block.
            let mut j = i;
            let mut dropped = 0;
            while j > 0 && dropped < 2 {
                j -= 1;
                if lines[j].trim().is_empty() {
                    continue;
                }
                if is_fence(lines[j]) || clean_path(lines[j]).is_some() {
                    keep[j] = false;
                    dropped += 1;
                } else {
                    break;
                }
            }
        }
        if in_block || in_fence || is_fence(line) {
            keep[i] = false;
        }
        if is_replace(line) {
            in_block = false;
        } else if is_fence(line) && !in_block {
            in_fence = !in_fence;
        }
        i += 1;
    }
    let kept: Vec<&str> = lines
        .iter()
        .zip(keep)
        .filter_map(|(l, k)| k.then_some(*l))
        .collect();
    let mut out = String::new();
    let mut blank_run = 0;
    for line in kept {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(search: &str, replace: &str) -> EditBlock {
        EditBlock {
            path: None,
            search: search.into(),
            replace: replace.into(),
        }
    }

    #[test]
    fn parses_multiple_blocks_with_paths_and_fences() {
        let response = "I renamed the function.\n\nsrc/lib.rs\n```rust\n<<<<<<< SEARCH\nfn a() {}\n=======\nfn b() {}\n>>>>>>> REPLACE\n```\n\n<<<<<<< SEARCH\nlet x = 1;\n=======\n>>>>>>> REPLACE\n\n**`src/other.rs`**\n```\n<<<<<<< SEARCH\n=======\nnew\n>>>>>>> REPLACE\n```\n";
        let blocks = parse_edit_blocks(response).unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(blocks[0].search, "fn a() {}");
        assert_eq!(blocks[0].replace, "fn b() {}");
        assert_eq!(blocks[1].path.as_deref(), Some("src/lib.rs"), "inherits previous path");
        assert_eq!(blocks[1].replace, "");
        assert_eq!(blocks[2].path.as_deref(), Some("src/other.rs"));
        assert_eq!(blocks[2].search, "");
        assert_eq!(explanation_text(response), "I renamed the function.");
    }

    #[test]
    fn crlf_response_and_marker_tolerance() {
        let response = "a.py\r\n<<<<<<<< SEARCH\r\nx = 1\r\n========\r\nx = 2\r\n>>>>>>>> REPLACE\r\n";
        let blocks = parse_edit_blocks(response).unwrap();
        assert_eq!(blocks, vec![EditBlock {
            path: Some("a.py".into()),
            search: "x = 1".into(),
            replace: "x = 2".into()
        }]);
    }

    #[test]
    fn unterminated_block_is_malformed() {
        let err = parse_edit_blocks("<<<<<<< SEARCH\nx\n=======\ny\n").unwrap_err();
        assert!(matches!(err, EditError::MalformedBlock { line: 1, .. }));
        let err = parse_edit_blocks("<<<<<<< SEARCH\nx\n>>>>>>> REPLACE\n").unwrap_err();
        assert!(matches!(err, EditError::MalformedBlock { .. }));
        assert!(parse_edit_blocks("no blocks here").unwrap().is_empty());
    }

    #[test]
    fn exact_application_with_multiple_blocks() {
        let original = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
        let out = apply_edit_blocks(
            "main.rs",
            original,
            &[
                block("    let a = 1;", "    let a = 10;"),
                block("    let b = 2;", "    let b = 20;\n    let c = a + b;"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(out, "fn main() {\n    let a = 10;\n    let b = 20;\n    let c = a + b;\n}\n");
    }

    #[test]
    fn whitespace_insensitive_match_reindents_replacement() {
        let original = "class A:\n    def f(self):\n        return 1\n";
        let out = apply_edit_blocks(
            "a.py",
            original,
            &[block("def f(self):\n    return 1", "def f(self):\n    return 2")],
            None,
        )
        .unwrap();
        assert_eq!(out, "class A:\n    def f(self):\n        return 2\n");
    }

    #[test]
    fn trailing_whitespace_and_blank_line_differences_are_tolerated() {
        let original = "a  \n\nb\nc\n";
        let out = apply_edit_blocks("f", original, &[block("a\nb", "A\nB")], None).unwrap();
        assert_eq!(out, "A\nB\nc\n");
        let out = apply_edit_blocks("f", "x = 1   \ny\n", &[block("x = 1", "x = 2")], None).unwrap();
        assert_eq!(out, "x = 2\ny\n");
    }

    #[test]
    fn crlf_file_keeps_crlf_and_unicode_survives() {
        let original = "// Xin chào thế giới\r\nlet s = \"😀\";\r\n";
        let out = apply_edit_blocks(
            "a.js",
            original,
            &[block("let s = \"😀\";", "let s = \"🚀 Việt Nam\";")],
            None,
        )
        .unwrap();
        assert_eq!(out, "// Xin chào thế giới\r\nlet s = \"🚀 Việt Nam\";\r\n");
    }

    #[test]
    fn empty_search_creates_or_appends() {
        assert_eq!(
            apply_edit_blocks("new.txt", "", &[block("", "hello\nworld")], None).unwrap(),
            "hello\nworld\n"
        );
        assert_eq!(
            apply_edit_blocks("f", "a", &[block("", "b")], None).unwrap(),
            "a\nb\n"
        );
    }

    #[test]
    fn missing_search_is_a_descriptive_error() {
        let original = "fn one() {}\nfn two() {}\n";
        let err = apply_edit_blocks(
            "lib.rs",
            original,
            &[block("fn one() {}", "fn uno() {}"), block("fn three() {}\nfn two() {}", "x")],
            None,
        )
        .unwrap_err();
        match &err {
            EditError::SearchNotFound {
                block_index,
                total,
                closest_line,
                ..
            } => {
                assert_eq!((*block_index, *total), (1, 2));
                assert_eq!(*closest_line, Some(2));
            }
            other => panic!("{other:?}"),
        }
        assert!(err.to_string().contains("SEARCH block 2 of 2"));
    }

    #[test]
    fn duplicate_matches_prefer_the_hint() {
        let original = "x = 0\nfoo()\nx = 0\nbar()\n";
        let out = apply_edit_blocks("f", original, &[block("x = 0", "x = 1")], Some(3)).unwrap();
        assert_eq!(out, "x = 0\nfoo()\nx = 1\nbar()\n");
        let out = apply_edit_blocks("f", original, &[block("x = 0", "x = 1")], None).unwrap();
        assert_eq!(out, "x = 1\nfoo()\nx = 0\nbar()\n");
    }

    #[test]
    fn file_without_trailing_newline_is_preserved() {
        let out = apply_edit_blocks("f", "a\nb", &[block("b", "c")], None).unwrap();
        assert_eq!(out, "a\nc");
    }

    #[test]
    fn full_file_fallback_and_partial_detection() {
        let original = "a\nb\n";
        let resp = "Here is the file:\n```python\na\nB\n```\n";
        assert_eq!(extract_full_file(resp, original).unwrap(), "a\nB\n");
        let lazy = "```python\na\n# ... existing code ...\n```";
        assert!(matches!(
            extract_full_file(lazy, original),
            Err(EditError::PartialFile(_))
        ));
        let big: String = (0..40).map(|i| format!("line {i}\n")).collect();
        assert!(matches!(
            extract_full_file("```\nline 1\n```", &big),
            Err(EditError::PartialFile(_))
        ));
        assert_eq!(extract_full_file("no code", original), Err(EditError::NoEditFound));
    }

    #[test]
    fn path_matching_rules() {
        assert!(path_matches("src/lib.rs", "/home/u/proj/src/lib.rs"));
        assert!(path_matches("./src\\lib.rs", "src/lib.rs"));
        assert!(path_matches("lib.rs", "crates/x/src/lib.rs"));
        assert!(!path_matches("main.rs", "src/lib.rs"));
    }
}
