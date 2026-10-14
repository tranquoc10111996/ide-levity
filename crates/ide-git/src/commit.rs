//! Commit creation (tree building from `.git/index`) and `HEAD` commit summary inspection.

use crate::{repo_err, util, GitError, GitRepository};
use gix::bstr::{BString, ByteSlice};
use gix::index::entry::{Flags, Stage};
use gix::objs::tree::{Entry, EntryMode};
use gix::ObjectId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Metadata summary of a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSummary {
    /// Full hex object id.
    pub id: String,
    /// 7-character short hex id.
    pub short_id: String,
    /// First line of the commit message.
    pub summary: String,
    /// Full commit message.
    pub message: String,
    /// Author name.
    pub author_name: String,
    /// Author email.
    pub author_email: String,
    /// Author timestamp in Unix seconds.
    pub timestamp_seconds: i64,
}

impl GitRepository {
    /// Returns the summary of the `HEAD` commit, or `None` if `HEAD` is unborn.
    pub fn head_commit_summary(&self) -> Result<Option<CommitSummary>, GitError> {
        let repo = self.local();
        let Some(commit_id) = util::head_commit_id(&repo)? else {
            return Ok(None);
        };
        let commit = repo
            .find_object(commit_id)
            .map_err(repo_err)?
            .try_into_commit()
            .map_err(repo_err)?;
        let decoded = commit.decode().map_err(repo_err)?;
        let message = decoded.message.to_str_lossy().into_owned();
        let summary = message
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let author = &decoded.author;
        let id = commit_id.to_hex().to_string();
        let short_id = id.chars().take(7).collect();
        Ok(Some(CommitSummary {
            id,
            short_id,
            summary,
            message,
            author_name: author.name.to_str_lossy().into_owned(),
            author_email: author.email.to_str_lossy().into_owned(),
            timestamp_seconds: author.time.seconds,
        }))
    }

    /// Creates a new commit on `HEAD` from the currently staged index entries.
    ///
    /// Uses `author_name` / `author_email` when provided, falling back to `user.name` and
    /// `user.email` from the repository's Git configuration. Returns the full hex commit ID.
    pub fn commit(
        &self,
        message: &str,
        author_name: Option<&str>,
        author_email: Option<&str>,
    ) -> Result<String, GitError> {
        let cleaned_message = clean_commit_message(message)?;
        let repo = self.local();
        let index = util::open_or_create_index(&repo)?;

        let unmerged = util::unmerged_paths(&index);
        if !unmerged.is_empty() {
            return Err(GitError::Unmerged(unmerged));
        }

        let head_commit = util::head_commit_id(&repo)?;
        let head_tree = match head_commit {
            Some(id) => Some(
                repo.find_object(id)
                    .map_err(repo_err)?
                    .try_into_commit()
                    .map_err(repo_err)?
                    .tree_id()
                    .map_err(repo_err)?
                    .detach(),
            ),
            None => None,
        };

        let has_stage0 = index.entries().iter().any(|e| {
            !e.flags.contains(Flags::REMOVE)
                && !e.flags.contains(Flags::INTENT_TO_ADD)
                && !e.mode.is_sparse()
                && e.stage() == Stage::Unconflicted
        });
        if head_commit.is_none() && !has_stage0 {
            return Err(GitError::NothingToCommit);
        }

        let tree_id = write_tree_from_index(&repo, &index)?;
        if Some(tree_id) == head_tree {
            return Err(GitError::NothingToCommit);
        }

        let cfg_author = repo.author().and_then(|r| r.ok());
        let cfg_committer = repo.committer().and_then(|r| r.ok());

        let (name, email) = resolve_identity(
            author_name,
            author_email,
            cfg_author
                .as_ref()
                .map(|a| a.name.to_str_lossy().into_owned())
                .as_deref(),
            cfg_author
                .as_ref()
                .map(|a| a.email.to_str_lossy().into_owned())
                .as_deref(),
        )?;

        let (committer_name, committer_email) = resolve_identity(
            author_name,
            author_email,
            cfg_committer
                .as_ref()
                .map(|c| c.name.to_str_lossy().into_owned())
                .as_deref()
                .or(Some(name.as_str())),
            cfg_committer
                .as_ref()
                .map(|c| c.email.to_str_lossy().into_owned())
                .as_deref()
                .or(Some(email.as_str())),
        )?;

        let now = gix::date::Time::now_local_or_utc();
        let author_sig = gix::actor::Signature {
            name: BString::from(name),
            email: BString::from(email),
            time: now,
        };
        let committer_sig = gix::actor::Signature {
            name: BString::from(committer_name),
            email: BString::from(committer_email),
            time: now,
        };

        let parents: Vec<ObjectId> = head_commit.into_iter().collect();
        let new_id = repo
            .commit_as(
                committer_sig.to_ref(),
                author_sig.to_ref(),
                "HEAD",
                &cleaned_message,
                tree_id,
                parents,
            )
            .map_err(repo_err)?
            .detach();

        Ok(new_id.to_hex().to_string())
    }
}

