//! Unified diff generation (`HEAD` vs index for staged, index vs worktree for unstaged).

use crate::{index_ops, repo_err, util, GitError, GitRepository};
use gix::bstr::ByteSlice;
use gix::diff::blob::{diff, intern::InternedInput, Algorithm};
use gix::index::entry::{Flags, Stage};
use std::ops::Range;
use std::path::Path;

const CONTEXT_LINES: usize = 3;

impl GitRepository {
    /// Generates a Git-style unified diff for `path`.
    ///
    /// * When `staged == true`, compares `HEAD` against `.git/index`.
    /// * When `staged == false`, compares `.git/index` against the working tree.
    pub fn file_diff(&self, path: impl AsRef<Path>, staged: bool) -> Result<String, GitError> {
        let workdir = self.require_worktree()?;
        let rela = self.rela_path(path.as_ref())?;
        if rela.is_empty() {
            return Ok(String::new());
        }
        let repo = self.local();
        let index = util::open_or_create_index(&repo)?;

        // Lookup stage-0 index entry for `rela`.
        let index_entry = index.entries().iter().find(|e| {
            !e.flags.contains(Flags::REMOVE)
                && !e.flags.contains(Flags::INTENT_TO_ADD)
                && e.stage() == Stage::Unconflicted
                && e.path(&index) == rela.as_bytes().as_bstr()
        });

        let (old_bytes, new_bytes) = if staged {
            let old = match util::head_commit_id(&repo)? {
                Some(commit_id) => util::blob_at_commit(&repo, commit_id, &rela)?,
                None => None,
            };
            let new = match index_entry {
                Some(e) => Some(repo.find_object(e.id).map_err(repo_err)?.data.clone()),
                None => None,
            };
            (old, new)
        } else {
            let old = match index_entry {
                Some(e) => Some(repo.find_object(e.id).map_err(repo_err)?.data.clone()),
                None => None,
            };
            let disk_path = workdir.join(&rela);
            let new = match std::fs::symlink_metadata(&disk_path) {
                Ok(meta) if !meta.is_dir() => {
                    let mode = util::mode_for_worktree_path(&disk_path)?;
                    Some(index_ops::read_for_git_storage(
                        &repo,
                        &index,
                        &disk_path,
                        &rela,
                        mode,
                    )?)
                }
                _ => None,
            };
            (old, new)
        };

        if old_bytes == new_bytes {
            return Ok(String::new());
        }
        Ok(format_unified_diff(
            &rela,
            old_bytes.as_deref(),
            new_bytes.as_deref(),
        ))
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    let check_len = bytes.len().min(8000);
    bytes[..check_len].contains(&0)
}

fn format_unified_diff(rela: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> String {
    let (old_label, new_label) = match (old.is_some(), new.is_some()) {
        (false, true) => ("/dev/null".to_string(), format!("b/{rela}")),
        (true, false) => (format!("a/{rela}"), "/dev/null".to_string()),
        _ => (format!("a/{rela}"), format!("b/{rela}")),
    };

    let mut out = format!("diff --git a/{rela} b/{rela}\n");
    if old.is_none() {
        out.push_str("new file mode 100644\n");
    } else if new.is_none() {
        out.push_str("deleted file mode 100644\n");
    }

    let old_slice = old.unwrap_or(&[]);
    let new_slice = new.unwrap_or(&[]);
    if is_binary(old_slice) || is_binary(new_slice) {
        out.push_str(&format!(
            "Binary files {old_label} and {new_label} differ\n"
        ));
        return out;
    }

    out.push_str(&format!("--- {old_label}\n+++ {new_label}\n"));

    let old_text = String::from_utf8_lossy(old_slice);
    let new_text = String::from_utf8_lossy(new_slice);
    let old_lines = crate::gutter::split_lines(&old_text);
    let new_lines = crate::gutter::split_lines(&new_text);

    let mut input = InternedInput::default();
    input.update_before(old_lines.iter().copied());
    input.update_after(new_lines.iter().copied());

    let mut changes: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    diff(
        Algorithm::Histogram,
        &input,
        |before: Range<u32>, after: Range<u32>| {
            changes.push((
                before.start as usize..before.end as usize,
                after.start as usize..after.end as usize,
            ));
        },
    );

    if changes.is_empty() {
        return String::new();
    }

    // Group adjacent changes whose context windows overlap.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (idx, (b, _)) in changes.iter().enumerate() {
        if let Some((_, last_idx)) = groups.last_mut() {
            let prev_end = changes[*last_idx].0.end;
            if b.start <= prev_end + CONTEXT_LINES * 2 {
                *last_idx = idx;
                continue;
            }
        }
        groups.push((idx, idx));
    }

    for (first_idx, last_idx) in groups {
        let first_b = &changes[first_idx].0;
        let first_a = &changes[first_idx].1;
        let last_b = &changes[last_idx].0;
        let last_a = &changes[last_idx].1;

        let pre_ctx = first_b.start.min(CONTEXT_LINES);
        let post_ctx = (old_lines.len() - last_b.end).min(CONTEXT_LINES);

        let old_start = first_b.start - pre_ctx;
        let old_end = last_b.end + post_ctx;
        let new_start = first_a.start - pre_ctx;
        let new_end = last_a.end + post_ctx;

        let old_count = old_end - old_start;
        let new_count = new_end - new_start;

        let old_hdr_line = if old_count == 0 {
            old_start
        } else {
            old_start + 1
        };
        let new_hdr_line = if new_count == 0 {
            new_start
        } else {
            new_start + 1
        };

        out.push_str(&format!(
            "@@ -{old_hdr_line},{old_count} +{new_hdr_line},{new_count} @@\n"
        ));

        let mut cur_old = old_start;
        for (b, a) in &changes[first_idx..=last_idx] {
            for line in &old_lines[cur_old..b.start] {
                out.push(' ');
                out.push_str(line);
                out.push('\n');
            }
            for line in &old_lines[b.start..b.end] {
                out.push('-');
                out.push_str(line);
                out.push('\n');
            }
            for line in &new_lines[a.start..a.end] {
                out.push('+');
                out.push_str(line);
                out.push('\n');
            }
            cur_old = b.end;
        }
        for line in &old_lines[cur_old..old_end] {
            out.push(' ');
            out.push_str(line);
            out.push('\n');
        }
    }

    out
}
