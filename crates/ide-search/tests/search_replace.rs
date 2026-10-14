//! End-to-end tests for `ide-search` against real directory trees on disk.

use ide_search::{
    replace_in_files, search_workspace, spawn_search, SearchError, SearchEvent, SearchQuery,
    MAX_PREVIEW_CHARS,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tempfile::TempDir;

const MAIN_RS: &str = "fn main() {\n    let needle = 1;\n    println!(\"{}\", needle);\n}\n";
const LIB_RS: &str = "pub fn Needle() {}\n// needles everywhere\n";
const VI_MD: &str = "Xin chào thế giới\nTiếng Việt có dấu: chào bạn\n";
const CRLF_TXT: &str = "alpha=1\r\nbeta=2\r\nno match here\r\n";

fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".gitignore", "ignored.txt\nbuild/\n");
    write(root, "ignored.txt", "needle in an ignored file\n");
    write(root, "build/out.txt", "needle in an ignored dir\n");
    write(root, ".hidden/secret.txt", "needle in a hidden dir\n");
    write(root, "src/main.rs", MAIN_RS);
    write(root, "src/lib.rs", LIB_RS);
    write(root, "docs/vi.md", VI_MD);
    write(root, "crlf.txt", CRLF_TXT);
    let mut binary = b"needle\0\x01\x02".to_vec();
    binary.extend_from_slice(&[0xff; 64]);
    write(root, "image.bin", binary);
    dir
}

fn search(root: &Path, query: &SearchQuery) -> ide_search::SearchOutcome {
    search_workspace(root, query, &AtomicBool::new(false)).unwrap()
}

fn paths(outcome: &ide_search::SearchOutcome) -> Vec<String> {
    outcome
        .files
        .iter()
        .map(|f| f.path.to_string_lossy().replace('\\', "/"))
        .collect()
}

#[test]
fn literal_search_respects_gitignore_hidden_and_binary_files() {
    let dir = fixture();
    let outcome = search(dir.path(), &SearchQuery::new("needle"));
    assert_eq!(paths(&outcome), vec!["src/lib.rs", "src/main.rs"]);
    assert!(!outcome.truncated);
    assert!(!outcome.cancelled);
    // src/main.rs, src/lib.rs, docs/vi.md, crlf.txt: hidden (.gitignore, .hidden/),
    // ignored and binary files are never searched.
    assert_eq!(outcome.files_scanned, 4);

    let main = &outcome.files[1];
    assert_eq!(main.matches.len(), 2);
    assert_eq!(main.matches[0].line, 1);
    assert_eq!(main.matches[0].line_text, "let needle = 1;");
    assert_eq!(main.matches[0].preview_offset, 4);
    assert_eq!(main.matches[0].ranges, vec![(8, 14)]);
    assert_eq!(main.matches[1].line, 2);

    let lib = &outcome.files[0];
    assert_eq!(
        lib.matches.len(),
        2,
        "case-insensitive by default: Needle + needles"
    );
    assert_eq!(outcome.match_count(), 4);
}

#[test]
fn case_sensitive_and_whole_word() {
    let dir = fixture();
    let mut query = SearchQuery::new("needle");
    query.case_sensitive = true;
    let outcome = search(dir.path(), &query);
    assert_eq!(outcome.match_count(), 3, "Needle excluded");

    query.whole_word = true;
    let outcome = search(dir.path(), &query);
    assert_eq!(paths(&outcome), vec!["src/main.rs"], "needles excluded");
    assert_eq!(outcome.match_count(), 2);
}

#[test]
fn regex_search() {
    let dir = fixture();
    let mut query = SearchQuery::new(r"^(\w+)=(\d+)$");
    query.is_regex = true;
    let outcome = search(dir.path(), &query);
    assert_eq!(paths(&outcome), vec!["crlf.txt"]);
    let lines: Vec<_> = outcome.files[0]
        .matches
        .iter()
        .map(|m| (m.line, m.line_text.as_str(), m.ranges.clone()))
        .collect();
    assert_eq!(
        lines,
        vec![(0, "alpha=1", vec![(0, 7)]), (1, "beta=2", vec![(0, 6)])]
    );
}

