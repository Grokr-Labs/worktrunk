//! Remote-aware push for `wt merge`.
//!
//! Normal `git push` assumes the remote is either absent or a fast-forwardable
//! subset of the local branch. In agent-only workflows that assumption
//! frequently fails: feature branches are pushed to origin with intermediate
//! commits for CI visibility, PR reviews, and multi-session handoffs, and
//! then `wt merge` locally squashes them. The result is a non-fast-forward
//! push which plain `git push` cannot resolve without `--force`, and
//! force-push is disallowed by convention in governed repos.
//!
//! This module models the four possible remote-state outcomes and dispatches
//! to a configured reconciliation strategy. See
//! [`super::super::super::config::user::sections::RemoteDivergenceStrategy`]
//! for the user-facing explanation.
//!
//! ## Module placement
//!
//! Lives next to `push.rs` because it is the policy layer around the actual
//! `git push` — a push that knows about GitHub PRs and can fall back to
//! server-side squash or branch restacking instead of force-pushing.

use anyhow::{Context, anyhow};
use worktrunk::config::RemoteDivergenceStrategy;
use worktrunk::git::Repository;
use worktrunk::shell_exec::Cmd;

/// Snapshot of how local HEAD relates to `origin/<branch>`.
///
/// Produced once per reconciliation run by [`classify_remote_state`]; dispatch
/// is a pure match on the variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteState {
    /// `origin/<branch>` does not exist. First push; no reconciliation needed.
    Absent,

    /// `origin/<branch>` points at the same commit as local HEAD. No-op.
    InSync,

    /// Local and remote share a base but have diverged: remote has `behind`
    /// commits not in local, local has `ahead` commits not in remote. When
    /// `behind == 0` the push is a fast-forward (not reached here — the
    /// caller treats that as a normal push). When `behind > 0` this is the
    /// collision that needs reconciliation.
    Diverges { behind: u32, ahead: u32 },

    /// Remote is strictly ahead of local (someone pushed while we were
    /// working). Reconcile by rebasing local onto remote, then retry.
    Ahead,
}

/// Classify the remote state of `branch` relative to local HEAD.
///
/// Fetches `origin/<branch>` first so the tracking ref is fresh. Returns
/// [`RemoteState::Absent`] if the remote branch does not exist.
pub fn classify_remote_state(repo: &Repository, branch: &str) -> anyhow::Result<RemoteState> {
    // ls-remote answers "does origin have this branch?" without touching local refs.
    let ls_remote = repo
        .run_command(&["ls-remote", "--heads", "origin", branch])
        .context("failed to query origin for branch state")?;
    if ls_remote.trim().is_empty() {
        return Ok(RemoteState::Absent);
    }

    // Fetch so origin/<branch> is current before comparing.
    repo.run_command(&["fetch", "origin", branch])
        .context("failed to fetch origin/<branch>")?;

    let local_head = repo.run_command(&["rev-parse", "HEAD"])?.trim().to_string();
    let remote_ref = format!("origin/{branch}");
    let remote_head = repo
        .run_command(&["rev-parse", &remote_ref])?
        .trim()
        .to_string();

    if local_head == remote_head {
        return Ok(RemoteState::InSync);
    }

    // `rev-list --count A..B` counts commits reachable from B but not from A.
    let behind = count_commits(repo, &format!("HEAD..{remote_ref}"))?;
    let ahead = count_commits(repo, &format!("{remote_ref}..HEAD"))?;

    if ahead == 0 && behind > 0 {
        Ok(RemoteState::Ahead)
    } else {
        Ok(RemoteState::Diverges { behind, ahead })
    }
}

fn count_commits(repo: &Repository, range: &str) -> anyhow::Result<u32> {
    let out = repo.run_command(&["rev-list", "--count", range])?;
    out.trim()
        .parse::<u32>()
        .with_context(|| format!("failed to parse commit count for range {range}"))
}

