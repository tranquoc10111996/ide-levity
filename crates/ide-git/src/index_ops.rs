//! Staging, unstaging, worktree discard, and untracked file cleanup operations.

use crate::{repo_err, status::ChangeKind, util, GitError, GitRepository};
use gix::bstr::ByteSlice;
use gix::index::entry::{Flags, Mode, Stage, Stat};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

impl GitRepository {
    /// Stages the given paths (files or directories, relative to worktree or absolute)
    /// into `.git/index`. Staging a deleted file removes it from the index.
    pub fn stage(&self, paths: &[impl AsRef<Path>]) -> Result<(), GitError> {
        let workdir = self.require_worktree()?;
        if paths.is_empty() {
            return Ok(());
        }
        let targets: Vec<String> = paths
            .iter()
            .map(|p| self.rela_path(p.as_ref()))
            .collect::<Result<_, _>>()?;

        let status = self.status_detailed()?;
        let repo = self.local();
        let mut index = util::open_or_create_index(&repo)?;

        // Collect all candidate repo-relative file paths to stage or remove.
        let mut to_stage: BTreeSet<String> = BTreeSet::new();
        let mut to_remove: BTreeSet<String> = BTreeSet::new();

        for entry in &status {
            if !targets
                .iter()
                .any(|t| util::path_matches_target(&entry.path, t))
            {
                continue;
            }
            let disk_path = workdir.join(&entry.path);
            if std::fs::symlink_metadata(&disk_path).is_ok() {
                to_stage.insert(entry.path.clone());
            } else {
                to_remove.insert(entry.path.clone());
            }
        }

        // Also handle explicit file paths that might be conflicted or already match worktree.
        for t in &targets {
            if t.is_empty() {
                continue;
            }
            let disk_path = workdir.join(t);
            match std::fs::symlink_metadata(&disk_path) {
                Ok(meta) if !meta.is_dir() => {
                    to_stage.insert(t.clone());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    to_remove.insert(t.clone());
                }
                Err(e) => return Err(GitError::Io(e)),
            }
        }

        for rela in to_remove {
            util::remove_index_path(&mut index, &rela);
        }

        for rela in to_stage {
            let disk_path = workdir.join(&rela);
            let mode = util::mode_for_worktree_path(&disk_path)?;
            let stat = util::stat_for_path(&disk_path)?;
            let bytes = read_for_git_storage(&repo, &index, &disk_path, &rela, mode)?;
            let blob_id = repo.write_blob(&bytes).map_err(repo_err)?.detach();
            util::upsert_index_entry(&mut index, &rela, blob_id, mode, stat);
        }

        util::write_index(&mut index)
    }

    /// Stages all modified, added, deleted, and untracked files in the working tree.
    pub fn stage_all(&self) -> Result<(), GitError> {
        self.stage(&[Path::new("")])
    }

    /// Unstages the given paths: resets their index entries back to `HEAD` (or removes them
    /// from the index if they did not exist in `HEAD`).
    pub fn unstage(&self, paths: &[impl AsRef<Path>]) -> Result<(), GitError> {
        let _workdir = self.require_worktree()?;
        if paths.is_empty() {
            return Ok(());
        }
        let targets: Vec<String> = paths
            .iter()
            .map(|p| self.rela_path(p.as_ref()))
            .collect::<Result<_, _>>()?;

        let repo = self.local();
        let head_map = util::head_tree_entries(&repo)?;
        let mut index = util::open_or_create_index(&repo)?;

        let mut affected: BTreeSet<String> = BTreeSet::new();
        for path in head_map.keys() {
            if targets.iter().any(|t| util::path_matches_target(path, t)) {
                affected.insert(path.clone());
            }
        }
        for entry in index.entries() {
            if entry.flags.contains(Flags::REMOVE) {
                continue;
            }
            let p = entry.path(&index).to_str_lossy().into_owned();
            if targets.iter().any(|t| util::path_matches_target(&p, t)) {
                affected.insert(p);
            }
        }

        for rela in affected {
            util::remove_index_path(&mut index, &rela);
            if let Some((entry_mode, oid)) = head_map.get(&rela) {
                let mode = Mode::from_bits_truncate(u32::from(entry_mode.0));
                util::upsert_index_entry(&mut index, &rela, *oid, mode, Stat::default());
            }
        }

        util::write_index(&mut index)
    }

    /// Discards unstaged working tree modifications for tracked files by restoring their
    /// content and permissions from the stage-0 index entry.
    ///
    /// Untracked files are never deleted by this method; use [`Self::delete_untracked`] instead.
    pub fn discard_worktree_changes(&self, paths: &[impl AsRef<Path>]) -> Result<(), GitError> {
        let workdir = self.require_worktree()?;
        if paths.is_empty() {
            return Ok(());
        }
        let targets: Vec<String> = paths
            .iter()
            .map(|p| self.rela_path(p.as_ref()))
            .collect::<Result<_, _>>()?;

        let status = self.status_detailed()?;
        let repo = self.local();
        let mut index = util::open_or_create_index(&repo)?;

        // Snapshot stage-0 entries.
        let mut stage0_map = std::collections::BTreeMap::new();
        for entry in index.entries() {
            if entry.flags.contains(Flags::REMOVE)
                || entry.flags.contains(Flags::INTENT_TO_ADD)
                || entry.stage() != Stage::Unconflicted
            {
                continue;
            }
            let p = entry.path(&index).to_str_lossy().into_owned();
            stage0_map.insert(p, (entry.mode, entry.id));
        }

        let mut restored_any = false;
        for item in status {
            if !targets
                .iter()
                .any(|t| util::path_matches_target(&item.path, t))
            {
                continue;
            }
            match item.worktree_status {
                Some(ChangeKind::Modified | ChangeKind::Deleted | ChangeKind::TypeChange) => {}
                _ => continue,
            }
            let Some(&(mode, oid)) = stage0_map.get(&item.path) else {
                continue;
            };
            let disk_path = workdir.join(&item.path);
            let obj = repo.find_object(oid).map_err(repo_err)?;
            write_worktree_entry(&repo, &index, &disk_path, &item.path, mode, &obj.data)?;
            if let Ok(stat) = util::stat_for_path(&disk_path) {
                util::upsert_index_entry(&mut index, &item.path, oid, mode, stat);
                restored_any = true;
            }
        }

        if restored_any {
            util::write_index(&mut index)?;
        }
        Ok(())
    }