#[test]
fn vietnamese_unicode_columns_and_case_folding() {
    let dir = fixture();
    let outcome = search(dir.path(), &SearchQuery::new("chào"));
    assert_eq!(paths(&outcome), vec!["docs/vi.md"]);
    let m = &outcome.files[0].matches;
    assert_eq!(m[0].ranges, vec![(4, 8)]);
    assert_eq!(m[1].ranges, vec![(19, 23)]);
    let chars: Vec<char> = m[1].line_text.chars().collect();
    assert_eq!(chars[19..23].iter().collect::<String>(), "chào");

    let outcome = search(dir.path(), &SearchQuery::new("VIỆT"));
    assert_eq!(outcome.files[0].matches[0].ranges, vec![(6, 10)]);

    let mut whole = SearchQuery::new("Việ");
    whole.whole_word = true;
    assert_eq!(search(dir.path(), &whole).match_count(), 0);
}

#[test]
fn include_and_exclude_globs() {
    let dir = fixture();
    let mut query = SearchQuery::new("n");
    query.include_globs = vec!["*.rs, *.md".into()];
    assert_eq!(
        paths(&search(dir.path(), &query)),
        vec!["docs/vi.md", "src/lib.rs", "src/main.rs"]
    );
    query.exclude_globs = vec!["src/lib.rs".into(), "docs".into()];
    assert_eq!(paths(&search(dir.path(), &query)), vec!["src/main.rs"]);

    // Include globs must not re-include gitignored or hidden files.
    let mut query = SearchQuery::new("needle");
    query.include_globs = vec!["*.txt".into()];
    assert!(search(dir.path(), &query).files.is_empty());
}

#[test]
fn max_results_truncates() {
    let dir = fixture();
    let mut query = SearchQuery::new("needle");
    query.max_results = 1;
    let outcome = search(dir.path(), &query);
    assert!(outcome.truncated);
    assert_eq!(outcome.match_count(), 1);
}

#[test]
fn oversized_files_are_skipped() {
    let dir = fixture();
    let mut query = SearchQuery::new("needle");
    query.max_file_size = 30;
    let outcome = search(dir.path(), &query);
    assert_eq!(paths(&outcome), Vec::<String>::new());
}

#[test]
fn long_lines_are_trimmed_but_columns_stay_exact() {
    let dir = tempfile::tempdir().unwrap();
    let line = format!("{}TARGET{}\n", "x".repeat(5000), "y".repeat(5000));
    write(dir.path(), "minified.js", &line);
    let outcome = search(dir.path(), &SearchQuery::new("target"));
    let m = &outcome.files[0].matches[0];
    assert_eq!(m.ranges, vec![(5000, 5006)]);
    assert_eq!(m.line_text.chars().count(), MAX_PREVIEW_CHARS);
    let rel = (m.ranges[0].0 - m.preview_offset) as usize;
    assert_eq!(&m.line_text[rel..rel + 6], "TARGET");
}

#[test]
fn errors_are_reported() {
    let dir = fixture();
    let mut query = SearchQuery::new("(unclosed");
    query.is_regex = true;
    assert!(matches!(
        search_workspace(dir.path(), &query, &AtomicBool::new(false)),
        Err(SearchError::InvalidPattern(_))
    ));
    let missing = dir.path().join("missing");
    assert_eq!(
        search_workspace(&missing, &SearchQuery::new("x"), &AtomicBool::new(false)),
        Err(SearchError::InvalidRoot(missing))
    );
    let mut query = SearchQuery::new("x");
    query.include_globs = vec!["a[".into()];
    assert!(matches!(
        search_workspace(dir.path(), &query, &AtomicBool::new(false)),
        Err(SearchError::InvalidGlob { .. })
    ));
    assert!(search(dir.path(), &SearchQuery::new("")).files.is_empty());
}

#[test]
fn pre_cancelled_search_reads_nothing() {
    let dir = fixture();
    let outcome = search_workspace(
        dir.path(),
        &SearchQuery::new("needle"),
        &AtomicBool::new(true),
    )
    .unwrap();
    assert!(outcome.cancelled);
    assert_eq!(outcome.files_scanned, 0);
    assert!(outcome.files.is_empty());
}

fn big_tree(files: usize) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..files {
        write(
            dir.path(),
            &format!("pkg{}/file{i}.txt", i % 20),
            format!("line one\nneedle {i}\nline three\n"),
        );
    }
    dir
}

fn drain(
    handle: &ide_search::SearchHandle,
) -> (Vec<ide_search::FileMatches>, ide_search::SearchSummary) {
    let mut files = Vec::new();
    loop {
        match handle
            .events()
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
        {
            SearchEvent::File(f) => files.push(f),
            SearchEvent::Finished(result) => return (files, result.unwrap()),
        }
    }
}