/// Outcome of [`reconcile_and_push`] for the caller to log / report.
///
/// Every variant that includes a `pr_number` means the GitHub side has already
/// squash-merged the feature branch into the target — the local-merge phase in
/// the caller should be skipped and the post-merge sync hook will pull the new
/// target commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// Remote branch didn't exist; pushed it and ran the GitHub squash-merge.
    FirstPush { pr_number: u32 },

    /// Remote already matched local; no push needed, ran GitHub squash-merge.
    AlreadyPushed { pr_number: u32 },

    /// Remote was ahead; rebased local onto remote, pushed, ran GitHub
    /// squash-merge.
    RebasedAndPushed { pr_number: u32 },

    /// Diverged remote + `RemoteSquash` strategy: delegated collapse to
    /// GitHub via `gh pr merge --squash --delete-branch`.
    RemoteSquashed { pr_number: u32 },

    /// Diverged remote + `Restack` strategy: created `<branch>-vN`, closed the
    /// old PR as superseded, opened a new PR, squash-merged the new PR.
    Restacked {
        new_branch: String,
        closed_pr: Option<u32>,
        new_pr: u32,
    },
}

/// Reconcile `origin/<branch>` with local HEAD and push.
///
/// * `repo` — the repository to operate on.
/// * `branch` — the feature branch being merged (local current branch).
/// * `target_branch` — the merge target (usually `main`); needed to open a
///   replacement PR when restacking.
/// * `strategy` — what to do when the remote has diverged.
/// * `auto_open_pr_if_missing` — whether to auto-create a draft PR when the
///   remote-squash path needs one that doesn't exist.
pub fn reconcile_and_push(
    repo: &Repository,
    branch: &str,
    target_branch: &str,
    strategy: RemoteDivergenceStrategy,
    auto_open_pr_if_missing: bool,
) -> anyhow::Result<ReconcileOutcome> {
    let state = classify_remote_state(repo, branch)?;

    // Handle the diverged paths up front — Restack and Abort don't share the
    // Absent/InSync/Ahead terminal (they take their own code path).
    if let RemoteState::Diverges { .. } = state {
        match strategy {
            RemoteDivergenceStrategy::Restack => {
                return do_restack(repo, branch, target_branch, auto_open_pr_if_missing);
            }
            RemoteDivergenceStrategy::Abort => {
                return Err(anyhow!(abort_message(branch, target_branch)));
            }
            RemoteDivergenceStrategy::RemoteSquash => {
                // The diverged-but-pushable content IS the open PR's diff.
                // Skip push — gh will squash the PR's existing commits
                // server-side. Local squash would be force-push territory.
            }
        }
    }

    // Push the feature branch so the PR references the final (local) squash
    // commit. RemoteSquash on a diverged remote is the exception handled above.
    match &state {
        RemoteState::Absent => {
            repo.run_command(&["push", "-u", "origin", branch])
                .context("initial push to origin failed")?;
        }
        RemoteState::InSync => { /* remote already matches local */ }
        RemoteState::Ahead => {
            repo.run_command(&["rebase", &format!("origin/{branch}")])
                .context("rebase onto origin/<branch> failed")?;
            repo.run_command(&["push", "origin", branch])
                .context("push after rebase failed")?;
        }
        RemoteState::Diverges { .. } => { /* RemoteSquash: gh owns the collapse */ }
    }

    // Single terminal step shared by every "push-then-merge" outcome. This is
    // the fix for the P0: previously only Diverges+RemoteSquash reached this,
    // leaving Absent/InSync/Ahead with a pushed branch but no PR + no merge.
    let pr_number = finalize_via_github(repo, branch, target_branch, auto_open_pr_if_missing)?;

    Ok(match state {
        RemoteState::Absent => ReconcileOutcome::FirstPush { pr_number },
        RemoteState::InSync => ReconcileOutcome::AlreadyPushed { pr_number },
        RemoteState::Ahead => ReconcileOutcome::RebasedAndPushed { pr_number },
        RemoteState::Diverges { .. } => ReconcileOutcome::RemoteSquashed { pr_number },
    })
}

/// Open (or find) a PR for `branch` targeting `target_branch`, squash-merge it
/// via GitHub, delete the remote feature branch, then fetch the new
/// `target_branch` tip locally. Shared terminal for every "the feature is now
/// on origin, land it" outcome.
fn finalize_via_github(
    repo: &Repository,
    branch: &str,
    target_branch: &str,
    auto_open_pr_if_missing: bool,
) -> anyhow::Result<u32> {
    let pr_number = match find_open_pr(repo, branch)? {
        Some(n) => n,
        None if auto_open_pr_if_missing => open_draft_pr(repo, branch, target_branch)?,
        None => {
            return Err(anyhow!(
                "wt merge expected an open PR for branch '{branch}' targeting '{target_branch}', \
and `auto_open_pr_if_missing` is disabled. Either open one manually \
(`gh pr create --draft --head {branch} --base {target_branch}`) or set \
`[merge] auto_open_pr_if_missing = true` in wt config."
            ));
        }
    };

    gh(
        repo,
        &[
            "pr",
            "merge",
            &pr_number.to_string(),
            "--squash",
            "--delete-branch",
        ],
    )
    .context("gh pr merge --squash failed")?;

    // Sync local `target_branch` to the new commit on origin so the caller's
    // post-merge hooks (pull / deploy) see the new state.
    repo.run_command(&["fetch", "origin", target_branch])?;
    Ok(pr_number)
}