    /// Deletes untracked files matching `paths` and prunes empty parent directories.
    ///
    /// Returns the sorted list of repository-relative paths that were deleted.
    /// Errors with [`GitError::NotUntracked`] if a non-empty path target matches no untracked file.
    pub fn delete_untracked(&self, paths: &[impl AsRef<Path>]) -> Result<Vec<String>, GitError> {
        let workdir = self.require_worktree()?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let targets: Vec<String> = paths
            .iter()
            .map(|p| self.rela_path(p.as_ref()))
            .collect::<Result<_, _>>()?;

        let status = self.status_detailed()?;
        let untracked: Vec<String> = status
            .into_iter()
            .filter(|e| e.worktree_status == Some(ChangeKind::Untracked) && e.index_status.is_none())
            .map(|e| e.path)
            .collect();

        let mut to_delete: BTreeSet<String> = BTreeSet::new();
        for t in &targets {
            let matched: Vec<&String> = untracked
                .iter()
                .filter(|p| util::path_matches_target(p, t))
                .collect();
            if matched.is_empty() && !t.is_empty() {
                return Err(GitError::NotUntracked(t.clone()));
            }
            for m in matched {
                to_delete.insert(m.clone());
            }
        }

        let mut deleted = Vec::with_capacity(to_delete.len());
        for rela in to_delete {
            let full = workdir.join(&rela);
            if std::fs::symlink_metadata(&full).is_ok() {
                std::fs::remove_file(&full)?;
            }
            prune_empty_parents(&full, workdir);
            deleted.push(rela);
        }
        Ok(deleted)
    }
}

/// Reads a worktree file (or symlink target) and applies Git's `convert_to_git` filter pipeline.
pub(crate) fn read_for_git_storage(
    repo: &gix::Repository,
    index: &gix::index::State,
    disk_path: &Path,
    rela_path: &str,
    mode: Mode,
) -> Result<Vec<u8>, GitError> {
    if mode == Mode::SYMLINK {
        let target = std::fs::read_link(disk_path)?;
        return Ok(gix::path::into_bstr(target).into_owned().into());
    }
    let raw = std::fs::read(disk_path)?;
    if let Ok((mut pipeline, _)) = repo.filter_pipeline(None) {
        if let Ok(mut outcome) =
            pipeline.convert_to_git(raw.as_slice(), Path::new(rela_path), index)
        {
            let mut converted = Vec::with_capacity(raw.len());
            outcome.read_to_end(&mut converted)?;
            return Ok(converted);
        }
    }
    Ok(raw)
}

/// Writes a blob (or symlink) from the object database out to `disk_path`, applying
/// Git's `convert_to_worktree` filter pipeline and executable mode bits.
pub(crate) fn write_worktree_entry(
    repo: &gix::Repository,
    _index: &gix::index::State,
    disk_path: &Path,
    rela_path: &str,
    mode: Mode,
    blob_data: &[u8],
) -> Result<(), GitError> {
    if let Some(parent) = disk_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(disk_path) {
        if meta.is_dir() {
            std::fs::remove_dir_all(disk_path)?;
        } else {
            std::fs::remove_file(disk_path)?;
        }
    }
    if mode == Mode::SYMLINK {
        #[cfg(unix)]
        {
            let target = gix::path::from_bstr(std::borrow::Cow::Borrowed(blob_data.as_bstr()));
            std::os::unix::fs::symlink(target, disk_path)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            std::fs::write(disk_path, blob_data)?;
            return Ok(());
        }
    }

    let mut to_write: Option<Vec<u8>> = None;
    if let Ok((mut pipeline, _)) = repo.filter_pipeline(None) {
        if let Ok(mut outcome) = pipeline.convert_to_worktree(
            blob_data,
            rela_path.as_bytes().as_bstr(),
            gix::filter::plumbing::driver::apply::Delay::Forbid,
        ) {
            let mut buf = Vec::with_capacity(blob_data.len());
            if outcome.read_to_end(&mut buf).is_ok() {
                to_write = Some(buf);
            }
        }
    }
    std::fs::write(disk_path, to_write.as_deref().unwrap_or(blob_data))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm_mode = if mode == Mode::FILE_EXECUTABLE {
            0o755
        } else {
            0o644
        };
        std::fs::set_permissions(disk_path, std::fs::Permissions::from_mode(perm_mode))?;
    }
    Ok(())
}

/// Removes empty parent directories of `file_path` up to (but not including) `workdir`.
pub(crate) fn prune_empty_parents(file_path: &Path, workdir: &Path) {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if dir == workdir || !dir.starts_with(workdir) {
            break;
        }
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}
