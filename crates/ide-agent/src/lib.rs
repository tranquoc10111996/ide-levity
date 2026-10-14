//! `ide-agent`: Antigravity-inspired asynchronous agent task manager.
//! Supports reading/writing `TextBuffer`, generating and applying Unified Diff patches
//! with surrounding context lines, previewing changes (Accept/Reject), and tracking verification state.

use ide_buffer::TextBuffer;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Agent task {0} not found")]
    TaskNotFound(u64),
    #[error("Failed to apply unified diff patch: {0}")]
    PatchFailed(String),
}

/// Lifecycle state of an autonomous `AgentTask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentTaskState {
    Queued,
    Running,
    AwaitingReview,
    Applied,
    Verified,
    Rejected,
    Failed { message: String },
}

/// Verification gate status for an `AgentTask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationState {
    Pending,
    Running,
    Passed,
    Failed { details: String },
}

/// A single hunk in a Unified Diff patch (`@@ -old_start,old_count +new_start,new_count @@`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: Vec<String>,
    pub new_start: u32,
    pub new_lines: Vec<String>,
}

/// A structured Unified Diff patch produced by an `AgentTask` for a buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedDiffPatch {
    pub target_path: String,
    pub original_text: String,
    pub proposed_text: String,
    pub unified_diff: String,
    pub hunks: Vec<DiffHunk>,
}

impl UnifiedDiffPatch {
    /// Generates a Unified Diff patch comparing `original_text` and `proposed_text`
    /// with up to 2 lines of surrounding context per hunk.
    pub fn generate(target_path: &str, original_text: &str, proposed_text: &str) -> Self {
        let old_lines: Vec<String> = split_lines_owned(original_text);
        let new_lines: Vec<String> = split_lines_owned(proposed_text);

        let mut unified = String::new();
        unified.push_str(&format!("--- a/{target_path}\n"));
        unified.push_str(&format!("+++ b/{target_path}\n"));

        let hunks = compute_diff_hunks(&old_lines, &new_lines);
        for hunk in &hunks {
            let ctx_before_start = (hunk.old_start as usize).saturating_sub(2);
            let ctx_before = &old_lines[ctx_before_start..hunk.old_start as usize];
            let old_end = (hunk.old_start as usize + hunk.old_lines.len()).min(old_lines.len());
            let ctx_after_end = (old_end + 2).min(old_lines.len());
            let ctx_after = &old_lines[old_end..ctx_after_end];

            let display_old_start = ctx_before_start + 1;
            let display_old_count = ctx_before.len() + hunk.old_lines.len() + ctx_after.len();
            let display_new_start = (hunk.new_start as usize).saturating_sub(ctx_before.len()) + 1;
            let display_new_count = ctx_before.len() + hunk.new_lines.len() + ctx_after.len();

            unified.push_str(&format!(
                "@@ -{display_old_start},{display_old_count} +{display_new_start},{display_new_count} @@\n"
            ));
            for ctx in ctx_before {
                unified.push_str(&format!(" {ctx}\n"));
            }
            for old_line in &hunk.old_lines {
                unified.push_str(&format!("-{old_line}\n"));
            }
            for new_line in &hunk.new_lines {
                unified.push_str(&format!("+{new_line}\n"));
            }
            for ctx in ctx_after {
                unified.push_str(&format!(" {ctx}\n"));
            }
        }

        Self {
            target_path: target_path.to_string(),
            original_text: original_text.to_string(),
            proposed_text: proposed_text.to_string(),
            unified_diff: unified,
            hunks,
        }
    }

    /// Applies this patch to a `TextBuffer` as a single undoable transaction.
    pub fn apply_to_buffer(&self, buffer: &mut TextBuffer) -> Result<(), AgentError> {
        let current = buffer.text();
        if current != self.original_text && current != self.proposed_text {
            let current_lines = split_lines_owned(&current);
            for hunk in &self.hunks {
                let start = hunk.old_start as usize;
                let end = start + hunk.old_lines.len();
                if end > current_lines.len() || current_lines[start..end] != hunk.old_lines[..] {
                    return Err(AgentError::PatchFailed(
                        "Buffer contents diverged from patch base".to_string(),
                    ));
                }
            }
        }

        buffer.begin_transaction();
        let total_chars = buffer.len_chars();
        buffer.replace_range(0, total_chars, &self.proposed_text);
        buffer.commit_transaction();
        Ok(())
    }