/// Canonical pwm-os recovery pattern: create a fresh `<branch>-vN` from the
/// local squash, push it, close the old PR as superseded, open a new PR, and
/// finalize the cycle by squash-merging the new PR. The `_auto_open_pr_if_missing`
/// argument is accepted for signature parity with the terminal step; restack
/// always opens its replacement PR explicitly.
fn do_restack(
    repo: &Repository,
    branch: &str,
    target_branch: &str,
    _auto_open_pr_if_missing: bool,
) -> anyhow::Result<ReconcileOutcome> {
    let new_branch = next_vn_name(repo, branch)?;

    repo.run_command(&["branch", &new_branch, "HEAD"])
        .context("failed to create -vN branch")?;
    repo.run_command(&["push", "-u", "origin", &new_branch])
        .context("failed to push -vN branch")?;

    let closed_pr = find_open_pr(repo, branch)?;
    if let Some(old) = closed_pr {
        let comment = format!(
            "Superseded by replacement branch after wt merge local-squash / remote-history \
collision. Tree unchanged; recreated on `{new_branch}` with a single squash commit."
        );
        gh(
            repo,
            &["pr", "close", &old.to_string(), "--comment", &comment],
        )
        .context("failed to close superseded PR")?;
    }

    let title = commit_subject(repo, "HEAD")?;
    let body = supersession_body(branch, closed_pr);
    let pr_url = gh(
        repo,
        &[
            "pr",
            "create",
            "--base",
            target_branch,
            "--head",
            &new_branch,
            "--title",
            &title,
            "--body",
            &body,
        ],
    )
    .context("failed to create replacement PR")?;

    let new_pr = parse_pr_number_from_url(&pr_url)
        .with_context(|| format!("failed to parse PR number from gh output: {pr_url:?}"))?;

    // Squash-merge the replacement PR and fetch the new target so `wt merge`
    // completes the full cycle rather than leaving a PR open for human
    // attention.
    gh(
        repo,
        &[
            "pr",
            "merge",
            &new_pr.to_string(),
            "--squash",
            "--delete-branch",
        ],
    )
    .context("failed to squash-merge replacement PR")?;
    repo.run_command(&["fetch", "origin", target_branch])?;

    Ok(ReconcileOutcome::Restacked {
        new_branch,
        closed_pr,
        new_pr,
    })
}

fn abort_message(branch: &str, target_branch: &str) -> String {
    format!(
        "remote branch 'origin/{branch}' has commits not in the local squash; merge aborted. \
To recover manually:\n\
  git branch {branch}-v2 HEAD\n\
  git push -u origin {branch}-v2\n\
  gh pr close <old-pr-number> --comment 'Superseded by {branch}-v2'\n\
  gh pr create --base {target_branch} --head {branch}-v2 --title ... --body ...\n\
Or re-run with `[merge] on_diverged_remote = \"remote-squash\"` or `\"restack\"`."
    )
}

fn find_open_pr(repo: &Repository, branch: &str) -> anyhow::Result<Option<u32>> {
    let out = gh(
        repo,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "open",
            "--json",
            "number",
            "--jq",
            ".[0].number",
        ],
    )?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        trimmed
            .parse::<u32>()
            .map(Some)
            .with_context(|| format!("failed to parse PR number: {trimmed:?}"))
    }
}

