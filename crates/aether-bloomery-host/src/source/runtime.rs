//! The `SourceShell`-backed runtime for [`SourceCapability`] (ADR-0149 §The
//! boundary).
//!
//! State wraps the existing [`SourceShell`] (`bloomery/source.rs`) unchanged —
//! reused, not re-implemented; the `Arc<dyn SourceBackend>` indirection it
//! already carries is what keeps the concrete GitHub type out of core
//! modules. Each inherent method decodes its request's
//! `aether_data::wire`-encoded port values, calls the matching shell method,
//! and encodes the outcome into the reply — a mail front over the shell, no
//! new source behavior. The `#[handler::single]` functions are thin
//! delegates, mirroring [`ArtifactsCapabilityState`](crate::artifacts::ArtifactsCapabilityState)'s
//! `put` / `get` split, so the handler tests can drive these methods directly
//! over an explicit [`SourceShell`].

use aether_actor::runtime;
use aether_bloomery::{
    BloomId, Checkpoint, ClaimOutcome, ClaimSeal, ClaimSealResult, Digest, IntegrateOutcome, LandOutcome, ReleaseSeal,
    ReleaseSealResult, WorkpieceId,
};
use aether_data::wire::{from_bytes, to_vec};

use super::SourceCapability;
use super::kinds::{
    Integrate, IntegrateResult, Land, LandResult, ListCheckpoints, ListCheckpointsResult, RecordCheckpoint,
    RecordCheckpointResult, Snapshot, SnapshotResult,
};
use crate::bloomery::SourceShell;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Runtime state for [`SourceCapability`]: the shell the dispatcher owns, plus
/// whether the GitHub connection is configured. An unconfigured instance runs
/// the seal-claim path in **local-backstop-only** mode (ADR-0150): it never
/// reaches GitHub, so `claim_seal` / `release_seal` short-circuit to
/// `Acquired` / `Ok` and the instance's exclusivity rests on the local `SQLite`
/// constraint alone — the single-instance / untokened-dev case, and what keeps
/// a seal from depending on GitHub reachability when no coordination is needed.
pub struct SourceCapabilityState {
    shell: SourceShell,
    configured: bool,
}

impl SourceCapabilityState {
    /// Build state over an explicit shell — the seam the handler tests drive.
    /// Configured (the claim path reaches the shell) so tests exercise the real
    /// acquire/release logic against the fake-GitHub-backed shell.
    #[must_use]
    pub fn new(shell: SourceShell) -> Self {
        Self { shell, configured: true }
    }

    /// Build state in local-backstop-only mode (unconfigured) — the seam a test
    /// drives to exercise the GitHub-free seal-claim short-circuit.
    #[must_use]
    pub fn new_unconfigured(shell: SourceShell) -> Self {
        Self { shell, configured: false }
    }

    /// Decode `base`, snapshot the source there, and encode the outcome.
    #[must_use]
    pub fn snapshot(&self, base: &[u8]) -> SnapshotResult {
        let base: Digest = match from_bytes(base) {
            Ok(base) => base,
            Err(error) => return SnapshotResult::Err { error: error.to_string() },
        };
        match self.shell.snapshot(&base) {
            Ok(snapshot) => match to_vec(&snapshot) {
                Ok(snapshot) => SnapshotResult::Ok { snapshot },
                Err(error) => SnapshotResult::Err { error: error.to_string() },
            },
            Err(error) => SnapshotResult::Err { error: error.to_string() },
        }
    }

