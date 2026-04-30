//! Integration coverage for `wt clean --format=json` (BRW-HFCTL0).
//!
//! The text path of `wt clean` shipped untested in BRW-HYOXK5; these tests
//! focus narrowly on the JSON contract relied on by `brw doctor` and the
//! brainwrap onboarding wizard. Schema changes here are user-visible API.

use crate::common::{TestRepo, repo_with_main_worktree};
use rstest::rstest;
use serde_json::Value;

fn run_clean_json(repo: &TestRepo, extra_args: &[&str]) -> (Value, std::process::Output) {
    let mut cmd = repo.wt_command();
    cmd.args(["clean", "--format=json"]);
    for a in extra_args {
        cmd.arg(a);
    }
    let output = cmd.output().expect("wt clean --format=json");
    assert!(
        output.status.success(),
        "wt clean failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout was not valid JSON: {e}\n{stdout}"));
    (value, output)
}

/// Schema check: top-level keys, summary fields, per-worktree fields all
/// present and well-typed. Same contract `brw doctor` will rely on.
#[rstest]
fn test_clean_json_schema_minimal_repo(mut repo_with_main_worktree: TestRepo) {
    let repo = &mut repo_with_main_worktree;
    repo.add_feature();

    let (json, _) = run_clean_json(repo, &["--dry-run"]);

    assert!(json.is_object(), "top-level must be object: {json}");
    assert!(json.get("worktrees").is_some(), "missing worktrees key");
    assert!(json.get("summary").is_some(), "missing summary key");

    let worktrees = json["worktrees"].as_array().expect("worktrees array");
    assert!(
        !worktrees.is_empty(),
        "expected at least the primary worktree"
    );
    for entry in worktrees {
        for key in ["path", "branch", "classification", "action", "reason"] {
            assert!(
                entry.get(key).is_some(),
                "worktree entry missing {key}: {entry}"
            );
        }
        // branch may be null (detached HEAD); the rest are strings.
        assert!(entry["path"].is_string(), "path must be string: {entry}");
        assert!(
            entry["branch"].is_string() || entry["branch"].is_null(),
            "branch must be string or null: {entry}"
        );
        assert!(
            ["clean", "skip"].contains(&entry["action"].as_str().unwrap_or("")),
            "action must be clean|skip: {entry}"
        );
    }

    let summary = &json["summary"];
    for key in ["total", "to_clean", "skipped", "by_classification"] {
        assert!(
            summary.get(key).is_some(),
            "summary missing {key}: {summary}"
        );
    }
    let total = summary["total"].as_u64().expect("total u64");
    let to_clean = summary["to_clean"].as_u64().expect("to_clean u64");
    let skipped = summary["skipped"].as_u64().expect("skipped u64");
    assert_eq!(
        total,
        to_clean + skipped,
        "total ({total}) must equal to_clean + skipped ({to_clean} + {skipped})"
    );
    assert_eq!(
        total as usize,
        worktrees.len(),
        "summary.total must match worktrees.len()"
    );

    let by_class = summary["by_classification"]
        .as_object()
        .expect("by_classification object");
    let class_total: u64 = by_class
        .values()
        .map(|v| v.as_u64().expect("class count"))
        .sum();
    assert_eq!(
        class_total, total,
        "by_classification counts must sum to total"
    );
}

/// The current worktree must always be classified `current` and skipped — JSON
/// callers rely on this to count "real" stale worktrees vs. the host worktree.
#[rstest]
fn test_clean_json_marks_current_worktree(mut repo_with_main_worktree: TestRepo) {
    let repo = &mut repo_with_main_worktree;
    let (json, _) = run_clean_json(repo, &["--dry-run"]);

    let entries = json["worktrees"].as_array().unwrap();
    let current_count = entries
        .iter()
        .filter(|e| e["classification"] == "current")
        .count();
    assert_eq!(current_count, 1, "exactly one entry should be `current`");
}

/// `--dry-run` does not delete anything; running the same command twice gives
/// the same plan.
#[rstest]
fn test_clean_json_dry_run_is_idempotent(mut repo_with_main_worktree: TestRepo) {
    let repo = &mut repo_with_main_worktree;
    repo.add_feature();

    let (a, _) = run_clean_json(repo, &["--dry-run"]);
    let (b, _) = run_clean_json(repo, &["--dry-run"]);
    assert_eq!(a, b, "dry-run output drifted between two invocations");
}
