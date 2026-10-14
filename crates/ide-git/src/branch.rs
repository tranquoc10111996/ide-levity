//! Local branch listing, branch creation, and safe branch checkout.

use crate::{index_ops, repo_err, status::ChangeKind, util, GitError, GitRepository};
use gix::bstr::{BString, ByteSlice};
use gix::index::entry::{Mode, Stat};
use gix::refs::{
    transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    Target,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Metadata for a local Git branch (`refs/heads/*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    /// Short branch name (e.g. `"main"`, `"feature/login"`).
    pub name: String,
    /// Whether `HEAD` currently points to this branch.
    pub is_current: bool,
    /// Full hex commit ID at the tip of this branch.
    pub commit_id: String,
    /// 7-character short hex commit ID.
    pub short_id: String,
}

impl GitRepository {
    /// Lists all local branches (`refs/heads/*`), sorted alphabetically by name.
    pub fn branches(&self) -> Result<Vec<BranchInfo>, GitError> {
        let repo = self.local();
        let current = repo
            .head_name()
            .map_err(repo_err)?
            .map(|n| n.shorten().to_str_lossy().into_owned());

        let refs = repo.references().map_err(repo_err)?;
        let local = refs.local_branches().map_err(repo_err)?;
        let mut out = Vec::new();
        for branch_res in local {
            let mut reference = branch_res.map_err(repo_err)?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            let id = match reference.peel_to_id_in_place() {
                Ok(id) => id.detach().to_hex().to_string(),
                Err(_) => continue,
            };
            let short_id = id.chars().take(7).collect();
            let is_current = current.as_deref() == Some(name.as_str());
            out.push(BranchInfo {
                name,
                is_current,
                commit_id: id,
                short_id,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Creates a new local branch `refs/heads/<name>` pointing at the current `HEAD` commit.
    pub fn create_branch(&self, name: &str) -> Result<(), GitError> {
        let full_ref = validate_branch_ref(name)?;
        let repo = self.local();
        let Some(head_id) = util::head_commit_id(&repo)? else {
            return Err(GitError::UnbornHead);
        };
        if repo.try_find_reference(&full_ref).map_err(repo_err)?.is_some() {
            return Err(GitError::BranchExists(name.to_string()));
        }
        repo.reference(
            full_ref,
            head_id,
            PreviousValue::MustNotExist,
            format!("branch: Created from HEAD ({})", head_id.to_hex()),
        )
        .map_err(repo_err)?;
        Ok(())
    }

    /// Checks out an existing local branch `refs/heads/<name>`, updating the working tree,
    /// `.git/index`, and symbolic `HEAD`.
    ///
    /// Refuses with [`GitError::WouldOverwrite`] if any modified, staged, or untracked
    /// working tree file would be overwritten by switching trees.
    pub fn checkout_branch(&self, name: &str) -> Result<(), GitError> {
        let workdir = self.require_worktree()?;
        let full_ref = validate_branch_ref(name)?;
        let repo = self.local();

        let mut target_ref = repo
            .try_find_reference(&full_ref)
            .map_err(repo_err)?
            .ok_or_else(|| GitError::BranchNotFound(name.to_string()))?;
        let target_commit_id = target_ref.peel_to_id_in_place().map_err(repo_err)?.detach();

        let head_map = util::head_tree_entries(&repo)?;
        let target_map = util::tree_entries_for_commit(&repo, target_commit_id)?;

        // Determine paths that differ between HEAD and target branch.
        let mut differing: BTreeSet<String> = BTreeSet::new();
        for (p, h_val) in &head_map {
            if target_map.get(p) != Some(h_val) {
                differing.insert(p.clone());
            }
        }
        for (p, t_val) in &target_map {
            if head_map.get(p) != Some(t_val) {
                differing.insert(p.clone());
            }
        }

        if !differing.is_empty() {
            let status = self.status_detailed()?;
            let mut conflicts = Vec::new();
            for entry in &status {
                let is_dirty = entry.index_status.is_some()
                    || matches!(
                        entry.worktree_status,
                        Some(
                            ChangeKind::Modified
                                | ChangeKind::Deleted
                                | ChangeKind::TypeChange
                                | ChangeKind::Added
                                | ChangeKind::Conflicted
                                | ChangeKind::Untracked
                        )
                    );
                if !is_dirty {
                    continue;
                }
                let collides = differing.iter().any(|d| {
                    d == &entry.path
                        || util::path_matches_target(d, &entry.path)
                        || util::path_matches_target(&entry.path, d)
                });
                if collides {
                    conflicts.push(entry.path.clone());
                }
            }
            if !conflicts.is_empty() {
                conflicts.sort();
                conflicts.dedup();
                return Err(GitError::WouldOverwrite(conflicts));
            }
        }

        let mut index = util::open_or_create_index(&repo)?;

        // 1. Remove files present in HEAD but removed in target.
        for path in &differing {
            if !target_map.contains_key(path) {
                let full = workdir.join(path);
                if std::fs::symlink_metadata(&full).is_ok() {
                    let _ = std::fs::remove_file(&full);
                }
                index_ops::prune_empty_parents(&full, workdir);
                util::remove_index_path(&mut index, path);
            }
        }

        // 2. Write files added or modified in target.
        for path in &differing {
            if let Some((entry_mode, oid)) = target_map.get(path) {
                let mode = Mode::from_bits_truncate(u32::from(entry_mode.0));
                let obj = repo.find_object(*oid).map_err(repo_err)?;
                let full = workdir.join(path);
                index_ops::write_worktree_entry(&repo, &index, &full, path, mode, &obj.data)?;
                let stat = util::stat_for_path(&full).unwrap_or_else(|_| Stat::default());
                util::upsert_index_entry(&mut index, path, *oid, mode, stat);
            }
        }

        util::write_index(&mut index)?;

        // 3. Point HEAD symbolically to `refs/heads/<name>`.
        let target_full_name: gix::refs::FullName = full_ref.try_into().map_err(repo_err)?;
        let head_edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: BString::from(format!("checkout: moving to {name}")),
                },
                expected: PreviousValue::Any,
                new: Target::Symbolic(target_full_name),
            },
            name: "HEAD".try_into().map_err(repo_err)?,
            deref: false,
        };
        repo.edit_reference(head_edit).map_err(repo_err)?;
        Ok(())
    }
}

fn validate_branch_ref(name: &str) -> Result<String, GitError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "HEAD" || trimmed.starts_with('-') {
        return Err(GitError::InvalidBranchName(
            name.to_string(),
            "branch name cannot be empty, 'HEAD', or start with '-'".into(),
        ));
    }
    let full = format!("refs/heads/{trimmed}");
    gix::validate::reference::name(full.as_bytes().as_bstr())
        .map_err(|e| GitError::InvalidBranchName(name.to_string(), e.to_string()))?;
    Ok(full)
}
