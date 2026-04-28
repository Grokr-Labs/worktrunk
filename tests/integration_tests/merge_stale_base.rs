//! BRW-6GRP8P regression coverage.
//!
//! `wt merge` (and `wt step squash`) anchor the squash on the upstream
//! remote-tracking ref, not the local target. These tests exercise the
//! divergence-aware behaviors:
//!
//! 1. A branch rebased onto an advanced `origin/main` produces a clean squash
//!    even when local `main` is stale.
//! 2. Local `<target>` that has diverged from `origin/<target>` causes the
//!    command to refuse with the structured error.
//! 3. With no remote tracking, the command falls back to local `<target>`.
//! 4. After a clean run, local `<target>` is fast-forwarded to match origin.
//! 5. `--no-fetch` keeps the squash anchor on the cached upstream ref without
//!    contacting the remote.

use crate::common::{TestRepo, repo};
use rstest::rstest;
use std::fs;
use std::path::Path;
use worktrunk::testing::canonicalize;

/// Expand the bare 'origin' remote with a sibling commit that local `main` does not pull.
///
/// Touches a file inside the bare remote workspace via a temporary clone, so the
/// caller's local repo only learns about it after a `git fetch`.
fn advance_origin_main(repo: &TestRepo, filename: &str, content: &str, message: &str) -> String {
    // `git remote get-url` yields whatever URL was registered, which may be
    // relative ("../origin.git"). Resolve it relative to the repo root so the
    // clone works from a tempdir.
    let raw = repo
        .git_output(&["remote", "get-url", "origin"])
        .trim()
        .to_string();
    let remote_path = match Path::new(&raw) {
        p if p.is_absolute() => p.to_path_buf(),
        rel => repo.root_path().join(rel),
    };
    let remote_path = canonicalize(&remote_path).expect("origin remote path canonicalize");
    let remote_url = remote_path.to_string_lossy().into_owned();
    let workspace = tempfile::TempDir::new().unwrap();
    let work_root = workspace.path();
    let worker = TempClone::clone_from(&remote_url, work_root);
    fs::write(worker.root.join(filename), content).unwrap();
    worker.run_git(&["add", filename]);
    worker.run_git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-m",
        message,
    ]);
    worker.run_git(&["push", "origin", "main"]);
    worker.run_git(&["rev-parse", "HEAD"]).trim().to_string()
}

struct TempClone {
    root: std::path::PathBuf,
}

impl TempClone {
    fn clone_from(remote_url: &str, work_root: &Path) -> Self {
        let dest = work_root.join("worker");
        let status = std::process::Command::new("git")
            .args(["clone", remote_url])
            .arg(&dest)
            .status()
            .expect("git clone");
        assert!(status.success());
        Self { root: dest }
    }

    fn run_git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

/// `wt step squash` ignores stale local main and anchors on `origin/main`.
///
/// Reproduces the BRW-6GRP8P scenario:
/// - Origin advances by one sibling commit.
/// - Local main does NOT pull, stays at the old SHA.
/// - Feature branch is rebased onto origin/main and adds one new commit.
/// - `wt step squash main` must produce a single squash whose tree differs from
///   origin/main only by the feature commit's file — the sibling file must NOT
///   appear in the squash diff against origin/main.
#[rstest]
fn test_squash_anchor_uses_origin_main_when_local_main_is_stale(mut repo: TestRepo) {
    repo.setup_remote("main");
    let stale_main_sha = repo.head_sha();
    let sibling_sha = advance_origin_main(
        &repo,
        "sibling.txt",
        "sibling content",
        "feat: sibling change",
    );

    let feature_wt = repo.add_worktree("feature");
    repo.commit_in_worktree(
        &feature_wt,
        "feature.txt",
        "feature content",
        "feat: add feature",
    );
    let feature_only_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&feature_wt)
        .output()
        .unwrap();
    assert!(feature_only_sha.status.success());

    repo.run_git(&["fetch", "origin", "main"]);
    repo.run_git_in(&feature_wt, &["fetch", "origin", "main"]);
    repo.run_git_in(&feature_wt, &["rebase", "origin/main"]);
    let post_rebase_head = repo
        .git_output(&["-C", feature_wt.to_str().unwrap(), "rev-parse", "HEAD"])
        .trim()
        .to_string();

    assert_eq!(
        repo.git_output(&["rev-parse", "refs/heads/main"]).trim(),
        stale_main_sha,
        "local main must remain stale before the squash"
    );

