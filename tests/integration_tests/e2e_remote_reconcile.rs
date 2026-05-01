//! End-to-end coverage for `reconcile_and_push` against a real GitHub sandbox
//! repo (BRW-J23HNY).
//!
//! Each test creates a throwaway `JonathanWorks/wt-e2e-<random>` repo via
//! [`SandboxRepo`], drives the appropriate pre-state, runs `wt merge main
//! --yes`, then asserts on the outcome (printed to stderr by `merge.rs`) and
//! the resulting GitHub state via `gh`.
//!
//! All tests are `#[ignore]`-gated. Opt in with:
//! ```bash
//! cargo test -- --ignored e2e_remote_reconcile
//! ```
//! and ensure `gh auth status` is logged in. The fixture deletes its repo on
//! `Drop` (best-effort).

use crate::common::sandbox_repo::{SandboxRepo, require_gh_auth};
use crate::common::{wt_bin, wt_command};
use std::process::Command;

/// Per-test temp user config that opts the sandbox project into the v0.38
/// reconciliation path (`push_to_origin = true`) and selects a divergence
/// strategy. Held by the test for the lifetime of `wt merge` so the file
/// pointed to by `WORKTRUNK_CONFIG_PATH` doesn't disappear mid-invocation.
struct E2eEnv {
    config_path: tempfile::NamedTempFile,
}

impl E2eEnv {
    fn new(sandbox: &SandboxRepo, on_diverged: &str) -> Self {
        let project_id = format!("github.com/{}", sandbox.full_name);
        let body = format!(
            r#"[projects."{project_id}".merge]
push_to_origin = true
on_diverged_remote = "{on_diverged}"
auto_open_pr_if_missing = true
draft = false
"#
        );
        let mut file = tempfile::NamedTempFile::new().expect("create temp config");
        std::io::Write::write_all(&mut file, body.as_bytes()).expect("write config");
        Self { config_path: file }
    }
}

