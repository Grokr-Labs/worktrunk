//! `wt clean` — remove stale worktrees and merged branches.
//!
//! Fills the gap between "wt merge finished cleanly" and "something failed
//! mid-cycle and left a worktree behind that neither `wt remove` nor git
//! commands know how to triage in one step." Scans all worktrees in the
//! repository, classifies each, prints the plan, and — with confirmation —
//! removes the stale ones using the same `remove_worktree_with_cleanup` API
//! the `wt remove` path uses, so lifecycle hooks + trash-path staging stay
//! consistent.
//!
//! Safety:
//! - The primary worktree is never touched.
//! - The current worktree is never touched (can't remove self).
//! - Locked worktrees are skipped with a note unless `--force`.
//! - Worktrees with uncommitted changes are skipped unless `--force`.
//! - Branches with unpushed commits not reachable from the target are
//!   skipped unless `--force`.
//! - Dry-run by default when stdin is a TTY and `--yes` isn't passed; the
//!   user must explicitly confirm.

use anyhow::Context;
use worktrunk::config::UserConfig;
use worktrunk::git::{
    BranchDeletionMode, RemoveOptions, Repository, WorktreeInfo, remove_worktree_with_cleanup,
};
use worktrunk::styling::{eprintln, info_message};

use super::context::CommandEnv;

/// Classification of a single worktree for cleanup purposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Primary worktree; never cleaned.
    Primary,
    /// Current worktree (where the command was invoked); never cleaned.
    Current,
    /// Worktree is locked via `git worktree lock`; skipped unless `--force`.
    Locked { reason: String },
    /// Worktree directory is missing on disk; `git worktree prune` territory.
    /// Always safe to clean.
    Prunable,
    /// Working tree has uncommitted changes; skipped unless `--force`.
    Dirty,
    /// Branch has commits not reachable from the target branch (unpushed
    /// work); skipped unless `--force`.
    NotMerged { branch: String },
    /// Detached HEAD — no branch to safely reason about. Skipped unless
    /// `--force`.
    DetachedHead,
    /// Branch is fully merged into the target. Safe to clean.
    Merged { branch: String },
}

impl Classification {
    /// Whether the default (non-`--force`) invocation would clean this worktree.
    fn is_safe_to_clean(&self) -> bool {
        matches!(self, Self::Prunable | Self::Merged { .. })
    }

    /// Whether `--force` would clean this worktree (safe cases + risky cases).
    fn is_forceable(&self) -> bool {
        !matches!(self, Self::Primary | Self::Current)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Current => "current",
            Self::Locked { .. } => "locked",
            Self::Prunable => "prunable",
            Self::Dirty => "dirty",
            Self::NotMerged { .. } => "not-merged",
            Self::DetachedHead => "detached",
            Self::Merged { .. } => "merged",
        }
    }
}

/// Options for [`handle_clean`].
pub struct CleanOptions {
    pub target_branch: Option<String>,
    pub yes: bool,
    pub dry_run: bool,
    pub force: bool,
    /// Emit JSON instead of text. Suppresses the human-readable plan + status
    /// stream on stderr; the structured output goes to stdout for programmatic
    /// consumers (`brw doctor`, brainwrap onboarding wizard, future
    /// `wt clean --auto`).
    pub json: bool,
}