#[test]
fn streaming_search_matches_batch_search() {
    let dir = big_tree(500);
    let handle = spawn_search(dir.path(), SearchQuery::new("needle"));
    let (mut streamed, summary) = drain(&handle);
    streamed.sort_by(|a, b| a.path.cmp(&b.path));
    let batch = search(dir.path(), &SearchQuery::new("needle"));
    assert_eq!(streamed, batch.files);
    assert_eq!(streamed.len(), 500);
    assert_eq!(summary.files_scanned, 500);
    assert!(!summary.cancelled && !summary.truncated);
}

#[test]
fn streaming_search_can_be_cancelled() {
    let dir = big_tree(3000);
    let handle = spawn_search(dir.path(), SearchQuery::new("needle"));
    handle.cancel();
    let (files, summary) = drain(&handle);
    assert!(summary.cancelled);
    assert!(files.len() <= 3000);
}

#[test]
fn replace_literal_whole_word_only_touches_matching_files() {
    let dir = fixture();
    let root = dir.path();
    let vi_mtime = fs::metadata(root.join("docs/vi.md"))
        .unwrap()
        .modified()
        .unwrap();
    let mut query = SearchQuery::new("needle");
    query.case_sensitive = true;
    query.whole_word = true;
    let outcome = replace_in_files(root, &query, "pin$1", None).unwrap();
    assert_eq!(outcome.files_changed, 1);
    assert_eq!(outcome.replacements, 2);
    assert_eq!(outcome.changed_paths, vec![PathBuf::from("src/main.rs")]);
    assert!(outcome.failures.is_empty());
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        MAIN_RS.replace("needle", "pin$1")
    );
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), LIB_RS);
    assert_eq!(
        fs::read_to_string(root.join("ignored.txt")).unwrap(),
        "needle in an ignored file\n"
    );
    assert!(fs::read(root.join("image.bin"))
        .unwrap()
        .starts_with(b"needle\0"));
    assert_eq!(
        fs::metadata(root.join("docs/vi.md"))
            .unwrap()
            .modified()
            .unwrap(),
        vi_mtime
    );
    assert!(search(root, &query).files.is_empty());
}

#[test]
fn replace_regex_with_captures_preserves_crlf() {
    let dir = fixture();
    let mut query = SearchQuery::new(r"(\w+)=(?P<num>\d+)");
    query.is_regex = true;
    let outcome = replace_in_files(dir.path(), &query, "${num}:$1$$", None).unwrap();
    assert_eq!(outcome.replacements, 2);
    assert_eq!(
        fs::read_to_string(dir.path().join("crlf.txt")).unwrap(),
        "1:alpha$\r\n2:beta$\r\nno match here\r\n"
    );
}

#[test]
fn replace_regex_newline_escape_uses_file_line_ending() {
    let dir = fixture();
    let mut query = SearchQuery::new(r"(\w+)=(\d+)");
    query.is_regex = true;
    replace_in_files(dir.path(), &query, r"$1\n$2", None).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("crlf.txt")).unwrap(),
        "alpha\r\n1\r\nbeta\r\n2\r\nno match here\r\n"
    );
}

#[test]
fn replace_vietnamese_in_explicit_file_list() {
    let dir = fixture();
    let root = dir.path();
    write(root, "docs/other.md", "chào\n");
    let outcome = replace_in_files(
        root,
        &SearchQuery::new("CHÀO"),
        "xin chào",
        Some(&[PathBuf::from("docs/vi.md"), root.join("docs/vi.md")]),
    )
    .unwrap();
    assert_eq!(outcome.files_changed, 1);
    assert_eq!(outcome.replacements, 2);
    assert_eq!(
        fs::read_to_string(root.join("docs/vi.md")).unwrap(),
        "Xin xin chào thế giới\nTiếng Việt có dấu: xin chào bạn\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("docs/other.md")).unwrap(),
        "chào\n"
    );
}

#[cfg(unix)]
#[test]
fn replace_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let script = dir.path().join("run.sh");
    fs::write(&script, "echo needle\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    replace_in_files(dir.path(), &SearchQuery::new("needle"), "pin", None).unwrap();
    assert_eq!(fs::read_to_string(&script).unwrap(), "echo pin\n");
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}
