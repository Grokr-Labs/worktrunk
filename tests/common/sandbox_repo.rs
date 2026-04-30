//! Throwaway GitHub sandbox repos for end-to-end `reconcile_and_push` tests.
//!
//! Each [`SandboxRepo`] creates a private `JonathanWorks/wt-e2e-<random>` repo
//! via `gh repo create`, clones it locally, and tears it down on `Drop`.
//! Tests using this helper must be `#[ignore]`-gated so a normal `cargo test`
//! never hits the GitHub API; opt in with `cargo test -- --ignored`.
//!
//! Lifetime contract:
//! - `new()` succeeds only when `gh auth status` returns 0 — otherwise it
//!   panics with a clear message; tests should use [`require_gh_auth`] at the
//!   top of every test body so they fail fast with the same diagnostic if
//!   somebody runs the suite without `gh` auth.
//! - `Drop` runs even when a test panics. It calls `gh repo delete --yes`
//!   best-effort: if the delete fails (token revoked mid-test, network blip,
//!   repo never finished creating), the failure is logged to stderr but does
//!   not propagate as a panic-in-drop (which would abort the test process and
//!   mask the original failure).
//!
//! Test repos are named `wt-e2e-<unix-ms>-<rand6>` so concurrent runs don't
//! collide and ad-hoc cleanup (`gh repo list JonathanWorks --json name --jq
//! '.[] | select(.name | startswith("wt-e2e-"))'`) is grep-able.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Account that owns the throwaway sandbox repos.
///
/// Hardcoded rather than env-driven: the test suite is `#[ignore]`-gated and
/// only meaningful for this fork's operator. Future contributors who fork the
/// fork can change this constant.
pub const SANDBOX_OWNER: &str = "JonathanWorks";

/// Verify `gh auth status` succeeds; panic with a helpful message otherwise.
///
/// Call from every E2E test body before constructing a [`SandboxRepo`] so the
/// failure mode is "skipped because gh isn't logged in" not "fixture crashed
/// while creating a repo we couldn't reach".
pub fn require_gh_auth() {
    static CHECKED: OnceLock<bool> = OnceLock::new();
    let ok = *CHECKED.get_or_init(|| {
        Command::new("gh")
            .args(["auth", "status"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    });
    assert!(
        ok,
        "gh CLI is not authenticated. Run `gh auth login` before invoking the \
e2e_remote_reconcile suite (these tests are `#[ignore]`-gated and only run \
on demand via `cargo test -- --ignored`)."
    );
}

/// A throwaway GitHub repo + matching local clone, scoped to one test.
///
/// On `Drop`, the remote repo is deleted via `gh repo delete --yes`. The local
/// `tempfile::TempDir` clean themselves up.
pub struct SandboxRepo {
    /// Owner/repo string, e.g. `JonathanWorks/wt-e2e-1714562345-abc123`.
    pub full_name: String,
    /// Short name, e.g. `wt-e2e-1714562345-abc123`.
    pub name: String,
    /// HTTPS clone URL.
    pub clone_url: String,
    /// Local clone path (lives under `_dir`).
    pub clone_path: PathBuf,
    /// Owns the tempdir; dropping cleans up the local clone.
    _dir: tempfile::TempDir,
}

impl SandboxRepo {
    /// Create a private `wt-e2e-<random>` repo on GitHub, clone it locally,
    /// configure git identity, and seed it with one initial commit on `main`
    /// pushed to origin so subsequent feature branches have a base to diverge
    /// from.
    pub fn new() -> Self {
        require_gh_auth();
        let name = generate_repo_name();
        let full_name = format!("{SANDBOX_OWNER}/{name}");

        // Create private repo on GitHub.
        let create = Command::new("gh")
            .args([
                "repo",
                "create",
                &full_name,
                "--private",
                "--description",
                "Throwaway sandbox for worktrunk E2E reconcile_and_push tests \
(BRW-J23HNY). Auto-deleted by the test fixture on drop.",
            ])
            .output()
            .expect("gh repo create");
        if !create.status.success() {
            panic!(
                "gh repo create failed for {full_name}: {}",
                String::from_utf8_lossy(&create.stderr)
            );
        }
        let clone_url = format!("https://github.com/{full_name}.git");

        // Clone into a tempdir.
        let dir = tempfile::tempdir().expect("create tempdir for sandbox clone");
        let clone_path = dir.path().join(&name);
        let clone = Command::new("git")
            .args(["clone", "--quiet", &clone_url])
            .arg(&clone_path)
            .output()
            .expect("git clone");
        if !clone.status.success() {
            panic!(
                "git clone {clone_url} failed: {}",
                String::from_utf8_lossy(&clone.stderr)
            );
        }

        // Local git identity. Tests run in CI / sandbox accounts that may not
        // have global identity configured.
        run_git_in(&clone_path, &["config", "user.email", "wt-e2e@example.com"]);
        run_git_in(&clone_path, &["config", "user.name", "wt-e2e"]);

        // Seed `main` with one commit so feature branches branch off something.
        std::fs::write(clone_path.join("README.md"), "# wt-e2e sandbox\n").expect("seed README");
        run_git_in(&clone_path, &["add", "README.md"]);
        run_git_in(&clone_path, &["commit", "--quiet", "-m", "initial"]);
        run_git_in(&clone_path, &["push", "--quiet", "-u", "origin", "main"]);

        Self {
            full_name,
            name,
            clone_url,
            clone_path,
            _dir: dir,
        }
    }

    /// Run `git` in the local clone, returning the captured output. Panics on
    /// non-zero exit so test bodies fail loudly rather than silently misbehaving.
    pub fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.clone_path)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        if !output.status.success() {
            panic!(
                "git {args:?} in {} failed: {}",
                self.clone_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run `gh` against this sandbox repo (`--repo {full_name}` is implicit
    /// when the cwd is the local clone). Returns stdout; panics on non-zero
    /// exit.
    pub fn gh(&self, args: &[&str]) -> String {
        let output = Command::new("gh")
            .args(args)
            .current_dir(&self.clone_path)
            .output()
            .unwrap_or_else(|e| panic!("gh {args:?} failed to spawn: {e}"));
        if !output.status.success() {
            panic!(
                "gh {args:?} for {} failed: {}",
                self.full_name,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

impl Drop for SandboxRepo {
    fn drop(&mut self) {
        // Best-effort: log but don't panic-in-drop, which would abort the
        // process and mask whatever the test was actually asserting.
        let result = Command::new("gh")
            .args(["repo", "delete", &self.full_name, "--yes"])
            .output();
        match result {
            Ok(o) if o.status.success() => {}
            Ok(o) => eprintln!(
                "warning: sandbox repo {} was not deleted (exit {}): {}",
                self.full_name,
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!(
                "warning: sandbox repo {} cleanup spawn failed: {e}",
                self.full_name
            ),
        }
    }
}

fn run_git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
    if !output.status.success() {
        panic!(
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn generate_repo_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    // ms timestamp gives cross-process uniqueness; an atomic counter gives
    // intra-process uniqueness so parallel tests in the same `cargo test` run
    // never collide. Process id mixes in for additional safety when two
    // operators run the suite concurrently.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let rand = format!("{:x}{:x}", std::process::id(), n);
    format!("wt-e2e-{ms}-{rand}")
}