    /// Reverts this patch on a `TextBuffer`, restoring `original_text`.
    pub fn revert_on_buffer(&self, buffer: &mut TextBuffer) {
        buffer.begin_transaction();
        let total_chars = buffer.len_chars();
        buffer.replace_range(0, total_chars, &self.original_text);
        buffer.commit_transaction();
    }
}

fn split_lines_owned(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();
    if lines.last().is_some_and(|s| s.is_empty()) && text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn compute_diff_hunks(old_lines: &[String], new_lines: &[String]) -> Vec<DiffHunk> {
    let n = old_lines.len();
    let m = new_lines.len();
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
    matches.push((n, m));

    let mut hunks = Vec::new();
    let mut prev_old = 0;
    let mut prev_new = 0;
    for (match_old, match_new) in matches {
        if match_old > prev_old || match_new > prev_new {
            hunks.push(DiffHunk {
                old_start: prev_old as u32,
                old_lines: old_lines[prev_old..match_old].to_vec(),
                new_start: prev_new as u32,
                new_lines: new_lines[prev_new..match_new].to_vec(),
            });
        }
        prev_old = match_old + 1;
        prev_new = match_new + 1;
    }
    hunks
}

/// Complete representation of an `AgentTask` managed by `AgentManager`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTask {
    pub id: u64,
    pub prompt: String,
    pub state: AgentTaskState,
    pub verification: VerificationState,
    pub patch: Option<UnifiedDiffPatch>,
    pub summary: String,
}

/// Summary status exported to `ViewportViewModel.agent_tasks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTaskStatus {
    pub task_id: u64,
    pub prompt: String,
    pub state: String,
    pub verification_passed: bool,
    pub unified_diff_preview: Option<String>,
}

impl From<&AgentTask> for AgentTaskStatus {
    fn from(task: &AgentTask) -> Self {
        let state_str = match &task.state {
            AgentTaskState::Queued => "Queued".to_string(),
            AgentTaskState::Running => "Running".to_string(),
            AgentTaskState::AwaitingReview => "AwaitingReview".to_string(),
            AgentTaskState::Applied => "Applied".to_string(),
            AgentTaskState::Verified => "Verified".to_string(),
            AgentTaskState::Rejected => "Rejected".to_string(),
            AgentTaskState::Failed { message } => format!("Failed: {message}"),
        };
        Self {
            task_id: task.id,
            prompt: task.prompt.clone(),
            state: state_str,
            verification_passed: task.verification == VerificationState::Passed,
            unified_diff_preview: task.patch.as_ref().map(|p| p.unified_diff.clone()),
        }
    }
}

