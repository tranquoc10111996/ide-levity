//! Internal `gix` helpers for `HEAD` lookups, tree flattening, and index mutation.

use crate::{repo_err, GitError};
use gix::bstr::{BStr, ByteSlice};
use gix::index::entry::{Flags, Mode, Stage, Stat};
use gix::objs::tree::EntryMode;
use gix::ObjectId;
use std::collections::BTreeMap;
use std::path::Path;

/// Returns the `ObjectId` of the commit at `HEAD`, or `None` when `HEAD` is unborn.
pub(crate) fn head_commit_id(repo: &gix::Repository) -> Result<Option<ObjectId>, GitError> {
    let mut head = repo.head().map_err(repo_err)?;
    if head.is_unborn() {
        return Ok(None);
    }
    match head.peel_to_commit_in_place() {
        Ok(commit) => Ok(Some(commit.id)),
        Err(_) => Ok(None),
    }
}

/// Reads the raw bytes of a blob (or symlink) at `rela_path` in `commit_id`.
pub(crate) fn blob_at_commit(
    repo: &gix::Repository,
    commit_id: ObjectId,
    rela_path: &str,
) -> Result<Option<Vec<u8>>, GitError> {
    if rela_path.is_empty() {
        return Ok(None);
    }
    let commit = repo
        .find_object(commit_id)
        .map_err(repo_err)?
        .try_into_commit()
        .map_err(repo_err)?;
    let tree = commit.tree().map_err(repo_err)?;
    let mut buf = Vec::new();
    let Some(entry) = tree
        .lookup_entry_by_path(rela_path, &mut buf)
        .map_err(repo_err)?
    else {
        return Ok(None);
    };
    if !entry.mode().is_blob_or_symlink() {
        return Ok(None);
    }
    let obj = entry.object().map_err(repo_err)?;
    Ok(Some(obj.data.clone()))
}

/// Flattens the `HEAD` commit's tree into a map of `repo_relative_path -> (EntryMode, ObjectId)`.
/// Returns an empty map if `HEAD` is unborn.
pub(crate) fn head_tree_entries(
    repo: &gix::Repository,
) -> Result<BTreeMap<String, (EntryMode, ObjectId)>, GitError> {
    let Some(commit_id) = head_commit_id(repo)? else {
        return Ok(BTreeMap::new());
    };
    tree_entries_for_commit(repo, commit_id)
}

/// Flattens a commit's tree into `repo_relative_path -> (EntryMode, ObjectId)`.
pub(crate) fn tree_entries_for_commit(
    repo: &gix::Repository,
    commit_id: ObjectId,
) -> Result<BTreeMap<String, (EntryMode, ObjectId)>, GitError> {
    let commit = repo
        .find_object(commit_id)
        .map_err(repo_err)?
        .try_into_commit()
        .map_err(repo_err)?;
    let tree = commit.tree().map_err(repo_err)?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(repo_err)?;
    let mut map = BTreeMap::new();
    for entry in recorder.records {
        if entry.mode.is_tree() {
            continue;
        }
        let path = entry.filepath.to_str_lossy().into_owned();
        map.insert(path, (entry.mode, entry.oid));
    }
    Ok(map)
}

/// Opens `.git/index` or creates an empty index state when no index file exists yet.
pub(crate) fn open_or_create_index(repo: &gix::Repository) -> Result<gix::index::File, GitError> {
    match repo.open_index() {
        Ok(index) => Ok(index),
        Err(gix::worktree::open_index::Error::IndexFile(
            gix::index::file::init::Error::Io(err),
        )) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut file = gix::index::File::from_state(
                gix::index::State::new(repo.object_hash()),
                repo.index_path(),
            );
            let _ = write_index(&mut file);
            Ok(file)
        }
        Err(err) => Err(repo_err(err)),
    }
}

/// Writes `index` back to disk atomically (holding `.git/index.lock`).
pub(crate) fn write_index(index: &mut gix::index::File) -> Result<(), GitError> {
    index.sort_entries();
    index
        .write(gix::index::write::Options::default())
        .map_err(repo_err)
}

/// Computes `gix::index::entry::Stat` for a worktree path (`symlink_metadata`).
pub(crate) fn stat_for_path(path: &Path) -> Result<Stat, GitError> {
    let meta = gix::index::fs::Metadata::from_path_no_follow(path)?;
    Stat::from_fs(&meta).map_err(repo_err)
}

/// Determines the Git index `Mode` for a path in the worktree.
pub(crate) fn mode_for_worktree_path(path: &Path) -> Result<Mode, GitError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(Mode::SYMLINK);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return Ok(Mode::FILE_EXECUTABLE);
        }
    }
    Ok(Mode::FILE)
}

/// Removes all index entries (at any stage) equal to `rela` or inside `rela/`.
pub(crate) fn remove_index_path(state: &mut gix::index::State, rela: &str) {
    let prefix = format!("{rela}/");
    state.remove_entries(|_idx, p, _entry| {
        if rela.is_empty() {
            return true;
        }
        p == BStr::new(rela.as_bytes()) || p.starts_with(prefix.as_bytes())
    });
}

/// Inserts or replaces an unconflicted stage-0 index entry at `rela`.
pub(crate) fn upsert_index_entry(
    state: &mut gix::index::State,
    rela: &str,
    id: ObjectId,
    mode: Mode,
    stat: Stat,
) {
    remove_index_path(state, rela);
    state.dangerously_push_entry(
        stat,
        id,
        Flags::empty(),
        mode,
        BStr::new(rela.as_bytes()),
    );
}

/// Returns whether `path` matches `target` (`target == ""` matches everything,
/// otherwise exact match or directory prefix `target/`).
pub(crate) fn path_matches_target(path: &str, target: &str) -> bool {
    if target.is_empty() || path == target {
        return true;
    }
    path.strip_prefix(target)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Collects unmerged (conflicted) paths in `state`.
pub(crate) fn unmerged_paths(state: &gix::index::State) -> Vec<String> {
    let mut out = Vec::new();
    for entry in state.entries() {
        if entry.flags.contains(Flags::REMOVE) {
            continue;
        }
        if entry.stage() != Stage::Unconflicted {
            let p = entry.path(state).to_str_lossy().into_owned();
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}
