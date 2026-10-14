//! `ide-git`: Git engine for IDE Levity, powered by `gix` (gitoxide).
//!
//! Read side:
//! * current branch, HEAD file contents and per-line gutter diff markers
//!   ([`GitRepository::diff_buffer_lines`], cached per HEAD commit),
//! * detailed status distinguishing staged and unstaged changes
//!   ([`GitRepository::status_detailed`]),
//! * unified diffs of single files ([`GitRepository::file_diff`]),
//! * HEAD commit summary and local branches.
//!
//! Write side (Source Control panel):
//! * [`GitRepository::stage`], [`GitRepository::stage_all`], [`GitRepository::unstage`],
//! * [`GitRepository::discard_worktree_changes`], [`GitRepository::delete_untracked`],
//! * [`GitRepository::commit`], [`GitRepository::create_branch`],
//!   [`GitRepository::checkout_branch`].
//!
//! Everything is implemented with `gix` 0.66 directly (index mutation, blob/tree/commit
//! writing, reference edits); the `git` executable is not required.
//!
//! # Threading
//!
//! [`GitRepository`] is `Clone + Send + Sync` and cheap to clone. All methods are
//! synchronous and touch the filesystem, so a GUI should call them from a background
//! thread (or `tokio::task::spawn_blocking`). Write operations take the index lock
//! (`.git/index.lock`) and fail fast if another process holds it.

mod branch;
mod commit;
mod diff;
mod gutter;
mod index_ops;
mod status;
mod util;

pub use branch::BranchInfo;
pub use commit::CommitSummary;
pub use gutter::compute_line_diff;
pub use status::{ChangeKind, StatusEntry};

use gix::bstr::ByteSlice;
use gix::ObjectId;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Errors returned by [`GitRepository`] operations.
#[derive(Debug, Error)]
pub enum GitError {
    /// The repository could not be discovered or opened.
    #[error("Failed to open git repository: {0}")]
    Open(String),
    /// A lower level `gix` operation failed.
    #[error("Git repository operation failed: {0}")]
    Repo(String),
    /// Filesystem error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The operation needs a worktree but the repository is bare.
    #[error("the repository has no worktree (bare repository)")]
    Bare,
    /// A path argument points outside of the worktree.
    #[error("path is outside of the repository worktree: {0}")]
    PathOutsideWorktree(String),
    /// No author identity was given and none is configured.
    #[error("no git identity configured: set user.name and user.email with `git config` or pass an author")]
    MissingIdentity,
    /// The index matches `HEAD`, so a commit would be empty.
    #[error("nothing to commit: no staged changes")]
    NothingToCommit,
    /// The commit message is empty after whitespace cleanup.
    #[error("aborting commit due to empty commit message")]
    EmptyMessage,
    /// The index contains unresolved merge conflicts.
    #[error("unmerged paths must be resolved first: {}", .0.join(", "))]
    Unmerged(Vec<String>),
    /// The named local branch does not exist.
    #[error("branch not found: {0}")]
    BranchNotFound(String),
    /// The branch name is not a valid git reference name.
    #[error("invalid branch name `{0}`: {1}")]
    InvalidBranchName(String, String),
    /// A branch with this name already exists.
    #[error("a branch named `{0}` already exists")]
    BranchExists(String),
    /// `HEAD` has no commit yet (unborn branch).
    #[error("HEAD does not point to a commit yet")]
    UnbornHead,
    /// A checkout would overwrite local modifications or untracked files.
    #[error("your local changes to the following files would be overwritten: {}", .0.join(", "))]
    WouldOverwrite(Vec<String>),
    /// [`GitRepository::delete_untracked`] was asked to delete a path that is not untracked.
    #[error("not an untracked path: {0}")]
    NotUntracked(String),
}

pub(crate) fn repo_err(err: impl std::fmt::Display) -> GitError {
    GitError::Repo(err.to_string())
}

/// Kind of change for a file in the working tree compared to `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    /// Not in `HEAD` but present in the worktree (staged or untracked).
    Added,
    /// Present in both with different content or mode.
    Modified,
    /// In `HEAD` but missing from the worktree.
    Deleted,
}

/// Status of a changed file in the Git working tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileStatus {
    /// Repository-relative path with `/` separators.
    pub path: String,
    /// Change compared to `HEAD`.
    pub kind: FileChangeKind,
}

/// Kind of diff marker for a single line in the editor gutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffMarkerKind {
    /// No change (never emitted by [`compute_line_diff`], kept for consumers).
    Unchanged,
    /// Line was added.
    Added,
    /// Line replaced one or more old lines.
    Modified,
    /// Old lines were deleted just before this line.
    Deleted,
}