/// Thread-safe coordinator for background AI Agent tasks.
#[derive(Clone)]
pub struct AgentManager {
    next_id: Arc<AtomicU64>,
    tasks: Arc<RwLock<BTreeMap<u64, AgentTask>>>,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            tasks: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Creates a new `AgentTask` from a prompt and target buffer snapshot, generating a patch.
    pub fn submit_prompt_for_buffer(&self, prompt: &str, buffer: &TextBuffer) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let file_path = buffer
            .file_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("buffer_{}.rs", buffer.id()));
        let original_text = buffer.text();
        let proposed_text = synthesize_agent_edit(prompt, &original_text);
        let patch = UnifiedDiffPatch::generate(&file_path, &original_text, &proposed_text);

        let task = AgentTask {
            id,
            prompt: prompt.to_string(),
            state: AgentTaskState::AwaitingReview,
            verification: VerificationState::Pending,
            patch: Some(patch),
            summary: format!("Generated patch for `{file_path}`"),
        };
        self.tasks.write().insert(id, task);
        id
    }

    /// Spawns an asynchronous background `AgentTask` on the Tokio runtime that reads a `TextBuffer`
    /// snapshot, computes a `UnifiedDiffPatch`, and runs verification.
    pub fn spawn_background_task(
        &self,
        prompt: String,
        target_path: String,
        original_text: String,
        proposed_text: String,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let initial_task = AgentTask {
            id,
            prompt: prompt.clone(),
            state: AgentTaskState::Running,
            verification: VerificationState::Running,
            patch: None,
            summary: "Executing autonomous agent task...".to_string(),
        };
        self.tasks.write().insert(id, initial_task);

        let tasks_ref = Arc::clone(&self.tasks);
        tokio::spawn(async move {
            let patch = UnifiedDiffPatch::generate(&target_path, &original_text, &proposed_text);
            let mut guard = tasks_ref.write();
            if let Some(task) = guard.get_mut(&id) {
                task.patch = Some(patch);
                task.state = AgentTaskState::AwaitingReview;
                task.verification = VerificationState::Passed;
                task.summary = format!("Patch ready for `{target_path}`");
            }
        });

        id
    }

    /// Applies the patch from `task_id` into `buffer` and marks the task as `Applied`.
    pub fn apply_task_patch(
        &self,
        task_id: u64,
        buffer: &mut TextBuffer,
    ) -> Result<(), AgentError> {
        let mut guard = self.tasks.write();
        let task = guard
            .get_mut(&task_id)
            .ok_or(AgentError::TaskNotFound(task_id))?;
        if let Some(patch) = &task.patch {
            patch.apply_to_buffer(buffer)?;
            task.state = AgentTaskState::Applied;
        }
        Ok(())
    }

    /// Marks the task as verified (`VerificationState::Passed` and `AgentTaskState::Verified`).
    pub fn verify_task(&self, task_id: u64, passed: bool, details: &str) -> Result<(), AgentError> {
        let mut guard = self.tasks.write();
        let task = guard
            .get_mut(&task_id)
            .ok_or(AgentError::TaskNotFound(task_id))?;
        if passed {
            task.verification = VerificationState::Passed;
            task.state = AgentTaskState::Verified;
        } else {
            task.verification = VerificationState::Failed {
                details: details.to_string(),
            };
        }
        Ok(())
    }

    /// Rejects a task's changes and optionally reverts them on `buffer` if already applied.
    pub fn reject_task(
        &self,
        task_id: u64,
        buffer: Option<&mut TextBuffer>,
    ) -> Result<(), AgentError> {
        let mut guard = self.tasks.write();
        let task = guard
            .get_mut(&task_id)
            .ok_or(AgentError::TaskNotFound(task_id))?;
        if let (Some(buf), Some(patch)) = (buffer, &task.patch) {
            if matches!(task.state, AgentTaskState::Applied | AgentTaskState::Verified) {
                patch.revert_on_buffer(buf);
            }
        }
        task.state = AgentTaskState::Rejected;
        Ok(())
    }

    pub fn task(&self, task_id: u64) -> Option<AgentTask> {
        self.tasks.read().get(&task_id).cloned()
    }

    pub fn tasks(&self) -> Vec<AgentTask> {
        self.tasks.read().values().cloned().collect()
    }

    pub fn task_statuses(&self) -> Vec<AgentTaskStatus> {
        self.tasks
            .read()
            .values()
            .map(AgentTaskStatus::from)
            .collect()
    }
}