/// `wt clean` entry point.
pub fn handle_clean(opts: CleanOptions) -> anyhow::Result<()> {
    let config = UserConfig::load().context("Failed to load config")?;
    let env = CommandEnv::for_action(config)?;
    let repo = &env.repo;

    let target_branch = repo.require_target_branch(opts.target_branch.as_deref())?;
    let current_path = env.worktree_path.clone();
    let worktrees = repo.list_worktrees()?;

    let plan: Vec<_> = worktrees
        .iter()
        .map(|wt| {
            let classification = classify(repo, wt, &target_branch, &current_path);
            (wt, classification)
        })
        .collect();

    if !opts.json {
        print_plan(&plan, opts.force);
    }

    let to_clean: Vec<_> = plan
        .iter()
        .filter(|(_, c)| {
            if opts.force {
                c.is_forceable()
            } else {
                c.is_safe_to_clean()
            }
        })
        .collect();

    // JSON mode short-circuits the entire human stderr stream; we still
    // perform the cleanup (unless --dry-run or no --yes), but report through
    // the structured output instead.
    if opts.json {
        // Determine whether we'll actually run cleanup.
        let will_clean = !opts.dry_run && opts.yes && !to_clean.is_empty();
        let mut failures: Vec<(std::path::PathBuf, String)> = Vec::new();
        if will_clean {
            for (wt, classification) in &to_clean {
                if let Err(e) = clean_one(repo, wt, classification, &target_branch, opts.force) {
                    failures.push((wt.path.clone(), e.to_string()));
                }
            }
        }
        emit_json(&plan, opts.force, &failures);
        if !failures.is_empty() {
            anyhow::bail!("wt clean: {} worktree(s) failed to clean", failures.len());
        }
        return Ok(());
    }

    if to_clean.is_empty() {
        eprintln!("{}", info_message("Nothing to clean."));
        return Ok(());
    }

    if opts.dry_run {
        eprintln!(
            "{}",
            info_message(format!(
                "Dry run — would clean {} worktree(s). Re-run without --dry-run to apply.",
                to_clean.len()
            ))
        );
        return Ok(());
    }

    if !opts.yes {
        eprintln!(
            "{}",
            info_message(format!(
                "Would clean {} worktree(s). Re-run with --yes to confirm.",
                to_clean.len()
            ))
        );
        return Ok(());
    }

    let mut failures = 0usize;
    for (wt, classification) in to_clean {
        match clean_one(repo, wt, classification, &target_branch, opts.force) {
            Ok(()) => {
                eprintln!(
                    "{}",
                    info_message(format!(
                        "Cleaned {} ({})",
                        wt.path.display(),
                        classification.label()
                    ))
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!(
                    "{}",
                    info_message(format!(
                        "Failed to clean {} ({}): {e}",
                        wt.path.display(),
                        classification.label()
                    ))
                );
            }
        }
    }

    if failures > 0 {
        anyhow::bail!("wt clean: {failures} worktree(s) failed to clean");
    }
    Ok(())
}

/// Emit the plan + summary as JSON to stdout.
///
/// Schema (see BRW-HFCTL0):
/// ```json
/// {
///   "worktrees": [
///     { "path": "...", "branch": "feat/x" | null, "classification": "merged",
///       "action": "clean" | "skip", "reason": "..." }
///   ],
///   "summary": {
///     "total": N,
///     "to_clean": N,
///     "skipped": N,
///     "by_classification": { "merged": N, "not-merged": N, ... },
///     "failures": N      // present iff cleanup ran and any worktree failed
///   }
/// }
/// ```
fn emit_json(
    plan: &[(&WorktreeInfo, Classification)],
    force: bool,
    failures: &[(std::path::PathBuf, String)],
) {
    use serde_json::json;
    let failed_paths: std::collections::HashSet<_> =
        failures.iter().map(|(p, _)| p.clone()).collect();

    let mut by_class: std::collections::BTreeMap<&'static str, u64> = Default::default();
    let mut to_clean = 0u64;
    let mut skipped = 0u64;

    let worktrees: Vec<_> = plan
        .iter()
        .map(|(wt, c)| {
            *by_class.entry(c.label()).or_default() += 1;
            let action = action_for(c, force);
            if action == "clean" {
                to_clean += 1;
            } else {
                skipped += 1;
            }
            let mut entry = json!({
                "path": wt.path.to_string_lossy(),
                "branch": wt.branch,
                "classification": c.label(),
                "action": action,
                "reason": reason_for(c),
            });
            if failed_paths.contains(&wt.path) {
                let err = failures
                    .iter()
                    .find_map(|(p, e)| (p == &wt.path).then_some(e.as_str()))
                    .unwrap_or("");
                entry["failed"] = json!(true);
                entry["error"] = json!(err);
            }
            entry
        })
        .collect();

    let mut summary = json!({
        "total": plan.len() as u64,
        "to_clean": to_clean,
        "skipped": skipped,
        "by_classification": by_class,
    });
    if !failures.is_empty() {
        summary["failures"] = json!(failures.len() as u64);
    }

    let out = json!({ "worktrees": worktrees, "summary": summary });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}

fn action_for(c: &Classification, force: bool) -> &'static str {
    if force {
        if c.is_forceable() { "clean" } else { "skip" }
    } else if c.is_safe_to_clean() {
        "clean"
    } else {
        "skip"
    }
}