    /// Decode `bloom` / `tree`, record the checkpoint, and encode the outcome.
    #[must_use]
    pub fn record_checkpoint(&self, bloom: &[u8], tree: &[u8]) -> RecordCheckpointResult {
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return RecordCheckpointResult::Err { error: error.to_string() },
        };
        let tree: Digest = match from_bytes(tree) {
            Ok(tree) => tree,
            Err(error) => return RecordCheckpointResult::Err { error: error.to_string() },
        };
        match self.shell.checkpoint(&bloom, &tree) {
            Ok(checkpoint) => match to_vec(&checkpoint) {
                Ok(checkpoint) => RecordCheckpointResult::Ok { checkpoint },
                Err(error) => RecordCheckpointResult::Err { error: error.to_string() },
            },
            Err(error) => RecordCheckpointResult::Err { error: error.to_string() },
        }
    }

    /// Decode `bloom`, enumerate its checkpoints, and encode the outcome.
    #[must_use]
    pub fn list_checkpoints(&self, bloom: &[u8]) -> ListCheckpointsResult {
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return ListCheckpointsResult::Err { error: error.to_string() },
        };
        match self.shell.checkpoints(&bloom) {
            Ok(checkpoints) => {
                let mut encoded = Vec::with_capacity(checkpoints.len());
                for checkpoint in &checkpoints {
                    match to_vec(checkpoint) {
                        Ok(bytes) => encoded.push(bytes),
                        Err(error) => return ListCheckpointsResult::Err { error: error.to_string() },
                    }
                }
                ListCheckpointsResult::Ok { checkpoints: encoded }
            }
            Err(error) => ListCheckpointsResult::Err { error: error.to_string() },
        }
    }

    /// Decode `bloom` / `candidate` / `expected`, integrate, and encode the outcome.
    #[must_use]
    pub fn integrate(&self, bloom: &[u8], candidate: &[u8], expected: &[u8]) -> IntegrateResult {
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return IntegrateResult::Err { error: error.to_string() },
        };
        let candidate: Digest = match from_bytes(candidate) {
            Ok(candidate) => candidate,
            Err(error) => return IntegrateResult::Err { error: error.to_string() },
        };
        let expected: Checkpoint = match from_bytes(expected) {
            Ok(expected) => expected,
            Err(error) => return IntegrateResult::Err { error: error.to_string() },
        };
        match self.shell.integrate(&bloom, &candidate, &expected) {
            Ok(IntegrateOutcome::Integrated { tree }) => match to_vec(&tree) {
                Ok(tree) => IntegrateResult::Integrated { tree },
                Err(error) => IntegrateResult::Err { error: error.to_string() },
            },
            Ok(IntegrateOutcome::Conflict { at }) => match to_vec(&at) {
                Ok(at) => IntegrateResult::Conflict { at },
                Err(error) => IntegrateResult::Err { error: error.to_string() },
            },
            Ok(IntegrateOutcome::StaleCheckpoint { actual }) => match to_vec(&actual) {
                Ok(actual) => IntegrateResult::StaleCheckpoint { actual },
                Err(error) => IntegrateResult::Err { error: error.to_string() },
            },
            Err(error) => IntegrateResult::Err { error: error.to_string() },
        }
    }

    /// Decode `bloom` / `expected_base` / `new_head`, land, and encode the outcome.
    #[must_use]
    pub fn land(&self, bloom: &[u8], expected_base: &[u8], new_head: &[u8]) -> LandResult {
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return LandResult::Err { error: error.to_string() },
        };
        let expected_base: Digest = match from_bytes(expected_base) {
            Ok(expected_base) => expected_base,
            Err(error) => return LandResult::Err { error: error.to_string() },
        };
        let new_head: Digest = match from_bytes(new_head) {
            Ok(new_head) => new_head,
            Err(error) => return LandResult::Err { error: error.to_string() },
        };
        match self.shell.land(&bloom, &expected_base, &new_head) {
            Ok(LandOutcome::Landed(receipt)) => match to_vec(&receipt) {
                Ok(receipt) => LandResult::Landed { receipt },
                Err(error) => LandResult::Err { error: error.to_string() },
            },
            Ok(LandOutcome::BaseMoved { expected, actual }) => match (to_vec(&expected), to_vec(&actual)) {
                (Ok(expected), Ok(actual)) => LandResult::BaseMoved { expected, actual },
                (Err(error), _) | (_, Err(error)) => LandResult::Err { error: error.to_string() },
            },
            Err(error) => LandResult::Err { error: error.to_string() },
        }
    }

    /// Decode `bloom` / `workpieces`, acquire the shared seal claim, and encode
    /// the [`ClaimOutcome`](aether_bloomery::ClaimOutcome) into the reply,
    /// echoing `idempotency_key` for the admitter's correlation.
    #[must_use]
    pub fn claim_seal(&self, idempotency_key: String, bloom: &[u8], workpieces: &[Vec<u8>]) -> ClaimSealResult {
        // Local-backstop-only mode: no GitHub, so the acquire is a no-op success
        // and exclusivity rests on the local SQLite constraint (ADR-0150).
        if !self.configured {
            return match to_vec(&ClaimOutcome::Acquired) {
                Ok(outcome) => ClaimSealResult::Ok { idempotency_key, outcome },
                Err(error) => ClaimSealResult::Err { idempotency_key, error: error.to_string() },
            };
        }
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return ClaimSealResult::Err { idempotency_key, error: error.to_string() },
        };
        let workpieces = match decode_workpieces(workpieces) {
            Ok(workpieces) => workpieces,
            Err(error) => return ClaimSealResult::Err { idempotency_key, error },
        };
        match self.shell.claim_seal(&bloom, &workpieces) {
            Ok(outcome) => match to_vec(&outcome) {
                Ok(outcome) => ClaimSealResult::Ok { idempotency_key, outcome },
                Err(error) => ClaimSealResult::Err { idempotency_key, error: error.to_string() },
            },
            Err(error) => ClaimSealResult::Err { idempotency_key, error: error.to_string() },
        }
    }

    /// Decode `bloom` / `workpieces` and release the shared seal claim.
    #[must_use]
    pub fn release_seal(&self, bloom: &[u8], workpieces: &[Vec<u8>]) -> ReleaseSealResult {
        // Local-backstop-only mode: nothing was acquired, so nothing to release.
        if !self.configured {
            return ReleaseSealResult::Ok;
        }
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return ReleaseSealResult::Err { error: error.to_string() },
        };
        let workpieces = match decode_workpieces(workpieces) {
            Ok(workpieces) => workpieces,
            Err(error) => return ReleaseSealResult::Err { error },
        };
        match self.shell.release_seal(&bloom, &workpieces) {
            Ok(()) => ReleaseSealResult::Ok,
            Err(error) => ReleaseSealResult::Err { error: error.to_string() },
        }
    }
}

