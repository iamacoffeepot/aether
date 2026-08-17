//! A deterministic spawn seam for tests: writes a fixed `evidence.json` into the
//! run's output dir and hands back a process pinned to a fixed lifecycle — the
//! whole seam, without a real repo or Claude credential. Shared by this module's
//! unit tests and the executor-reactor runtime test.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use aether_bloomery::BackendObjectId;

use super::runner::CapturedObjects;
use super::{LocalExecutorError, RunLifecycle, RunProcess, RunSpec, TransformRunner};

/// The canned capture a [`FixedRunner`] with `captures: true` returns: a
/// fixed commit/tree object pair, so a test can assert the digests the
/// backend derives and records from it.
///
/// Twenty distinct bytes each — a SHA-1-shaped id, since that is what a real
/// capture against today's git produces.
#[must_use]
pub fn canned_capture() -> CapturedObjects {
    CapturedObjects {
        commit: BackendObjectId::new(vec![0xcc; 20]),
        tree: BackendObjectId::new(vec![0xdd; 20]),
        // The seam stub commits nothing, so it holds no diff: a lap the triage
        // cannot inspect is a lap it passes (#4959).
        diff: None,
    }
}

/// A runner that writes `evidence` and returns a process fixed at `lifecycle`.
///
/// Tests pin the richer lifecycle — a clean exit, a terminating signal, or a
/// wait fault — directly; this seam does not invent process facts.
pub struct FixedRunner {
    /// The `evidence.json` bytes every run writes.
    pub evidence: String,
    /// The lifecycle every spawned process reports.
    pub lifecycle: RunLifecycle,
    /// Whether `capture` returns the [`canned_capture`] pair (`true`) or a
    /// clean-worktree `None` (`false`).
    pub captures: bool,
    /// The message each `capture` was handed — what the real runner commits its
    /// subject from, recorded so a test can assert the lane's own message
    /// reached the capture rather than the flat literal.
    pub captured_messages: Arc<Mutex<Vec<Option<String>>>>,
    /// Direct parents `checkout_parents` reports for a checkout hex. A missing
    /// key is an empty parent list — the same answer as the trait default.
    pub parents: HashMap<String, Vec<String>>,
    /// When set, `checkout_parents` fails rather than returning a list — the
    /// fixture for a checkout the production adapter cannot inspect.
    pub fail_parents: bool,
}

impl FixedRunner {
    /// A runner writing `evidence`, fixed at `lifecycle`, capturing (or not) the
    /// canned pair. Its `captured_messages` log starts empty; clone the handle
    /// off the runner before mounting it to read what the backend handed each
    /// capture.
    #[must_use]
    pub fn new(evidence: &str, lifecycle: RunLifecycle, captures: bool) -> Self {
        Self {
            evidence: evidence.to_owned(),
            lifecycle,
            captures,
            captured_messages: Arc::new(Mutex::new(Vec::new())),
            parents: HashMap::new(),
            fail_parents: false,
        }
    }
}

impl TransformRunner for FixedRunner {
    fn start(&self, spec: &RunSpec<'_>) -> Result<Box<dyn RunProcess>, LocalExecutorError> {
        fs::create_dir_all(spec.evidence_dir).map_err(LocalExecutorError::Io)?;
        fs::write(spec.evidence_dir.join("evidence.json"), &self.evidence).map_err(LocalExecutorError::Io)?;
        Ok(Box::new(FixedProcess { lifecycle: self.lifecycle }))
    }

    // The stub never materializes a real worktree (`start` writes only the
    // evidence dir), so there is nothing to tear down and nothing is registered.
    fn release(&self, _worktree_dir: &Path) -> Result<(), LocalExecutorError> {
        Ok(())
    }

    fn registered_worktrees(&self) -> Result<Vec<PathBuf>, LocalExecutorError> {
        Ok(Vec::new())
    }

    fn capture(
        &self,
        _worktree_dir: &Path,
        message: Option<&str>,
    ) -> Result<Option<CapturedObjects>, LocalExecutorError> {
        self.captured_messages.lock().unwrap_or_else(PoisonError::into_inner).push(message.map(str::to_owned));
        Ok(self.captures.then(canned_capture))
    }

    fn checkout_parents(&self, checkout_hex: &str) -> Result<Vec<String>, LocalExecutorError> {
        if self.fail_parents {
            return Err(LocalExecutorError::Worktree("checkout parents unreadable".to_owned()));
        }
        Ok(self.parents.get(checkout_hex).cloned().unwrap_or_default())
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