fn reason_for(c: &Classification) -> String {
    match c {
        Classification::Primary => "primary worktree".to_string(),
        Classification::Current => "current worktree".to_string(),
        Classification::Locked { reason } if reason.is_empty() => "locked".to_string(),
        Classification::Locked { reason } => format!("locked: {reason}"),
        Classification::Prunable => "directory missing on disk".to_string(),
        Classification::Dirty => "uncommitted changes".to_string(),
        Classification::NotMerged { branch } => {
            format!("branch {branch} has commits not on target")
        }
        Classification::DetachedHead => "detached HEAD".to_string(),
        Classification::Merged { branch } => format!("branch {branch} merged into target"),
    }
}

fn classify(
    repo: &Repository,
    wt: &WorktreeInfo,
    target_branch: &str,
    current_path: &std::path::Path,
) -> Classification {
    // Primary worktree identification: first entry in `git worktree list`
    // by convention. For cleanup purposes we also want to skip any
    // worktree at the repo root.
    if wt.path == current_path {
        return Classification::Current;
    }
    if let Ok(primary) = repo.repo_path()
        && wt.path == primary
    {
        return Classification::Primary;
    }

    if let Some(reason) = &wt.locked {
        return Classification::Locked {
            reason: reason.clone(),
        };
    }
    if wt.prunable.is_some() || !wt.path.exists() {
        return Classification::Prunable;
    }
    if wt.detached {
        return Classification::DetachedHead;
    }
    let Some(branch) = wt.branch.as_deref() else {
        return Classification::DetachedHead;
    };

    // Is the working tree dirty? `git status --porcelain` in the worktree.
    match repo
        .worktree_at(&wt.path)
        .run_command(&["status", "--porcelain"])
    {
        Ok(s) if !s.trim().is_empty() => return Classification::Dirty,
        _ => {}
    }

    // Is every commit on <branch> reachable from <target_branch>?
    // `merge-base --is-ancestor A B` returns 0 if A is an ancestor of B.
    let is_ancestor = repo
        .run_command_check(&["merge-base", "--is-ancestor", branch, target_branch])
        .unwrap_or(false);
    if is_ancestor {
        Classification::Merged {
            branch: branch.to_string(),
        }
    } else {
        Classification::NotMerged {
            branch: branch.to_string(),
        }
    }
}

fn print_plan(plan: &[(&WorktreeInfo, Classification)], force: bool) {
    eprintln!("{}", info_message("wt clean — plan:"));
    for (wt, c) in plan {
        let action = if force {
            if c.is_forceable() { "clean" } else { "skip" }
        } else if c.is_safe_to_clean() {
            "clean"
        } else {
            "skip"
        };
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        eprintln!(
            "  [{action}] {} — {} ({branch})",
            wt.path.display(),
            c.label()
        );
    }
}

fn clean_one(
    repo: &Repository,
    wt: &WorktreeInfo,
    classification: &Classification,
    target_branch: &str,
    force: bool,
) -> anyhow::Result<()> {
    // Prunable: git worktree prune will clear the metadata; no safe branch
    // deletion because the branch may still hold valuable unpushed state
    // elsewhere (the missing-directory is ambiguous).
    if matches!(classification, Classification::Prunable) {
        repo.run_command(&["worktree", "prune"])
            .context("git worktree prune failed")?;
        return Ok(());
    }

    let options = RemoveOptions {
        branch: wt.branch.clone(),
        deletion_mode: if force {
            BranchDeletionMode::ForceDelete
        } else {
            BranchDeletionMode::SafeDelete
        },
        target_branch: Some(target_branch.to_string()),
        force_worktree: force,
    };
    let _ = remove_worktree_with_cleanup(repo, &wt.path, options)?;
    Ok(())
}
