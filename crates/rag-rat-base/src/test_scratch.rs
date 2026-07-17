//! Shared scratch-directory helper for the workspace's test fixtures.
//!
//! Historically every fixture rolled its own `std::env::temp_dir().join(...)` scratch dir, cleaned
//! only by a `Drop` impl. A panicking test, a killed `nextest` run, or a SIGKILL'd process bypasses
//! `Drop` and strands the directory under the system temp — on the dev box 16k dirs / ~14 GB of
//! tmpfs accumulated before a manual sweep (#726).
//!
//! # Why the system temp, not `target/tmp`
//!
//! The obvious "per-checkout, `cargo clean`-reclaimed" fix — relocate scratch under the build's
//! `target/tmp/` — is UNSOUND for this codebase. Many fixtures `git init` a throwaway repo and rely
//! on it having NO ancestor `.git`; `target/tmp/` is inside the project's own git working tree, so
//! rag-rat's git-context discovery walks up and resolves the *outer* rag-rat repo's identity/commit
//! (observed: 295 `schema_bootstrap_tests` / worktree-scope failures, panicking with the outer
//! repo's HEAD sha). Scratch therefore MUST stay in the system temp, isolated from any repo.
//!
//! So this module fixes the leak the other way the issue proposed — a self-healing sweep:
//!   1. all scratch lives under ONE dedicated, namespaced sub-root of the system temp
//!      ([`SCRATCH_NAMESPACE`]), so a stranded dir is trivially found and the sweep below can never
//!      touch anything but this helper's own dirs;
//!   2. the FIRST scratch request in each process sweeps sibling scratch dirs older than
//!      [`SWEEP_MAX_AGE`] out of that root — self-healing after a `kill -9`, no external cron.
//!
//! `Drop`-based cleanup stays the fast path; the age-bounded sweep is only the backstop for the
//! cases where `Drop` never runs.
//!
//! Not `#[cfg(test)]`: that gate does not propagate across crates, and fixtures in sibling crates
//! (`rag-rat-core`, `rag-rat-cli`, …) consume this helper. It is not part of the semver-stable API
//! surface. Matches the existing `rag_rat_oracle::test_support` convention.

use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// The single dedicated sub-directory of the system temp under which ALL test scratch lives. A
/// dedicated namespace keeps the sweep's blast radius to this helper's own dirs (never the shared
/// temp root) and makes any manual cleanup a one-liner: `rm -rf $TMPDIR/rag-rat-test-scratch`.
pub const SCRATCH_NAMESPACE: &str = "rag-rat-test-scratch";

/// Sweep scratch dirs older than this on first use per process. Two hours is far longer than any
/// single test or `nextest` run (CI caps a wedged test at 60s), so the threshold can never delete a
/// LIVE parallel worker's fresh dir — only genuinely stranded ones from a crashed/killed run.
pub const SWEEP_MAX_AGE: Duration = Duration::from_secs(2 * 60 * 60);

/// Process-wide counter that makes scratch names unique WITHIN a process; the PID makes them unique
/// ACROSS processes (nextest runs each test in its own process).
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Guards the once-per-process startup sweep.
static SWEEP: Once = Once::new();

/// The scratch root shared by every test in every crate: `<system temp>/rag-rat-test-scratch`.
///
/// The system temp (not `target/tmp`) so `git init` fixtures have no ancestor `.git`; a dedicated
/// namespace so the sweep can only ever remove this helper's own dirs.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(SCRATCH_NAMESPACE)
}

/// Delete scratch dirs directly under `root` whose mtime is older than [`SWEEP_MAX_AGE`].
/// Race-safe: every filesystem call tolerates a dir a parallel worker removed first (ENOENT and
/// friends are ignored), and the age bound guarantees a fresh dir from a live worker is never
/// touched.
fn sweep_stale(root: &Path) {
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(root) else {
        return; // root not created yet / vanished — nothing to sweep.
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else { continue };
        if !metadata.is_dir() {
            continue;
        }
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age >= SWEEP_MAX_AGE);
        if old_enough {
            // Ignore errors: another worker may be removing the same stale dir concurrently.
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// A unique scratch directory PATH under [`scratch_root`], keyed by `tag`, the PID, and a
/// process-wide counter. The path is NOT created (callers create it, matching the fixtures this
/// replaces); any pre-existing dir at the exact path is removed first.
///
/// On the first call per process this also sweeps stale sibling scratch dirs out of the root (see
/// module docs) — the self-healing backstop for `Drop` cleanup that a `kill -9` skips.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let root = scratch_root();
    let _ = std::fs::create_dir_all(&root);
    SWEEP.call_once(|| sweep_stale(&root));

    let name = format!("{tag}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let dir = root.join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use filetime::{FileTime, set_file_mtime};

    use super::*;

    /// New scratch dirs must land inside the ONE dedicated namespace sub-root of the system temp —
    /// never scattered directly across the shared temp root (which is what let 16k dirs accumulate,
    /// and what would make the sweep unsafe). Bites if creation is pointed back at the bare temp
    /// dir.
    #[test]
    fn scratch_dirs_live_under_the_dedicated_namespace() {
        let root = scratch_root();
        assert_eq!(
            root.file_name(),
            Some(OsStr::new(SCRATCH_NAMESPACE)),
            "scratch root should be the dedicated namespace, got {root:?}",
        );
        // The namespace is a CHILD of the system temp, not the bare temp root itself (a bare-root
        // sweep could delete unrelated files).
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert_ne!(root, std::env::temp_dir());

        let dir = scratch_dir("location-probe");
        assert!(dir.starts_with(&root), "{dir:?} is not under the scratch root {root:?}");
    }

    /// The startup sweep must delete a stranded (aged) sibling while sparing a fresh one — the
    /// self-healing backstop for a `kill -9` that skipped `Drop`. Uses an isolated sub-root so
    /// parallel test processes can't perturb the assertion. Bites if `sweep_stale` is neutered.
    #[test]
    fn sweep_removes_stale_dirs_but_spares_fresh_ones() {
        // A private root for this test only (unique via `scratch_dir`), so no other worker's sweep
        // or dirs interfere with what we assert.
        let test_root = scratch_dir("sweep-probe-root");
        std::fs::create_dir_all(&test_root).unwrap();

        let stale = test_root.join("stranded-by-kill");
        let fresh = test_root.join("live-worker");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();

        // Age the stale dir well past the threshold; leave `fresh` at its just-created mtime.
        let aged = SystemTime::now() - (SWEEP_MAX_AGE + Duration::from_secs(3600));
        set_file_mtime(&stale, FileTime::from_system_time(aged)).unwrap();

        sweep_stale(&test_root);

        assert!(!stale.exists(), "sweep should have removed the stale (aged) dir {stale:?}");
        assert!(fresh.exists(), "sweep must NOT remove the fresh dir {fresh:?}");
    }
}