/// Per-line git diff marker exported to `ViewportViewModel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineDiffStatus {
    /// Zero-based line number in the current buffer.
    pub line_number: u32,
    /// Marker kind.
    pub status: DiffMarkerKind,
}

/// Maximum number of HEAD blobs kept by the gutter cache.
const HEAD_CACHE_CAPACITY: usize = 32;

/// Cache of HEAD file contents used by [`GitRepository::diff_buffer_lines`].
/// Entries are valid for a single HEAD commit and dropped when HEAD moves.
#[derive(Default)]
struct HeadBlobCache {
    head: Option<ObjectId>,
    /// Most recently used entries last.
    entries: Vec<(String, Option<Arc<str>>)>,
}

impl HeadBlobCache {
    fn get(&mut self, head: Option<ObjectId>, path: &str) -> Option<Option<Arc<str>>> {
        if self.head != head {
            self.head = head;
            self.entries.clear();
            return None;
        }
        let pos = self.entries.iter().position(|(p, _)| p == path)?;
        let entry = self.entries.remove(pos);
        let value = entry.1.clone();
        self.entries.push(entry);
        Some(value)
    }

    fn put(&mut self, head: Option<ObjectId>, path: String, value: Option<Arc<str>>) {
        if self.head != head {
            self.head = head;
            self.entries.clear();
        }
        self.entries.retain(|(p, _)| *p != path);
        if self.entries.len() >= HEAD_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push((path, value));
    }
}

/// Git repository handle backed by `gix`. Cheap to clone and safe to share across threads.
#[derive(Clone)]
pub struct GitRepository {
    repo: gix::ThreadSafeRepository,
    workdir: PathBuf,
    has_worktree: bool,
    head_cache: Arc<Mutex<HeadBlobCache>>,
}

impl std::fmt::Debug for GitRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitRepository")
            .field("workdir", &self.workdir)
            .field("git_dir", &self.repo.git_dir())
            .finish()
    }
}