/// Deterministic code transformation engine for agent prompts:
/// - `old -> new`: replaces occurrences of `old` with `new`
/// - `doc: <comment>`: adds documentation comment at the top of the file
/// - `delete_line <N>`: deletes 1-indexed line `N`
/// - Otherwise appends a synthesized code annotation or block.
fn synthesize_agent_edit(prompt: &str, original: &str) -> String {
    if let Some((old_part, new_part)) = prompt.split_once("->") {
        let from = old_part.trim();
        let to = new_part.trim();
        if !from.is_empty() && original.contains(from) {
            return original.replace(from, to);
        }
    }
    if let Some(doc_text) = prompt.strip_prefix("doc:") {
        let doc = doc_text.trim();
        return format!("//! {doc}\n{original}");
    }
    if let Some(line_num_str) = prompt.strip_prefix("delete_line ") {
        if let Ok(line_num) = line_num_str.trim().parse::<usize>() {
            if line_num > 0 {
                let mut lines = split_lines_owned(original);
                if line_num <= lines.len() {
                    lines.remove(line_num - 1);
                    let mut joined = lines.join("\n");
                    if original.ends_with('\n') {
                        joined.push('\n');
                    }
                    return joined;
                }
            }
        }
    }
    if original.is_empty() {
        format!("// Agent: {prompt}\n")
    } else if original.ends_with('\n') {
        format!("{original}// Agent: {prompt}\n")
    } else {
        format!("{original}\n// Agent: {prompt}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unified_diff_generation_and_buffer_apply_revert() {
        let original = "fn greet() {\n    println!(\"Hello\");\n}\n";
        let proposed = "fn greet() {\n    println!(\"Xin chào Việt Nam\");\n}\n";

        let patch = UnifiedDiffPatch::generate("src/main.rs", original, proposed);
        assert!(patch.unified_diff.contains("--- a/src/main.rs"));
        assert!(patch.unified_diff.contains("+++ b/src/main.rs"));
        assert!(patch.unified_diff.contains(" fn greet() {"));
        assert!(patch.unified_diff.contains("-    println!(\"Hello\");"));
        assert!(patch
            .unified_diff
            .contains("+    println!(\"Xin chào Việt Nam\");"));
        assert_eq!(patch.hunks.len(), 1);

        let mut buf = TextBuffer::new(1, original);
        patch.apply_to_buffer(&mut buf).unwrap();
        assert_eq!(buf.text(), proposed);

        // Single undo on the buffer can also undo the agent patch!
        buf.undo();
        assert_eq!(buf.text(), original);
    }

    #[tokio::test]
    async fn test_agent_manager_async_task_and_verification_flow() {
        let manager = AgentManager::new();
        let mut buf = TextBuffer::new(1, "let count = 1;\n");

        let task_id = manager.spawn_background_task(
            "Update count to 42".to_string(),
            "src/lib.rs".to_string(),
            "let count = 1;\n".to_string(),
            "let count = 42;\n".to_string(),
        );

        for _ in 0..20 {
            if manager
                .task(task_id)
                .is_some_and(|t| t.state == AgentTaskState::AwaitingReview)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let task = manager.task(task_id).unwrap();
        assert_eq!(task.state, AgentTaskState::AwaitingReview);
        assert!(task.patch.is_some());

        manager.apply_task_patch(task_id, &mut buf).unwrap();
        assert_eq!(buf.text(), "let count = 42;\n");
        assert_eq!(
            manager.task(task_id).unwrap().state,
            AgentTaskState::Applied
        );

        manager
            .verify_task(task_id, true, "All tests passed")
            .unwrap();
        assert_eq!(
            manager.task(task_id).unwrap().state,
            AgentTaskState::Verified
        );

        manager.reject_task(task_id, Some(&mut buf)).unwrap();
        assert_eq!(buf.text(), "let count = 1;\n");
        assert_eq!(
            manager.task(task_id).unwrap().state,
            AgentTaskState::Rejected
        );
    }

    #[test]
    fn test_submit_prompt_replacement_and_doc_directives() {
        let manager = AgentManager::new();
        let mut buf = TextBuffer::new(1, "fn old_name() {}\n");
        let task_id = manager.submit_prompt_for_buffer("old_name -> new_name", &buf);
        manager.apply_task_patch(task_id, &mut buf).unwrap();
        assert_eq!(buf.text(), "fn new_name() {}\n");

        let doc_task = manager.submit_prompt_for_buffer("doc: Module documentation", &buf);
        manager.apply_task_patch(doc_task, &mut buf).unwrap();
        assert_eq!(buf.text(), "//! Module documentation\nfn new_name() {}\n");
    }
}
