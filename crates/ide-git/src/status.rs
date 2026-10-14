//! Detailed repository status (HEAD vs index for staged changes, index vs worktree for unstaged).

use crate::{repo_err, util, GitError, GitRepository};
use gix::bstr::ByteSlice;
use gix::index::entry::{Flags, Stage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Kind of change in the index (staged) or working tree (unstaged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    /// New file added to the index.
    Added,
    /// Content or executable bit changed.
    Modified,
    /// File removed.
    Deleted,
    /// File renamed (index-side exact rename detection).
    Renamed,
    /// File type changed (e.g. regular file <-> symlink).
    TypeChange,
    /// File in the working tree that is neither tracked nor ignored.
    Untracked,
    /// Unresolved merge conflict in the index.
    Conflicted,
}

/// Combined staged and unstaged status for one repository-relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    /// Repository-relative path with `/` separators.
    pub path: String,
    /// Staged change (`HEAD` -> index), if any.
    pub index_status: Option<ChangeKind>,
    /// Unstaged change (index -> worktree), if any.
    pub worktree_status: Option<ChangeKind>,
    /// Original path before rename when `index_status == Some(ChangeKind::Renamed)`.
    pub original_path: Option<String>,
}

impl GitRepository {
    /// Computes detailed repository status distinguishing staged (`index_status`) and
    /// unstaged (`worktree_status`) changes, while respecting `.gitignore`.
    pub fn status_detailed(&self) -> Result<Vec<StatusEntry>, GitError> {
        let _workdir = self.require_worktree()?;
        let repo = self.local();
        let head_map = util::head_tree_entries(&repo)?;
        let index = util::open_or_create_index(&repo)?;

        // 1. Compare HEAD tree against index entries.
        let mut staged: BTreeMap<String, (ChangeKind, Option<String>)> = BTreeMap::new();
        let mut conflicted: BTreeSet<String> = BTreeSet::new();
        let mut index_stage0: BTreeMap<String, (gix::objs::tree::EntryMode, gix::ObjectId)> =
            BTreeMap::new();
        let mut intent_to_add: BTreeSet<String> = BTreeSet::new();

        for entry in index.entries() {
            if entry.flags.contains(Flags::REMOVE) || entry.mode.is_sparse() {
                continue;
            }
            let path = entry.path(&index).to_str_lossy().into_owned();
            if entry.stage() != Stage::Unconflicted {
                conflicted.insert(path);
                continue;
            }
            if entry.flags.contains(Flags::INTENT_TO_ADD) {
                intent_to_add.insert(path);
                continue;
            }
            if let Some(tree_mode) = entry.mode.to_tree_entry_mode() {
                index_stage0.insert(path, (tree_mode, entry.id));
            }
        }

        let mut deleted_head: Vec<(String, gix::objs::tree::EntryMode, gix::ObjectId)> = Vec::new();
        let mut added_index: Vec<(String, gix::objs::tree::EntryMode, gix::ObjectId)> = Vec::new();

        for (path, (h_mode, h_id)) in &head_map {
            if conflicted.contains(path) {
                continue;
            }
            match index_stage0.get(path) {
                None => deleted_head.push((path.clone(), *h_mode, *h_id)),
                Some((i_mode, i_id)) => {
                    if i_id != h_id || i_mode != h_mode {
                        let kind = if i_mode.kind() != h_mode.kind() {
                            ChangeKind::TypeChange
                        } else {
                            ChangeKind::Modified
                        };
                        staged.insert(path.clone(), (kind, None));
                    }
                }
            }
        }

        for (path, (i_mode, i_id)) in &index_stage0 {
            if conflicted.contains(path) {
                continue;
            }
            if !head_map.contains_key(path) {
                added_index.push((path.clone(), *i_mode, *i_id));
            }
        }

        // Exact OID rename pairing between deleted HEAD entries and added index entries.
        let mut used_deleted = vec![false; deleted_head.len()];
        for (add_path, add_mode, add_id) in added_index {
            if let Some(idx) = deleted_head
                .iter()
                .enumerate()
                .position(|(i, (_, del_mode, del_id))| {
                    !used_deleted[i] && *del_id == add_id && del_mode.kind() == add_mode.kind()
                })
            {
                used_deleted[idx] = true;
                let orig = deleted_head[idx].0.clone();
                staged.insert(add_path, (ChangeKind::Renamed, Some(orig)));
            } else {
                staged.insert(add_path, (ChangeKind::Added, None));
            }
        }
        for (i, (del_path, _, _)) in deleted_head.into_iter().enumerate() {
            if !used_deleted[i] {
                staged.insert(del_path, (ChangeKind::Deleted, None));
            }
        }
        for path in &conflicted {
            staged.insert(path.clone(), (ChangeKind::Conflicted, None));
        }

        // 2. Compare index against worktree using gix::Repository::index_worktree_status.
        let mut collector = WorktreeCollector::default();
        let opts = gix::status::index_worktree::Options {
            dirwalk_options: repo.dirwalk_options().ok().map(|o| {
                o.emit_untracked(gix::dir::walk::EmissionMode::Matching)
                    .emit_ignored(None)
                    .emit_empty_directories(false)
            }),
            ..Default::default()
        };
        repo.index_worktree_status(
            &index,
            std::iter::empty::<&gix::bstr::BStr>(),
            &mut collector,
            gix::status::plumbing::index_as_worktree::traits::FastEq,
            NoSubmodules,
            &mut gix::progress::Discard,
            &std::sync::atomic::AtomicBool::new(false),
            opts,
        )
        .map_err(repo_err)?;

        let mut worktree = collector.items;
        for path in intent_to_add {
            worktree.entry(path).or_insert(ChangeKind::Added);
        }

        let mut all_paths: BTreeSet<String> = staged.keys().cloned().collect();
        all_paths.extend(worktree.keys().cloned());

        let mut entries = Vec::with_capacity(all_paths.len());
        for path in all_paths {
            let (index_status, original_path) = match staged.remove(&path) {
                Some((k, orig)) => (Some(k), orig),
                None => (None, None),
            };
            let worktree_status = worktree.remove(&path);
            entries.push(StatusEntry {
                path,
                index_status,
                worktree_status,
                original_path,
            });
        }
        Ok(entries)
    }
}

