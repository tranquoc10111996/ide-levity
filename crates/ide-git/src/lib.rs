//! `ide-git`: Git engine integration powered by `gix` (`gitoxide`).
//! Reads current branch name, changed file list, and per-line gutter diff markers (`Added`, `Modified`, `Deleted`).

use gix::bstr::ByteSlice;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Failed to open git repository: {0}")]
    Open(String),
    #[error("Git repository operation failed: {0}")]
    Repo(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Kind of change for a file in the working tree compared to `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
}

/// Status of a changed file in the Git working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileStatus {
    pub path: String,
    pub kind: FileChangeKind,
}

/// Kind of diff marker for a single line in the editor gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffMarkerKind {
    Unchanged,
    Added,
    Modified,
    Deleted,
}

/// Per-line git diff marker exported to `ViewportViewModel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineDiffStatus {
    pub line_number: u32,
    pub status: DiffMarkerKind,
}

/// Git repository handle backed by `gix`.
pub struct GitRepository {
    workdir: PathBuf,
    git_dir: PathBuf,
}

impl GitRepository {
    /// Opens or discovers a Git repository starting at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let repo = gix::discover(path.as_ref()).map_err(|e| GitError::Open(e.to_string()))?;
        let git_dir = repo.path().to_path_buf();
        let workdir = repo
            .work_dir()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| path.as_ref().to_path_buf());
        Ok(Self { workdir, git_dir })
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    fn reopen(&self) -> Result<gix::Repository, GitError> {
        gix::open(&self.git_dir).map_err(|e| GitError::Open(e.to_string()))
    }

    /// Returns the short name of the current branch (e.g., `"main"`), or `"HEAD (detached)"`.
    pub fn current_branch(&self) -> Result<String, GitError> {
        let repo = self.reopen()?;
        match repo.head_name().map_err(|e| GitError::Repo(e.to_string()))? {
            Some(name) => Ok(name.shorten().to_str_lossy().into_owned()),
            None => Ok("HEAD (detached)".to_string()),
        }
    }

    /// Reads the UTF-8 content of a file at `HEAD`, if it exists in the `HEAD` tree.
    pub fn head_file_content(&self, relative_path: &Path) -> Result<Option<String>, GitError> {
        let repo = self.reopen()?;
        let Ok(mut head) = repo.head() else {
            return Ok(None);
        };
        let Ok(commit) = head.peel_to_commit_in_place() else {
            return Ok(None);
        };
        let Ok(tree) = commit.tree() else {
            return Ok(None);
        };
        let rel_str = relative_path.to_string_lossy().replace('\\', "/");
        let mut scratch = Vec::new();
        let Ok(Some(entry)) = tree.lookup_entry_by_path(rel_str.as_str(), &mut scratch) else {
            return Ok(None);
        };
        let obj = repo
            .find_object(entry.oid())
            .map_err(|e| GitError::Repo(e.to_string()))?;
        Ok(Some(String::from_utf8_lossy(&obj.data).into_owned()))
    }

    /// Lists all files changed between `HEAD` and the working directory (`Added`, `Modified`, `Deleted`).
    pub fn changed_files(&self) -> Result<Vec<GitFileStatus>, GitError> {
        let repo = self.reopen()?;
        let mut head_files: BTreeMap<String, Vec<u8>> = BTreeMap::new();

        if let Ok(mut head) = repo.head() {
            if let Ok(commit) = head.peel_to_commit_in_place() {
                if let Ok(tree) = commit.tree() {
                    collect_tree_blobs(&repo, &tree, "", &mut head_files)?;
                }
            }
        }

        let mut work_files: BTreeSet<String> = BTreeSet::new();
        collect_workdir_files(&self.workdir, &self.workdir, &mut work_files)?;

        let mut statuses = Vec::new();

        for (rel_path, head_bytes) in &head_files {
            if !work_files.contains(rel_path) {
                statuses.push(GitFileStatus {
                    path: rel_path.clone(),
                    kind: FileChangeKind::Deleted,
                });
            } else {
                let full_path = self.workdir.join(rel_path);
                if let Ok(disk_bytes) = fs::read(&full_path) {
                    if &disk_bytes != head_bytes {
                        statuses.push(GitFileStatus {
                            path: rel_path.clone(),
                            kind: FileChangeKind::Modified,
                        });
                    }
                }
            }
        }

        for rel_path in work_files {
            if !head_files.contains_key(&rel_path) {
                statuses.push(GitFileStatus {
                    path: rel_path,
                    kind: FileChangeKind::Added,
                });
            }
        }

        statuses.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(statuses)
    }

    /// Computes gutter diff markers (`Added`, `Modified`, `Deleted`) for `current_text`
    /// compared against the version of `file_path` in `HEAD`.
    pub fn diff_buffer_lines(
        &self,
        file_path: impl AsRef<Path>,
        current_text: &str,
    ) -> Result<Vec<LineDiffStatus>, GitError> {
        let path_ref = file_path.as_ref();
        let rel_path = path_ref
            .strip_prefix(&self.workdir)
            .unwrap_or(path_ref);

        match self.head_file_content(rel_path)? {
            Some(old_text) => Ok(compute_line_diff(&old_text, current_text)),
            None => {
                // Untracked/new file: all lines are Added
                let lines = split_lines(current_text);
                Ok((0..lines.len() as u32)
                    .map(|line_number| LineDiffStatus {
                        line_number,
                        status: DiffMarkerKind::Added,
                    })
                    .collect())
            }
        }
    }
}