fn open_draft_pr(repo: &Repository, branch: &str, target_branch: &str) -> anyhow::Result<u32> {
    let title = commit_subject(repo, "HEAD")?;
    let body = format!(
        "Auto-opened by `wt merge` for remote-squash reconciliation. Feature branch `{branch}` \
has pre-squash commits on origin; server-side squash-merge will collapse them into one commit \
on `{target_branch}`."
    );
    let out = gh(
        repo,
        &[
            "pr",
            "create",
            "--draft",
            "--base",
            target_branch,
            "--head",
            branch,
            "--title",
            &title,
            "--body",
            &body,
        ],
    )?;
    parse_pr_number_from_url(&out)
        .with_context(|| format!("failed to parse PR number from gh output: {out:?}"))
}

fn next_vn_name(repo: &Repository, branch: &str) -> anyhow::Result<String> {
    // Start at v2; if -v2 already exists locally or remotely, go v3, v4, ...
    // The branch is pushed to origin as part of restack, so we check both.
    let mut n = 2u32;
    loop {
        let candidate = format!("{branch}-v{n}");
        let local_exists = repo
            .run_command_check(&[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ])
            .unwrap_or(false);
        let remote_exists = !repo
            .run_command(&["ls-remote", "--heads", "origin", &candidate])
            .unwrap_or_default()
            .trim()
            .is_empty();
        if !local_exists && !remote_exists {
            return Ok(candidate);
        }
        n += 1;
        if n > 99 {
            return Err(anyhow!(
                "exhausted -v2..-v99 candidates for branch '{branch}'; clean up old restacks"
            ));
        }
    }
}

fn commit_subject(repo: &Repository, commit: &str) -> anyhow::Result<String> {
    Ok(repo
        .run_command(&["log", "-1", "--format=%s", commit])?
        .trim()
        .to_string())
}

fn supersession_body(original_branch: &str, closed_pr: Option<u32>) -> String {
    let super_note = match closed_pr {
        Some(n) => format!("Supersedes #{n}."),
        None => "No prior PR found; this PR replaces the local-squash attempt.".to_string(),
    };
    format!(
        "{super_note}\n\nOriginal branch: `{original_branch}`. Tree unchanged; squashed into a \
single commit for clean main history. Opened automatically by `wt merge` after detecting a \
non-fast-forward remote state."
    )
}

fn parse_pr_number_from_url(url_or_number: &str) -> Option<u32> {
    let trimmed = url_or_number.trim();
    if let Ok(n) = trimmed.parse::<u32>() {
        return Some(n);
    }
    // gh pr create emits the full URL, e.g. https://github.com/owner/repo/pull/704
    trimmed
        .rsplit('/')
        .find_map(|segment| segment.parse::<u32>().ok())
}

/// Invoke `gh` with `args`, returning stdout. Routes through `shell_exec::Cmd`
/// so the invocation participates in the usual worktrunk logging + concurrency
/// semaphore (`gh` is not part of git and doesn't go through `repo.run_command`).
fn gh(repo: &Repository, args: &[&str]) -> anyhow::Result<String> {
    let cwd = repo.current_worktree().root()?;
    let output = Cmd::new("gh")
        .args(args.iter().map(|s| (*s).to_string()))
        .current_dir(cwd)
        .external("remote-reconcile:gh")
        .run()
        .context("failed to spawn gh; is the GitHub CLI installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh {args:?} failed: {}", stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_number_from_numeric_input() {
        assert_eq!(parse_pr_number_from_url("704"), Some(704));
    }

    #[test]
    fn parse_pr_number_from_full_url() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/Grokr-Labs/pwm-os/pull/704"),
            Some(704)
        );
    }

    #[test]
    fn parse_pr_number_tolerates_trailing_newline() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/Grokr-Labs/pwm-os/pull/709\n"),
            Some(709)
        );
    }

    #[test]
    fn parse_pr_number_returns_none_for_non_numeric() {
        assert_eq!(parse_pr_number_from_url("not-a-url"), None);
    }

    #[test]
    fn supersession_body_with_prior_pr_references_number() {
        let body = supersession_body("feat/x", Some(709));
        assert!(body.contains("Supersedes #709"));
        assert!(body.contains("feat/x"));
    }

    #[test]
    fn supersession_body_without_prior_pr_notes_absence() {
        let body = supersession_body("feat/x", None);
        assert!(body.contains("No prior PR found"));
    }

    #[test]
    fn abort_message_names_branches_and_fallback_strategies() {
        let msg = abort_message("feat/x", "main");
        assert!(msg.contains("feat/x"));
        assert!(msg.contains("main"));
        assert!(msg.contains("remote-squash"));
        assert!(msg.contains("restack"));
    }
}