#[derive(Clone)]
struct NoSubmodules;

impl gix::status::plumbing::index_as_worktree::traits::SubmoduleStatus for NoSubmodules {
    type Output = ();
    type Error = std::convert::Infallible;

    fn status(
        &mut self,
        _entry: &gix::index::Entry,
        _rela_path: &gix::bstr::BStr,
    ) -> Result<Option<Self::Output>, Self::Error> {
        Ok(None)
    }
}

#[derive(Default)]
struct WorktreeCollector {
    items: BTreeMap<String, ChangeKind>,
}

impl<'index> gix::status::plumbing::index_as_worktree_with_renames::VisitEntry<'index>
    for WorktreeCollector
{
    type ContentChange = ();
    type SubmoduleStatus = ();

    fn visit_entry(
        &mut self,
        entry: gix::status::plumbing::index_as_worktree_with_renames::Entry<'index, (), ()>,
    ) {
        let Some(summary) = entry.summary() else {
            return;
        };
        use gix::status::plumbing::index_as_worktree_with_renames::Summary;
        let kind = match summary {
            Summary::Added => ChangeKind::Untracked,
            Summary::IntentToAdd => ChangeKind::Added,
            Summary::Removed => ChangeKind::Deleted,
            Summary::Modified => ChangeKind::Modified,
            Summary::TypeChange => ChangeKind::TypeChange,
            Summary::Conflict => ChangeKind::Conflicted,
            Summary::Copied | Summary::Renamed => ChangeKind::Modified,
        };
        let path = entry.destination_rela_path().to_str_lossy().into_owned();
        self.items.insert(path, kind);
    }
}