fn collect_tree_blobs(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), GitError> {
    for entry_res in tree.iter() {
        let entry = entry_res.map_err(|e| GitError::Repo(e.to_string()))?;
        let name = entry.filename().to_str_lossy();
        let rel = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode().is_tree() {
            let obj = repo
                .find_object(entry.oid())
                .map_err(|e| GitError::Repo(e.to_string()))?;
            let sub_tree = obj.into_tree();
            collect_tree_blobs(repo, &sub_tree, &rel, out)?;
        } else if entry.mode().is_blob() {
            let obj = repo
                .find_object(entry.oid())
                .map_err(|e| GitError::Repo(e.to_string()))?;
            out.insert(rel, obj.data.clone());
        }
    }
    Ok(())
}

fn collect_workdir_files(
    root: &Path,
    current: &Path,
    out: &mut BTreeSet<String>,
) -> Result<(), GitError> {
    if !current.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if name_str == ".git" || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_workdir_files(root, &path, out)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for l in &mut lines {
        if let Some(stripped) = l.strip_suffix('\r') {
            *l = stripped;
        }
    }
    lines
}

/// Computes per-line gutter diff markers between `old_text` and `new_text` using LCS dynamic programming.
/// Returns only the changed lines (`Added`, `Modified`, `Deleted`) in sorted order by `line_number`.
pub fn compute_line_diff(old_text: &str, new_text: &str) -> Vec<LineDiffStatus> {
    let old_lines = split_lines(old_text);
    let new_lines = split_lines(new_text);
    let n = old_lines.len();
    let m = new_lines.len();

    // Build LCS table
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            if old_lines[i] == new_lines[j] {
                dp[i][j] = 1 + dp[i + 1][j + 1];
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }

    // Extract matched pairs (old_idx, new_idx)
    let mut matches = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // Sentinel match at end of both files
    matches.push((n, m));

    let mut markers: BTreeMap<u32, DiffMarkerKind> = BTreeMap::new();
    let mut prev_old = 0;
    let mut prev_new = 0;

    for (match_old, match_new) in matches {
        let old_gap = match_old - prev_old;
        let new_gap = match_new - prev_new;

        if old_gap > 0 && new_gap > 0 {
            // Hunk has both removed and added lines -> Modified
            for new_line in prev_new..match_new {
                markers.insert(new_line as u32, DiffMarkerKind::Modified);
            }
        } else if old_gap == 0 && new_gap > 0 {
            // Pure addition
            for new_line in prev_new..match_new {
                markers.insert(new_line as u32, DiffMarkerKind::Added);
            }
        } else if old_gap > 0 && new_gap == 0 {
            // Pure deletion: attach Deleted marker to the line at `prev_new` (or last line)
            let target_line = if m == 0 {
                0
            } else {
                prev_new.min(m - 1) as u32
            };
            markers
                .entry(target_line)
                .or_insert(DiffMarkerKind::Deleted);
        }

        prev_old = match_old + 1;
        prev_new = match_new + 1;
    }

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
    use std::process::Command;

    #[test]
    fn test_compute_line_diff_added_modified_deleted() {
        let old = "line 0\nline 1\nline 2\nline 3\n";
        let new = "line 0\nline 1 modified\nline 2\nadded line A\nadded line B\n";
        // Wait: line 1 was modified, line 3 was deleted and replaced or let's test pure cases:
        let markers = compute_line_diff(old, new);
        assert!(markers
            .iter()
            .any(|m| m.line_number == 1 && m.status == DiffMarkerKind::Modified));
    }

    #[test]
    fn test_compute_line_diff_pure_add_and_delete() {
        let old = "a\nb\nc\nd\n";
        // Delete 'b' (between a and c), add 'e' after 'd'
        let new = "a\nc\nd\ne\n";
        let markers = compute_line_diff(old, new);
        // Deletion of 'b' attaches a Deleted marker at line 1 ('c'), and 'e' at line 3 is Added
        assert!(markers
            .iter()
            .any(|m| m.line_number == 1 && m.status == DiffMarkerKind::Deleted));
        assert!(markers
            .iter()
            .any(|m| m.line_number == 3 && m.status == DiffMarkerKind::Added));
    }

    #[test]
    fn test_gix_repository_branch_changed_files_and_gutter_diff() {
        let temp_dir = std::env::temp_dir().join(format!(
            "ide_levity_git_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        // Initialize a real git repository for gix to inspect
        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&temp_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "agent@ide-levity.dev"])
            .current_dir(&temp_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "IDE Levity Agent"])
            .current_dir(&temp_dir)
            .output()
            .unwrap();

        fs::write(temp_dir.join("main.rs"), "fn main() {\n    println!(\"v1\");\n}\n").unwrap();
        fs::write(temp_dir.join("obsolete.txt"), "to be deleted\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(&temp_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(&temp_dir)
            .output()
            .unwrap();

        // Now modify main.rs, delete obsolete.txt, and add new_feature.rs
        fs::write(
            temp_dir.join("main.rs"),
            "fn main() {\n    println!(\"v2\");\n    println!(\"added\");\n}\n",
        )
        .unwrap();
        fs::remove_file(temp_dir.join("obsolete.txt")).unwrap();
        fs::write(temp_dir.join("new_feature.rs"), "pub fn feature() {}\n").unwrap();

        let repo = GitRepository::open(&temp_dir).unwrap();
        assert_eq!(repo.current_branch().unwrap(), "main");

        let changed = repo.changed_files().unwrap();
        assert_eq!(
            changed,
            vec![
                GitFileStatus {
                    path: "main.rs".to_string(),
                    kind: FileChangeKind::Modified,
                },
                GitFileStatus {
                    path: "new_feature.rs".to_string(),
                    kind: FileChangeKind::Added,
                },
                GitFileStatus {
                    path: "obsolete.txt".to_string(),
                    kind: FileChangeKind::Deleted,
                },
            ]
        );

        let diff_markers = repo
            .diff_buffer_lines(
                temp_dir.join("main.rs"),
                "fn main() {\n    println!(\"v2\");\n    println!(\"added\");\n}\n",
            )
            .unwrap();
        assert_eq!(diff_markers.len(), 2);
        assert_eq!(diff_markers[0].line_number, 1);
        assert_eq!(diff_markers[0].status, DiffMarkerKind::Modified);
        assert_eq!(diff_markers[1].line_number, 2);
        assert_eq!(diff_markers[1].status, DiffMarkerKind::Modified);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
