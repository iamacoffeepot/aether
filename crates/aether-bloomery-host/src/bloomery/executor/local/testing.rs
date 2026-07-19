//! A deterministic spawn seam for tests: writes a fixed `evidence.json` into the
//! run's output dir and hands back a process pinned to a fixed lifecycle — the
//! whole seam, without a real repo or Claude credential. Shared by this module's
//! unit tests and the executor-reactor runtime test.

use std::fs;
use std::path::Path;

use aether_bloomery_github::GitObjectId;

use super::runner::CapturedObjects;
use super::{LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};

/// The canned capture a [`FixedRunner`] with `captures: true` returns: a
/// fixed commit/tree object pair, so a test can assert the digests the
/// backend derives and records from it.
#[must_use]
pub fn canned_capture() -> CapturedObjects {
    CapturedObjects {
        commit: GitObjectId::from_hex(&"c".repeat(40)).expect("40 hex chars is a well-formed sha1"),
        tree: GitObjectId::from_hex(&"d".repeat(40)).expect("40 hex chars is a well-formed sha1"),
    }
}

/// A runner that writes `evidence` and returns a process fixed at `lifecycle`.
pub struct FixedRunner {
    /// The `evidence.json` bytes every run writes.
    pub evidence: String,
    /// The lifecycle every spawned process reports.
    pub lifecycle: RunLifecycle,
    /// Whether `capture` returns the [`canned_capture`] pair (`true`) or a
    /// clean-worktree `None` (`false`).
    pub captures: bool,
}

impl TransformRunner for FixedRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        fs::write(spec.evidence_dir.join("evidence.json"), &self.evidence).map_err(LocalExecutorError::Io)?;
        Ok(Box::new(FixedProcess { lifecycle: self.lifecycle }))
    }

    // The stub never materializes a real worktree (`start` writes only the
    // evidence dir), so there is nothing to tear down.
    fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        Ok(())
    }

    fn capture(&self, _worktree_dir: &Path) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        Ok(self.captures.then(canned_capture))
    }
}

struct FixedProcess {
    lifecycle: RunLifecycle,
}

impl RunProcess for FixedProcess {
    fn poll(&mut self) -> RunLifecycle {
        self.lifecycle
    }

    fn kill(&mut self) -> Result<(), LocalExecutorError> {
        Ok(())
    }
}
