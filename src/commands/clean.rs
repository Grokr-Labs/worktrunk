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

    print_plan(&plan, opts.force);

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