/// Decode a claim request's `workpieces` — one `aether_data::wire`-encoded
/// [`WorkpieceId`] per entry — into the typed list, or a human-readable error
/// naming the entry that did not decode.
fn decode_workpieces(encoded: &[Vec<u8>]) -> Result<Vec<WorkpieceId>, String> {
    encoded.iter().map(|bytes| from_bytes::<WorkpieceId>(bytes).map_err(|error| error.to_string())).collect()
}

#[runtime]
impl NativeActor for SourceCapability {
    type State = SourceCapabilityState;
    type Config = super::SourceConfig;

    const NAMESPACE: &'static str = "aether.source";

    fn init(config: super::SourceConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<SourceCapabilityState, BootError> {
        // Unconfigured (empty token / owner / repo) → local-backstop-only: the
        // seal-claim path never reaches GitHub (ADR-0150), matching the mirror
        // driver's disabled mode. The shell still connects (client construction
        // touches no network) so the pre-migration source ops stay available.
        let configured = !(config.token.is_empty() || config.owner.is_empty() || config.repo.is_empty());
        let shell = SourceShell::connect(&config).map_err(|error| BootError::Other(Box::new(error)))?;
        tracing::info!(target: "aether_bloomery_host::source", configured, "source shell connected");
        Ok(SourceCapabilityState { shell, configured })
    }

    // The `#[handler::single]` contract requires the mail by value; every
    // handler here only borrows its fields to decode, so clippy sees a
    // by-ref opportunity the macro signature cannot take.
    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_snapshot(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Snapshot) -> SnapshotResult {
        state.snapshot(&mail.base)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_record_checkpoint(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RecordCheckpoint,
    ) -> RecordCheckpointResult {
        state.record_checkpoint(&mail.bloom, &mail.tree)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_list_checkpoints(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ListCheckpoints,
    ) -> ListCheckpointsResult {
        state.list_checkpoints(&mail.bloom)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_integrate(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Integrate) -> IntegrateResult {
        state.integrate(&mail.bloom, &mail.candidate, &mail.expected)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_land(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Land) -> LandResult {
        state.land(&mail.bloom, &mail.expected_base, &mail.new_head)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_claim_seal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ClaimSeal) -> ClaimSealResult {
        state.claim_seal(mail.idempotency_key, &mail.bloom, &mail.workpieces)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_release_seal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ReleaseSeal) -> ReleaseSealResult {
        state.release_seal(&mail.bloom, &mail.workpieces)
    }
}
