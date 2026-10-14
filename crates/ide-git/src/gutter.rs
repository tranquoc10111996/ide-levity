//! Per-line gutter markers computed with a histogram diff (`imara-diff`, via `gix`).

use crate::{DiffMarkerKind, LineDiffStatus};
use gix::diff::blob::{diff, intern::InternedInput, Algorithm};
use std::collections::BTreeMap;
use std::ops::Range;

/// Splits text into lines without terminators (`\n` or `\r\n`); a trailing newline
/// does not produce an extra empty line.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for line in &mut lines {
        if let Some(stripped) = line.strip_suffix('\r') {
            *line = stripped;
        }
    }
    lines
}

/// Computes per-line gutter diff markers between `old_text` and `new_text`.
///
/// Uses the histogram diff algorithm (O((n + m) log n) in practice, suitable for
/// running on every keystroke on files with tens of thousands of lines). Line endings
/// are normalized, so `\r\n` vs `\n` and a missing final newline do not produce markers.
///
/// Returns only changed lines of `new_text`, sorted by `line_number`:
/// * `Added`: lines inserted without removing anything at that position.
/// * `Modified`: lines that replaced one or more old lines.
/// * `Deleted`: old lines were removed just before this line (or at the last line
///   when the removal happened at the end of the file; line `0` for an empty file).
pub fn compute_line_diff(old_text: &str, new_text: &str) -> Vec<LineDiffStatus> {
    let old_lines = split_lines(old_text);
    let new_lines = split_lines(new_text);
    let new_len = new_lines.len() as u32;

    let mut input = InternedInput::default();
    input.update_before(old_lines.into_iter());
    input.update_after(new_lines.into_iter());

    let mut markers: BTreeMap<u32, DiffMarkerKind> = BTreeMap::new();
    diff(
        Algorithm::Histogram,
        &input,
        |before: Range<u32>, after: Range<u32>| {
            let removed = before.end > before.start;
            let added = after.end > after.start;
            if added {
                let kind = if removed {
                    DiffMarkerKind::Modified
                } else {
                    DiffMarkerKind::Added
                };
                for line in after {
                    markers.insert(line, kind);
                }
            } else if removed {
                let target = if new_len == 0 {
                    0
                } else {
                    after.start.min(new_len - 1)
                };
                markers.entry(target).or_insert(DiffMarkerKind::Deleted);
            }
        },
    );

    markers
        .into_iter()
        .map(|(line_number, status)| LineDiffStatus {
            line_number,
            status,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn marker(markers: &[LineDiffStatus], line: u32) -> Option<DiffMarkerKind> {
        markers
            .iter()
            .find(|m| m.line_number == line)
            .map(|m| m.status)
    }

    #[test]
    fn modified_line() {
        let old = "line 0\nline 1\nline 2\nline 3\n";
        let new = "line 0\nline 1 modified\nline 2\nadded line A\nadded line B\n";
        let markers = compute_line_diff(old, new);
        assert_eq!(marker(&markers, 1), Some(DiffMarkerKind::Modified));
        assert_eq!(marker(&markers, 0), None);
        assert_eq!(marker(&markers, 2), None);
        assert_eq!(marker(&markers, 3), Some(DiffMarkerKind::Modified));
        assert_eq!(marker(&markers, 4), Some(DiffMarkerKind::Modified));
    }

    #[test]
    fn pure_add_and_delete() {
        let markers = compute_line_diff("a\nb\nc\nd\n", "a\nc\nd\ne\n");
        assert_eq!(
            markers,
            vec![
                LineDiffStatus {
                    line_number: 1,
                    status: DiffMarkerKind::Deleted
                },
                LineDiffStatus {
                    line_number: 3,
                    status: DiffMarkerKind::Added
                },
            ]
        );
    }

    #[test]
    fn deletion_at_end_and_empty_results() {
        let markers = compute_line_diff("a\nb\nc\n", "a\n");
        assert_eq!(marker(&markers, 0), Some(DiffMarkerKind::Deleted));
        assert_eq!(markers.len(), 1);
        let markers = compute_line_diff("a\nb\n", "");
        assert_eq!(marker(&markers, 0), Some(DiffMarkerKind::Deleted));
        assert!(compute_line_diff("", "").is_empty());
        assert_eq!(compute_line_diff("", "x\ny").len(), 2);
    }

    #[test]
    fn line_endings_are_normalized() {
        assert!(compute_line_diff("a\r\nb\r\n", "a\nb").is_empty());
    }

    #[test]
    fn vietnamese_and_emoji_lines() {
        let markers = compute_line_diff("Xin chào\n🦀 Rust\n", "Xin chào thế giới\n🦀 Rust\n");
        assert_eq!(
            markers,
            vec![LineDiffStatus {
                line_number: 0,
                status: DiffMarkerKind::Modified
            }]
        );
    }

    #[test]
    fn large_file_diff_is_fast() {
        let old: String = (0..20_000)
            .map(|i| format!("    let value_{i} = compute({i}) + {};\n", i % 7))
            .collect();
        let mut lines: Vec<String> = old.lines().map(str::to_owned).collect();
        lines[100] = "    // edited".into();
        lines.insert(5_000, "    inserted();".into());
        lines.remove(15_000);
        lines[19_000].push_str(" // tail edit");
        let new = lines.join("\n") + "\n";

        let start = Instant::now();
        let markers = compute_line_diff(&old, &new);
        let elapsed = start.elapsed();

        assert_eq!(marker(&markers, 100), Some(DiffMarkerKind::Modified));
        assert_eq!(marker(&markers, 5_000), Some(DiffMarkerKind::Added));
        assert_eq!(marker(&markers, 15_000), Some(DiffMarkerKind::Deleted));
        assert_eq!(marker(&markers, 19_000), Some(DiffMarkerKind::Modified));
        assert_eq!(markers.len(), 4);
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(50)
        };
        assert!(elapsed < budget, "diff of 20k lines took {elapsed:?}");
    }
}
