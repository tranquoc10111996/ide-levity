//! Line-oriented matcher shared by search and replace, plus file reading helpers.

use crate::{SearchError, SearchQuery};
use regex::{Regex, RegexBuilder};
use std::ops::Range;
use std::path::Path;

/// Number of leading bytes inspected for NUL bytes to classify a file as binary
/// (the same heuristic `git` uses).
const BINARY_SNIFF_LEN: usize = 8 * 1024;

/// Compiled form of a [`SearchQuery`] pattern.
pub(crate) struct Matcher {
    regex: Regex,
    whole_word: bool,
}

impl Matcher {
    /// Compiles `query`. Returns `Ok(None)` for an empty pattern, which matches nothing.
    pub(crate) fn new(query: &SearchQuery) -> Result<Option<Self>, SearchError> {
        if query.pattern.is_empty() {
            return Ok(None);
        }
        let source = if query.is_regex {
            query.pattern.clone()
        } else {
            regex::escape(&query.pattern)
        };
        let regex = RegexBuilder::new(&source)
            .case_insensitive(!query.case_sensitive)
            .multi_line(true)
            .crlf(true)
            .size_limit(64 * 1024 * 1024)
            .build()
            .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;
        Ok(Some(Self {
            regex,
            whole_word: query.whole_word,
        }))
    }

    /// Fast whole-buffer check used to skip files without any candidate match.
    /// It may return `true` for files that end up having no line match, never the reverse.
    pub(crate) fn may_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }

    /// Appends the byte ranges of all accepted matches in `line` (which must not
    /// contain line terminators) to `out`.
    pub(crate) fn find_in_line(&self, line: &str, out: &mut Vec<Range<usize>>) {
        let mut pos = 0;
        while pos <= line.len() {
            let Some(m) = self.regex.find_at(line, pos) else {
                break;
            };
            if m.is_empty() {
                pos = next_char_boundary(line, m.end());
            } else if self.accepts(line, m.start(), m.end()) {
                out.push(m.range());
                pos = m.end();
            } else {
                pos = next_char_boundary(line, m.start());
            }
        }
    }

    /// Replaces all accepted matches in `line`, appending the result to `out`.
    /// Returns the number of replacements performed.
    pub(crate) fn replace_in_line(
        &self,
        line: &str,
        replacement: &Replacement,
        out: &mut String,
    ) -> usize {
        let mut pos = 0;
        let mut copied_until = 0;
        let mut count = 0;
        while pos <= line.len() {
            let Some(caps) = self.regex.captures_at(line, pos) else {
                break;
            };
            let m = caps.get(0).expect("group 0 always participates");
            if m.is_empty() {
                pos = next_char_boundary(line, m.end());
            } else if self.accepts(line, m.start(), m.end()) {
                out.push_str(&line[copied_until..m.start()]);
                match replacement {
                    Replacement::Literal(text) => out.push_str(text),
                    Replacement::Template(template) => caps.expand(template, out),
                }
                copied_until = m.end();
                pos = m.end();
                count += 1;
            } else {
                pos = next_char_boundary(line, m.start());
            }
        }
        out.push_str(&line[copied_until..]);
        count
    }

    fn accepts(&self, line: &str, start: usize, end: usize) -> bool {
        if !self.whole_word {
            return true;
        }
        let matched = &line[start..end];
        let starts_with_word = matched.chars().next().is_some_and(is_word_char);
        let ends_with_word = matched.chars().next_back().is_some_and(is_word_char);
        if starts_with_word && line[..start].chars().next_back().is_some_and(is_word_char) {
            return false;
        }
        if ends_with_word && line[end..].chars().next().is_some_and(is_word_char) {
            return false;
        }
        true
    }
}

/// Replacement text prepared for a given query mode.
pub(crate) enum Replacement {
    /// Inserted verbatim (literal search mode).
    Literal(String),
    /// A `regex` expansion template (regex search mode).
    Template(String),
}

impl Replacement {
    /// Prepares `replacement` for the given mode and line ending.
    ///
    /// In regex mode the VS Code conventions are translated to `regex` templates:
    /// `$1`..`$99` and `$0` refer to numbered groups (so `$1a` means group 1 followed by
    /// `a`), `$&` is the whole match, `${name}` is a named group, `$$` is a literal `$`,
    /// and any other `$` is literal. The escapes `\n`, `\t` and `\\` insert a line break
    /// (using `eol`), a tab and a backslash.
    pub(crate) fn new(replacement: &str, is_regex: bool, eol: &str) -> Self {
        if !is_regex {
            return Self::Literal(replacement.to_owned());
        }
        let mut template = String::with_capacity(replacement.len() + 8);
        let mut chars = replacement.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '$' => match chars.peek().copied() {
                    Some('$') => {
                        chars.next();
                        template.push_str("$$");
                    }
                    Some('&') => {
                        chars.next();
                        template.push_str("${0}");
                    }
                    Some('{') => template.push('$'),
                    Some(d) if d.is_ascii_digit() => {
                        let mut digits = String::new();
                        while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                            if digits.len() == 2 {
                                break;
                            }
                            digits.push(d);
                            chars.next();
                        }
                        template.push_str("${");
                        template.push_str(&digits);
                        template.push('}');
                    }
                    _ => template.push_str("$$"),
                },
                '\\' => match chars.peek().copied() {
                    Some('n') => {
                        chars.next();
                        template.push_str(eol);
                    }
                    Some('t') => {
                        chars.next();
                        template.push('\t');
                    }
                    Some('\\') => {
                        chars.next();
                        template.push('\\');
                    }
                    _ => template.push('\\'),
                },
                other => template.push(other),
            }
        }
        Self::Template(template)
    }
}