impl GitRepository {
    /// Opens or discovers a Git repository starting at `path` (searching parent directories).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, GitError> {
        let repo = gix::discover(path.as_ref()).map_err(|e| GitError::Open(e.to_string()))?;
        let has_worktree = repo.work_dir().is_some();
        let workdir = repo
            .work_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.as_ref().to_path_buf());
        Ok(Self {
            repo: repo.into_sync(),
            workdir,
            has_worktree,
            head_cache: Arc::default(),
        })
    }

    /// Root directory of the worktree.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Path of the `.git` directory.
    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// Thread-local repository instance for one operation.
    pub(crate) fn local(&self) -> gix::Repository {
        self.repo.to_thread_local()
    }

    pub(crate) fn require_worktree(&self) -> Result<&Path, GitError> {
        if self.has_worktree {
            Ok(&self.workdir)
        } else {
            Err(GitError::Bare)
        }
    }

    /// Converts a path (absolute, or relative to the worktree root) into a normalized
    /// repository-relative path with `/` separators. The worktree root maps to `""`.
    pub(crate) fn rela_path(&self, path: &Path) -> Result<String, GitError> {
        let relative = if path.is_absolute() {
            match path.strip_prefix(&self.workdir) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => {
                    let canonical_root = self.workdir.canonicalize()?;
                    let canonical = canonicalize_lenient(path);
                    canonical
                        .strip_prefix(&canonical_root)
                        .map(Path::to_path_buf)
                        .map_err(|_| {
                            GitError::PathOutsideWorktree(path.display().to_string())
                        })?
                }
            }
        } else {
            path.to_path_buf()
        };
        let mut parts: Vec<String> = Vec::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if parts.pop().is_none() {
                        return Err(GitError::PathOutsideWorktree(path.display().to_string()));
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(GitError::PathOutsideWorktree(path.display().to_string()))
                }
            }
        }
        let rela = parts.join("/");
        if rela == ".git" || rela.starts_with(".git/") {
            return Err(GitError::PathOutsideWorktree(path.display().to_string()));
        }
        Ok(rela)
    }

    /// Returns the short name of the current branch (e.g., `"main"`), or `"HEAD (detached)"`.
    pub fn current_branch(&self) -> Result<String, GitError> {
        let repo = self.local();
        match repo.head_name().map_err(repo_err)? {
            Some(name) => Ok(name.shorten().to_str_lossy().into_owned()),
            None => Ok("HEAD (detached)".to_string()),
        }
    }

    /// Reads the content of a file at `HEAD` (lossily decoded as UTF-8), or `None` if
    /// the path is not a file in the `HEAD` tree or `HEAD` is unborn.
    ///
    /// Results are cached per HEAD commit.
    pub fn head_file_content(&self, relative_path: &Path) -> Result<Option<String>, GitError> {
        Ok(self
            .cached_head_content(relative_path)?
            .map(|text| text.to_string()))
    }

    fn cached_head_content(&self, path: &Path) -> Result<Option<Arc<str>>, GitError> {
        let rela = self.rela_path(path)?;
        let repo = self.local();
        let head = util::head_commit_id(&repo)?;
        if let Some(hit) = self
            .head_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(head, &rela)
        {
            return Ok(hit);
        }
        let content = match head {
            None => None,
            Some(commit) => util::blob_at_commit(&repo, commit, &rela)?
                .map(|data| Arc::<str>::from(String::from_utf8_lossy(&data).as_ref())),
        };
        self.head_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .put(head, rela, content.clone());
        Ok(content)
    }

    /// Lists all files changed between `HEAD` and the working directory
    /// (`Added`, `Modified`, `Deleted`), respecting `.gitignore`. Sorted by path.
    pub fn changed_files(&self) -> Result<Vec<GitFileStatus>, GitError> {
        use ChangeKind as K;
        let mut out = Vec::new();
        for entry in self.status_detailed()? {
            let mut push = |path: &str, kind| {
                out.push(GitFileStatus {
                    path: path.to_owned(),
                    kind,
                })
            };
            match (entry.index_status, entry.worktree_status) {
                (Some(K::Renamed), wt) => {
                    if let Some(original) = &entry.original_path {
                        push(original, FileChangeKind::Deleted);
                    }
                    if wt != Some(K::Deleted) {
                        push(&entry.path, FileChangeKind::Added);
                    }
                }
                (Some(K::Added), Some(K::Deleted)) => {}
                (Some(K::Added), _) => push(&entry.path, FileChangeKind::Added),
                (Some(K::Deleted), Some(K::Untracked)) => {
                    push(&entry.path, FileChangeKind::Modified)
                }
                (Some(K::Deleted), _) => push(&entry.path, FileChangeKind::Deleted),
                (_, Some(K::Deleted)) => push(&entry.path, FileChangeKind::Deleted),
                (_, Some(K::Untracked)) => push(&entry.path, FileChangeKind::Added),
                _ => push(&entry.path, FileChangeKind::Modified),
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup();
        Ok(out)
    }

    /// Computes gutter diff markers (`Added`, `Modified`, `Deleted`) for `current_text`
    /// compared against the version of `file_path` in `HEAD`.
    ///
    /// Designed to run on every edit: the HEAD blob is cached per (path, HEAD commit)
    /// and the diff uses the histogram algorithm. Files not in `HEAD` are all `Added`.
    pub fn diff_buffer_lines(
        &self,
        file_path: impl AsRef<Path>,
        current_text: &str,
    ) -> Result<Vec<LineDiffStatus>, GitError> {
        match self.cached_head_content(file_path.as_ref())? {
            Some(old_text) => Ok(compute_line_diff(&old_text, current_text)),
            None => Ok((0..gutter::split_lines(current_text).len() as u32)
                .map(|line_number| LineDiffStatus {
                    line_number,
                    status: DiffMarkerKind::Added,
                })
                .collect()),
        }
    }
}

/// Canonicalizes the longest existing ancestor of `path` and re-appends the rest, so
/// paths of deleted files can still be related to the worktree root.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for name in rest.into_iter().rev() {
        out.push(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_cache_is_lru_and_invalidated_by_head_changes() {
        let a = ObjectId::null(gix::hash::Kind::Sha1);
        let b = ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let mut cache = HeadBlobCache::default();
        assert!(cache.get(Some(a), "x").is_none());
        cache.put(Some(a), "x".into(), Some(Arc::from("content")));
        assert_eq!(cache.get(Some(a), "x").unwrap().as_deref(), Some("content"));
        assert!(cache.get(Some(b), "x").is_none(), "HEAD moved");
        for i in 0..HEAD_CACHE_CAPACITY + 5 {
            cache.put(Some(b), format!("f{i}"), None);
        }
        assert_eq!(cache.entries.len(), HEAD_CACHE_CAPACITY);
        assert!(cache.get(Some(b), "f0").is_none(), "oldest evicted");
        assert!(cache.get(Some(b), "f36").is_some());
    }
}