fn make_feature_worktree(sandbox: &SandboxRepo, branch: &str) -> std::path::PathBuf {
    // Use plain git worktree add since `wt switch --create` would touch
    // post-start hooks that aren't relevant here. Worktree path mirrors what
    // `wt` would produce (`.worktrees/<branch>`).
    let wt_path = sandbox.clone_path.join(".worktrees").join(branch);
    sandbox.git(&[
        "worktree",
        "add",
        "-b",
        branch,
        wt_path.to_str().unwrap(),
        "main",
    ]);
    // Identity inside the worktree (worktrees inherit local config but
    // explicitness avoids surprises if global identity isn't set).
    Command::new("git")
        .args(["config", "user.email", "wt-e2e@example.com"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "wt-e2e"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    wt_path
}

fn commit_in(path: &std::path::Path, file: &str, content: &str, message: &str) {
    std::fs::write(path.join(file), content).expect("write file");
    Command::new("git")
        .args(["add", file])
        .current_dir(path)
        .status()
        .expect("git add");
    Command::new("git")
        .args(["commit", "--quiet", "-m", message])
        .current_dir(path)
        .status()
        .expect("git commit");
}

fn run_wt_merge(sandbox: &SandboxRepo, env: &E2eEnv, wt_path: &std::path::Path) -> (String, i32) {
    let output = wt_command()
        .current_dir(wt_path)
        .env("WORKTRUNK_CONFIG_PATH", env.config_path.path())
        .args(["merge", "main", "--yes", "--no-hooks"])
        .output()
        .expect("wt merge");
    let _ = sandbox; // keep sandbox alive for the duration of the call
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let combined = format!("{stdout}\n{stderr}");
    (combined, output.status.code().unwrap_or(-1))
}

fn assert_outcome_contains(combined: &str, needle: &str) {
    assert!(
        combined.contains(needle),
        "expected `{needle}` in wt merge output; got:\n{combined}"
    );
}

/// BRW-J59NF4: after `wt merge` squash-finalizes via GitHub, the main
/// worktree's index + working tree must match HEAD. `git update-ref` alone
/// only moves the branch pointer; without an explicit resync the merged
/// files show up as "modified" in `git status` and any tooling that reads
/// the working tree (e.g. `cargo install --path .`) silently uses stale code.
fn assert_main_worktree_clean_after_merge(sandbox: &SandboxRepo) {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&sandbox.clone_path)
        .output()
        .expect("git status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let dirty: Vec<_> = stdout
        .lines()
        // The .worktrees/ dir is intentionally untracked (per repo .gitignore
        // in real fleets); accept any "??" entries pointing at it. All other
        // status lines indicate the BRW-J59NF4 stale-state bug.
        .filter(|l| !l.is_empty() && !l.starts_with("?? .worktrees/"))
        .collect();
    assert!(
        dirty.is_empty(),
        "main worktree should be clean after squash-finalize (BRW-J59NF4); \
         got:\n{}",
        dirty.join("\n"),
    );
}

// ============================================================================
// 1. Absent — fresh feature branch, never pushed → FirstPush
// ============================================================================
#[test]
#[ignore = "requires gh auth + creates a real GitHub repo; run via `cargo test -- --ignored`"]
fn e2e_remote_reconcile_absent_first_push() {
    require_gh_auth();
    let _ = wt_bin(); // confirm the binary exists before paying repo-creation cost
    let sandbox = SandboxRepo::new();
    let env = E2eEnv::new(&sandbox, "remote-squash");

    let wt_path = make_feature_worktree(&sandbox, "feat/absent");
    commit_in(&wt_path, "feature.txt", "feature\n", "feat: add feature");

    let (out, code) = run_wt_merge(&sandbox, &env, &wt_path);
    assert_eq!(code, 0, "wt merge expected to succeed; got:\n{out}");
    assert_outcome_contains(&out, "FirstPush");
    assert_main_worktree_clean_after_merge(&sandbox);

    // Feature branch deleted on origin (gh api returns 404 once it's gone).
    let raw = Command::new("gh")
        .args([
            "api",
            &format!("repos/{}/git/refs/heads/feat/absent", sandbox.full_name),
        ])
        .output()
        .expect("gh api spawn");
    assert!(
        !raw.status.success(),
        "feature branch should have been deleted on origin (gh api stderr: {})",
        String::from_utf8_lossy(&raw.stderr).trim()
    );
}

// ============================================================================
// 2. InSync — pushed, local matches remote → AlreadyPushed
// ============================================================================
#[test]
#[ignore = "requires gh auth + creates a real GitHub repo; run via `cargo test -- --ignored`"]
fn e2e_remote_reconcile_in_sync_already_pushed() {
    require_gh_auth();
    let sandbox = SandboxRepo::new();
    let env = E2eEnv::new(&sandbox, "remote-squash");

    let wt_path = make_feature_worktree(&sandbox, "feat/insync");
    commit_in(&wt_path, "feature.txt", "feature\n", "feat: add feature");

    // Push the feature branch ourselves so wt observes RemoteState::InSync.
    Command::new("git")
        .args(["push", "--quiet", "-u", "origin", "feat/insync"])
        .current_dir(&wt_path)
        .status()
        .unwrap();

    let (out, code) = run_wt_merge(&sandbox, &env, &wt_path);
    assert_eq!(code, 0, "wt merge expected to succeed; got:\n{out}");
    assert_outcome_contains(&out, "AlreadyPushed");
    assert_main_worktree_clean_after_merge(&sandbox);
}

// ============================================================================
// 3. Ahead — remote received commits while we worked → RebasedAndPushed
// ============================================================================
#[test]
#[ignore = "requires gh auth + creates a real GitHub repo; run via `cargo test -- --ignored`"]
fn e2e_remote_reconcile_ahead_rebased_and_pushed() {
    require_gh_auth();
    let sandbox = SandboxRepo::new();
    let env = E2eEnv::new(&sandbox, "remote-squash");

    let wt_path = make_feature_worktree(&sandbox, "feat/ahead");
    commit_in(&wt_path, "feature.txt", "feature v1\n", "feat: v1");
    Command::new("git")
        .args(["push", "--quiet", "-u", "origin", "feat/ahead"])
        .current_dir(&wt_path)
        .status()
        .unwrap();

    // Simulate "someone pushed to origin" by pushing a second commit from a
    // sibling clone path, then resetting our local copy back so we look behind.
    let sibling = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["clone", "--quiet", &sandbox.clone_url])
        .arg(sibling.path().join("clone"))
        .status()
        .unwrap();
    let sibling_clone = sibling.path().join("clone");
    Command::new("git")
        .args(["config", "user.email", "wt-e2e@example.com"])
        .current_dir(&sibling_clone)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "wt-e2e"])
        .current_dir(&sibling_clone)
        .status()
        .unwrap();
    Command::new("git")
        .args(["checkout", "-q", "feat/ahead"])
        .current_dir(&sibling_clone)
        .status()
        .unwrap();
    commit_in(&sibling_clone, "sibling.txt", "sibling\n", "feat: sibling");
    Command::new("git")
        .args(["push", "--quiet", "origin", "feat/ahead"])
        .current_dir(&sibling_clone)
        .status()
        .unwrap();

    // Local now needs the sibling commit; wt merge should rebase + push.
    let (out, code) = run_wt_merge(&sandbox, &env, &wt_path);
    assert_eq!(code, 0, "wt merge expected to succeed; got:\n{out}");
    assert_outcome_contains(&out, "RebasedAndPushed");
    assert_main_worktree_clean_after_merge(&sandbox);
}

