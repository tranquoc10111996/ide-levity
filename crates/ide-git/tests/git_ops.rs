use ide_git::{ChangeKind, DiffMarkerKind, FileChangeKind, GitError, GitRepository};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .expect("git command failed to spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_temp_repo() -> (TempDir, GitRepository) {
    let tmp = TempDir::new().unwrap();
    git_cmd(tmp.path(), &["init", "-b", "main"]);
    git_cmd(tmp.path(), &["config", "user.name", "Levity Bot"]);
    git_cmd(tmp.path(), &["config", "user.email", "bot@levity.dev"]);
    let repo = GitRepository::open(tmp.path()).unwrap();
    (tmp, repo)
}

#[test]
fn stage_commit_unstage_discard_and_git_fsck() {
    let (tmp, repo) = init_temp_repo();
    assert_eq!(repo.head_commit_summary().unwrap(), None);

    fs::write(tmp.path().join(".gitignore"), "ignored.log\n").unwrap();
    fs::write(tmp.path().join("ignored.log"), "secret\n").unwrap();
    fs::write(tmp.path().join("hello.txt"), "line 1\nline 2\n").unwrap();

    // Untracked before staging; ignored file is excluded.
    let status = repo.status_detailed().unwrap();
    assert_eq!(status.len(), 2);
    assert!(status.iter().any(|s| s.path == "hello.txt"
        && s.worktree_status == Some(ChangeKind::Untracked)
        && s.index_status.is_none()));

    // Stage all and verify staged status.
    repo.stage_all().unwrap();
    let status = repo.status_detailed().unwrap();
    assert!(status.iter().any(|s| s.path == "hello.txt"
        && s.index_status == Some(ChangeKind::Added)
        && s.worktree_status.is_none()));

    // Commit and verify with real git CLI + git fsck.
    let commit_id = repo.commit("Initial commit\n", None, None).unwrap();
    assert_eq!(commit_id.len(), 40);
    assert!(git_cmd(tmp.path(), &["status", "--porcelain"]).trim().is_empty());
    git_cmd(tmp.path(), &["fsck", "--full"]);

    let summary = repo.head_commit_summary().unwrap().unwrap();
    assert_eq!(summary.id, commit_id);
    assert_eq!(summary.summary, "Initial commit");
    assert_eq!(summary.author_name, "Levity Bot");

    // Modify hello.txt and check unstaged + staged unified diffs.
    fs::write(
        tmp.path().join("hello.txt"),
        "line 1\nline 2 modified\nline 3\n",
    )
    .unwrap();
    let unstaged_diff = repo.file_diff("hello.txt", false).unwrap();
    assert!(unstaged_diff.contains("-line 2"));
    assert!(unstaged_diff.contains("+line 2 modified"));
    assert!(repo.file_diff("hello.txt", true).unwrap().is_empty());

    let markers = repo
        .diff_buffer_lines("hello.txt", "line 1\nline 2 modified\nline 3\n")
        .unwrap();
    assert_eq!(markers.len(), 2);
    assert_eq!(markers[0].status, DiffMarkerKind::Modified);

    // Stage and then unstage back to HEAD.
    repo.stage(&["hello.txt"]).unwrap();
    assert!(!repo.file_diff("hello.txt", true).unwrap().is_empty());
    repo.unstage(&["hello.txt"]).unwrap();
    let status = repo.status_detailed().unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].index_status, None);
    assert_eq!(status[0].worktree_status, Some(ChangeKind::Modified));

    // Discard worktree changes restores hello.txt from index.
    repo.discard_worktree_changes(&["hello.txt"]).unwrap();
    assert_eq!(
        fs::read_to_string(tmp.path().join("hello.txt")).unwrap(),
        "line 1\nline 2\n"
    );
    assert!(repo.status_detailed().unwrap().is_empty());
}

#[test]
fn branches_checkout_and_delete_untracked() {
    let (tmp, repo) = init_temp_repo();
    fs::write(tmp.path().join("a.txt"), "base\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("base commit", None, None).unwrap();

    repo.create_branch("feature").unwrap();
    assert!(matches!(
        repo.create_branch("feature"),
        Err(GitError::BranchExists(_))
    ));

    repo.checkout_branch("feature").unwrap();
    assert_eq!(repo.current_branch().unwrap(), "feature");

    fs::write(tmp.path().join("b.txt"), "on feature\n").unwrap();
    repo.stage_all().unwrap();
    repo.commit("feature commit", None, None).unwrap();
    git_cmd(tmp.path(), &["fsck", "--full"]);

    let branches = repo.branches().unwrap();
    assert_eq!(branches.len(), 2);
    assert!(branches.iter().any(|b| b.name == "feature" && b.is_current));

    // Dirty file that differs between feature and main blocks checkout.
    fs::write(tmp.path().join("b.txt"), "dirty local edit\n").unwrap();
    assert!(matches!(
        repo.checkout_branch("main"),
        Err(GitError::WouldOverwrite(_))
    ));
    repo.discard_worktree_changes(&["b.txt"]).unwrap();

    // Checkout main removes b.txt and switches HEAD.
    repo.checkout_branch("main").unwrap();
    assert_eq!(repo.current_branch().unwrap(), "main");
    assert!(!tmp.path().join("b.txt").exists());
    assert!(git_cmd(tmp.path(), &["status", "--porcelain"]).trim().is_empty());

    // Untracked cleanup.
    fs::create_dir_all(tmp.path().join("sub/dir")).unwrap();
    fs::write(tmp.path().join("sub/dir/temp.txt"), "scratch").unwrap();
    assert_eq!(
        repo.changed_files().unwrap(),
        vec![ide_git::GitFileStatus {
            path: "sub/dir/temp.txt".into(),
            kind: FileChangeKind::Added
        }]
    );
    let deleted = repo.delete_untracked(&["sub"]).unwrap();
    assert_eq!(deleted, vec!["sub/dir/temp.txt".to_string()]);
    assert!(!tmp.path().join("sub").exists());
}