/// Returns `true` for characters considered part of a word for whole-word matching.
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Index just past the character starting at byte `i`, or `line.len() + 1` at the end
/// so that scanning loops terminate.
fn next_char_boundary(line: &str, i: usize) -> usize {
    match line[i..].chars().next() {
        Some(c) => i + c.len_utf8(),
        None => line.len() + 1,
    }
}

/// Classification of a file read from disk.
pub(crate) enum FileText {
    /// Valid UTF-8 text.
    Utf8(String),
    /// Text that is not valid UTF-8, decoded lossily (search only, never rewritten).
    Lossy(String),
    /// Binary, unreadable or too large: skipped.
    Skipped,
}

/// Reads `path` if it is a text file no larger than `max_size` bytes.
pub(crate) fn read_text_file(path: &Path, max_size: u64) -> FileText {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= max_size => {}
        _ => return FileText::Skipped,
    }
    let Ok(bytes) = std::fs::read(path) else {
        return FileText::Skipped;
    };
    if bytes.len() as u64 > max_size || is_binary(&bytes) {
        return FileText::Skipped;
    }
    match String::from_utf8(bytes) {
        Ok(text) => FileText::Utf8(text),
        Err(err) => FileText::Lossy(String::from_utf8_lossy(err.as_bytes()).into_owned()),
    }
}

/// Binary heuristic: a NUL byte within the first [`BINARY_SNIFF_LEN`] bytes.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0)
}

/// Splits `text` into `(body, terminator)` pairs where the terminator is `"\r\n"`,
/// `"\n"` or `""` (last line without a newline).
pub(crate) fn split_lines_with_eol(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.split_inclusive('\n').map(|piece| {
        if let Some(body) = piece.strip_suffix("\r\n") {
            (body, "\r\n")
        } else if let Some(body) = piece.strip_suffix('\n') {
            (body, "\n")
        } else {
            (piece, "")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(pattern: &str, f: impl FnOnce(&mut SearchQuery)) -> Matcher {
        let mut q = SearchQuery::new(pattern);
        f(&mut q);
        Matcher::new(&q).unwrap().unwrap()
    }

    fn find(m: &Matcher, line: &str) -> Vec<String> {
        let mut ranges = Vec::new();
        m.find_in_line(line, &mut ranges);
        ranges.into_iter().map(|r| line[r].to_owned()).collect()
    }

    #[test]
    fn literal_case_insensitive_by_default() {
        let m = matcher("Foo", |_| {});
        assert_eq!(find(&m, "foo FOO fOo"), vec!["foo", "FOO", "fOo"]);
    }

    #[test]
    fn literal_escapes_regex_metacharacters() {
        let m = matcher("a.b(", |_| {});
        assert_eq!(find(&m, "a.b( axb("), vec!["a.b("]);
    }

    #[test]
    fn whole_word_respects_unicode_letters() {
        let m = matcher("việt", |q| q.whole_word = true);
        assert_eq!(
            find(&m, "việt việtnam tiếngviệt (việt)"),
            vec!["việt", "việt"]
        );
    }

    #[test]
    fn whole_word_retries_after_rejected_match() {
        let m = matcher("aa", |q| q.whole_word = true);
        assert_eq!(find(&m, "aaa aa"), vec!["aa"]);
    }

    #[test]
    fn whole_word_ignores_non_word_edges() {
        let m = matcher("(x)", |q| q.whole_word = true);
        assert_eq!(find(&m, "f(x)y"), vec!["(x)"]);
    }

    #[test]
    fn zero_length_matches_are_skipped() {
        let m = matcher("a*", |q| q.is_regex = true);
        assert_eq!(find(&m, "baab"), vec!["aa"]);
    }

    #[test]
    fn empty_pattern_compiles_to_none() {
        assert!(Matcher::new(&SearchQuery::new("")).unwrap().is_none());
    }

    #[test]
    fn invalid_regex_is_reported() {
        let mut q = SearchQuery::new("(unclosed");
        q.is_regex = true;
        assert!(matches!(
            Matcher::new(&q),
            Err(SearchError::InvalidPattern(_))
        ));
    }

    #[test]
    fn replacement_template_translation() {
        let Replacement::Template(t) =
            Replacement::new("$1a-$&-${name}-$$-$x-\\n\\t\\\\", true, "\r\n")
        else {
            panic!("expected template");
        };
        assert_eq!(t, "${1}a-${0}-${name}-$$-$$x-\r\n\t\\");
    }

    #[test]
    fn replace_in_line_with_captures() {
        let m = matcher(r"(\w+)=(\d+)", |q| q.is_regex = true);
        let mut out = String::new();
        let n = m.replace_in_line(
            "a=1, b=22",
            &Replacement::new("$2:$1", true, "\n"),
            &mut out,
        );
        assert_eq!(n, 2);
        assert_eq!(out, "1:a, 22:b");
    }

    #[test]
    fn split_lines_preserves_terminators() {
        let parts: Vec<_> = split_lines_with_eol("a\r\nb\nc").collect();
        assert_eq!(parts, vec![("a", "\r\n"), ("b", "\n"), ("c", "")]);
    }

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary("tiếng việt".as_bytes()));
    }
}