// ============================================================================
// 4. Diverges + RemoteSquash — local squash diverges from remote history → RemoteSquashed
// ============================================================================
#[test]
#[ignore = "requires gh auth + creates a real GitHub repo; run via `cargo test -- --ignored`"]
fn e2e_remote_reconcile_diverges_remote_squash() {
    require_gh_auth();
    let sandbox = SandboxRepo::new();
    let env = E2eEnv::new(&sandbox, "remote-squash");

    let wt_path = make_feature_worktree(&sandbox, "feat/divergesquash");
    // Two commits pushed to origin first…
    commit_in(&wt_path, "a.txt", "a\n", "feat: a");
    commit_in(&wt_path, "b.txt", "b\n", "feat: b");
    Command::new("git")
        .args(["push", "--quiet", "-u", "origin", "feat/divergesquash"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    // …then locally squash to one commit. Origin now has 2 commits, local has 1.
    Command::new("git")
        .args(["reset", "--soft", "HEAD~2"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "--quiet", "-m", "feat: squashed a+b"])
        .current_dir(&wt_path)
        .status()
        .unwrap();

    let (out, code) = run_wt_merge(&sandbox, &env, &wt_path);
    assert_eq!(code, 0, "wt merge expected to succeed; got:\n{out}");
    assert_outcome_contains(&out, "RemoteSquashed");
    assert_main_worktree_clean_after_merge(&sandbox);
}

// ============================================================================
// 5. Diverges + Restack — local squash diverges → Restacked + new -v2 PR
// ============================================================================
#[test]
#[ignore = "requires gh auth + creates a real GitHub repo; run via `cargo test -- --ignored`"]
fn e2e_remote_reconcile_diverges_restack() {
    require_gh_auth();
    let sandbox = SandboxRepo::new();
    let env = E2eEnv::new(&sandbox, "restack");

    let wt_path = make_feature_worktree(&sandbox, "feat/divergerestack");
    commit_in(&wt_path, "a.txt", "a\n", "feat: a");
    commit_in(&wt_path, "b.txt", "b\n", "feat: b");
    Command::new("git")
        .args(["push", "--quiet", "-u", "origin", "feat/divergerestack"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    // Open the original PR explicitly so restack has one to close.
    Command::new("gh")
        .args([
            "pr",
            "create",
            "--base",
            "main",
            "--head",
            "feat/divergerestack",
            "--title",
            "feat: divergerestack",
            "--body",
            "original PR for restack test",
        ])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    // Local squash.
    Command::new("git")
        .args(["reset", "--soft", "HEAD~2"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "--quiet", "-m", "feat: squashed restack"])
        .current_dir(&wt_path)
        .status()
        .unwrap();

    let (out, code) = run_wt_merge(&sandbox, &env, &wt_path);
    assert_eq!(code, 0, "wt merge expected to succeed; got:\n{out}");
    assert_outcome_contains(&out, "Restacked");
    assert_outcome_contains(&out, "v2");
    assert_main_worktree_clean_after_merge(&sandbox);
}

// ============================================================================
// 6. Diverges + Abort — fail with a clear error, no side effects
// ============================================================================
#[test]
#[ignore = "requires gh auth + creates a real GitHub repo; run via `cargo test -- --ignored`"]
fn e2e_remote_reconcile_diverges_abort() {
    require_gh_auth();
    let sandbox = SandboxRepo::new();
    let env = E2eEnv::new(&sandbox, "abort");

    let wt_path = make_feature_worktree(&sandbox, "feat/divergeabort");
    commit_in(&wt_path, "a.txt", "a\n", "feat: a");
    commit_in(&wt_path, "b.txt", "b\n", "feat: b");
    Command::new("git")
        .args(["push", "--quiet", "-u", "origin", "feat/divergeabort"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["reset", "--soft", "HEAD~2"])
        .current_dir(&wt_path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "--quiet", "-m", "feat: squashed abort"])
        .current_dir(&wt_path)
        .status()
        .unwrap();

    let (out, code) = run_wt_merge(&sandbox, &env, &wt_path);
    assert_ne!(code, 0, "wt merge expected to fail with abort; got:\n{out}");
    // The error message should mention recovery commands.
    assert_outcome_contains(&out, "remote-squash");
    assert_outcome_contains(&out, "restack");
}