fn clean_commit_message(raw: &str) -> Result<String, GitError> {
    let lines: Vec<&str> = raw.lines().map(str::trim_end).collect();
    let first = lines.iter().position(|l| !l.is_empty());
    let last = lines.iter().rposition(|l| !l.is_empty());
    let (Some(start), Some(end)) = (first, last) else {
        return Err(GitError::EmptyMessage);
    };
    let mut cleaned = lines[start..=end].join("\n");
    cleaned.push('\n');
    Ok(cleaned)
}

pub(crate) fn resolve_identity(
    explicit_name: Option<&str>,
    explicit_email: Option<&str>,
    config_name: Option<&str>,
    config_email: Option<&str>,
) -> Result<(String, String), GitError> {
    let name = explicit_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| config_name.map(str::trim).filter(|s| !s.is_empty()))
        .ok_or(GitError::MissingIdentity)?;
    let email = explicit_email
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| config_email.map(str::trim).filter(|s| !s.is_empty()))
        .ok_or(GitError::MissingIdentity)?;
    Ok((name.to_string(), email.to_string()))
}

#[derive(Default)]
struct TreeDir {
    files: Vec<(BString, EntryMode, ObjectId)>,
    dirs: BTreeMap<BString, TreeDir>,
}

impl TreeDir {
    fn insert(&mut self, parts: &[&[u8]], mode: EntryMode, oid: ObjectId) {
        match parts {
            [] => {}
            [leaf] => {
                self.files
                    .push((BString::from(*leaf), mode, oid));
            }
            [dir, rest @ ..] => {
                self.dirs
                    .entry(BString::from(*dir))
                    .or_default()
                    .insert(rest, mode, oid);
            }
        }
    }

    fn write(&self, repo: &gix::Repository) -> Result<ObjectId, GitError> {
        let mut entries: Vec<Entry> = Vec::with_capacity(self.files.len() + self.dirs.len());
        for (name, mode, oid) in &self.files {
            entries.push(Entry {
                mode: *mode,
                filename: name.clone(),
                oid: *oid,
            });
        }
        let tree_mode = EntryMode::try_from(0o040000u32).expect("valid tree mode");
        for (name, child) in &self.dirs {
            let child_oid = child.write(repo)?;
            entries.push(Entry {
                mode: tree_mode,
                filename: name.clone(),
                oid: child_oid,
            });
        }
        // Git requires tree entries sorted as if directory names had a trailing '/'.
        entries.sort_by(tree_entry_cmp);
        let tree = gix::objs::Tree { entries };
        Ok(repo.write_object(&tree).map_err(repo_err)?.detach())
    }
}

fn tree_entry_cmp(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    let a_bytes = a.filename.as_slice();
    let b_bytes = b.filename.as_slice();
    let min_len = a_bytes.len().min(b_bytes.len());
    match a_bytes[..min_len].cmp(&b_bytes[..min_len]) {
        std::cmp::Ordering::Equal => {
            let a_next = a_bytes
                .get(min_len)
                .copied()
                .or_else(|| a.mode.is_tree().then_some(b'/'));
            let b_next = b_bytes
                .get(min_len)
                .copied()
                .or_else(|| b.mode.is_tree().then_some(b'/'));
            a_next.cmp(&b_next)
        }
        other => other,
    }
}

/// Builds and writes nested tree objects from all unconflicted stage-0 entries in `index`.
pub(crate) fn write_tree_from_index(
    repo: &gix::Repository,
    index: &gix::index::State,
) -> Result<ObjectId, GitError> {
    let mut root = TreeDir::default();
    for entry in index.entries() {
        if entry.flags.contains(Flags::REMOVE)
            || entry.flags.contains(Flags::INTENT_TO_ADD)
            || entry.mode.is_sparse()
            || entry.stage() != Stage::Unconflicted
        {
            continue;
        }
        let Some(mode) = entry.mode.to_tree_entry_mode() else {
            continue;
        };
        let path = entry.path(index);
        let parts: Vec<&[u8]> = path.split(|&b| b == b'/').filter(|p| !p.is_empty()).collect();
        root.insert(&parts, mode, entry.id);
    }
    root.write(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_resolution_requires_both_name_and_email() {
        assert!(matches!(
            resolve_identity(None, None, None, None),
            Err(GitError::MissingIdentity)
        ));
        assert!(matches!(
            resolve_identity(Some("Alice"), None, None, None),
            Err(GitError::MissingIdentity)
        ));
        assert_eq!(
            resolve_identity(None, None, Some("Bob"), Some("bob@example.com")).unwrap(),
            ("Bob".into(), "bob@example.com".into())
        );
        assert_eq!(
            resolve_identity(
                Some("Override"),
                Some("o@example.com"),
                Some("Bob"),
                Some("bob@example.com")
            )
            .unwrap(),
            ("Override".into(), "o@example.com".into())
        );
    }
}