    let output = repo
        .wt_command()
        .args(["step", "squash", "main", "--yes", "--no-hooks"])
        .current_dir(&feature_wt)
        .output()
        .expect("wt step squash");
    assert!(
        output.status.success(),
        "wt step squash failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let diff_against_origin = repo
        .git_output(&[
            "-C",
            feature_wt.to_str().unwrap(),
            "diff",
            "--name-only",
            "refs/remotes/origin/main",
            "HEAD",
        ])
        .trim()
        .to_string();
    let changed: Vec<&str> = diff_against_origin.lines().collect();
    assert_eq!(
        changed,
        vec!["feature.txt"],
        "squash diff against origin/main must contain only feature.txt; sibling pollution would add sibling.txt"
    );

    let local_main_sha = repo
        .git_output(&["rev-parse", "refs/heads/main"])
        .trim()
        .to_string();
    assert_eq!(
        local_main_sha, sibling_sha,
        "local main should fast-forward to origin/main after a successful squash"
    );

    let _ = post_rebase_head;
}

/// Local main carrying unpushed commits must surface as `DivergedTarget`.
///
/// Worktrunk's contract: local `<target>` is always equal to or behind
/// `origin/<target>`. Anything else means the workflow was violated upstream of
/// `wt merge`. The squash must refuse, name the divergent SHAs, and offer the
/// two recovery commands. Silently absorbing the local-only commits would be
/// the original bug in the opposite direction.
#[rstest]
fn test_squash_refuses_when_local_main_diverges_from_origin(mut repo: TestRepo) {
    repo.setup_remote("main");
    fs::write(repo.root_path().join("local-only.txt"), "local-only").unwrap();
    repo.run_git(&["add", "local-only.txt"]);
    repo.run_git(&["commit", "-m", "feat: local-only main commit"]);
    let local_only_sha = repo.head_sha();

    let feature_wt =
        repo.add_worktree_with_commit("feature", "feature.txt", "feature", "feat: add feature");

    let output = repo
        .wt_command()
        .args(["step", "squash", "main", "--yes", "--no-hooks"])
        .current_dir(&feature_wt)
        .output()
        .expect("wt step squash");
    assert!(
        !output.status.success(),
        "wt step squash should refuse on diverged local main"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("diverged from"),
        "stderr should announce divergence; got: {stderr}"
    );
    assert!(
        stderr.contains(&local_only_sha[..7]),
        "stderr should list the divergent SHA {} ; got: {stderr}",
        &local_only_sha[..7]
    );
    assert!(
        stderr.contains("git push origin main"),
        "stderr should suggest the push recovery; got: {stderr}"
    );
    assert!(
        stderr.contains("git update-ref refs/heads/main"),
        "stderr should suggest the discard recovery; got: {stderr}"
    );
}

/// With no remote configured, `wt step squash` falls back to local `<target>`.
///
/// The reconciliation step is opt-in on the existence of a remote-tracking ref.
/// Pure-local repos must continue to work unchanged.
#[rstest]
fn test_squash_falls_back_to_local_target_without_remote(mut repo: TestRepo) {
    let feature_wt = repo.add_worktree("feature");
    repo.commit_in_worktree(&feature_wt, "f1.txt", "1", "feat: f1");
    repo.commit_in_worktree(&feature_wt, "f2.txt", "2", "feat: f2");

    let output = repo
        .wt_command()
        .args(["step", "squash", "main", "--yes", "--no-hooks"])
        .current_dir(&feature_wt)
        .output()
        .expect("wt step squash");
    assert!(
        output.status.success(),
        "squash should succeed on a local-only repo: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let commit_count = repo
        .git_output(&[
            "-C",
            feature_wt.to_str().unwrap(),
            "rev-list",
            "--count",
            "main..HEAD",
        ])
        .trim()
        .to_string();
    assert_eq!(commit_count, "1", "post-squash should leave one commit");
}

/// `--no-fetch` keeps the squash anchored on the cached `origin/<target>`.
///
/// We can't easily mock the fetch call; instead we point `origin` at a
/// non-existent path before the squash. With `--no-fetch`, the squash runs
/// against the cached `refs/remotes/origin/main` and succeeds. Without
/// `--no-fetch`, the same setup would fail at the fetch step.
#[rstest]
fn test_squash_no_fetch_uses_cached_origin(mut repo: TestRepo) {
    repo.setup_remote("main");
    let cached_origin_sha = repo
        .git_output(&["rev-parse", "refs/remotes/origin/main"])
        .trim()
        .to_string();

    let feature_wt = repo.add_worktree_with_commit(
        "feature",
        "feature.txt",
        "feature content",
        "feat: add feature",
    );

    repo.run_git(&["remote", "set-url", "origin", "/nonexistent/path/to/origin"]);

    let no_fetch = repo
        .wt_command()
        .args([
            "step",
            "squash",
            "main",
            "--yes",
            "--no-hooks",
            "--no-fetch",
        ])
        .current_dir(&feature_wt)
        .output()
        .expect("wt step squash --no-fetch");
    assert!(
        no_fetch.status.success(),
        "--no-fetch should succeed against a broken origin URL: stdout={}, stderr={}",
        String::from_utf8_lossy(&no_fetch.stdout),
        String::from_utf8_lossy(&no_fetch.stderr)
    );

    let post_squash_origin = repo
        .git_output(&["rev-parse", "refs/remotes/origin/main"])
        .trim()
        .to_string();
    assert_eq!(
        post_squash_origin, cached_origin_sha,
        "--no-fetch must not contact the remote"
    );
}
