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
use aether_bloomery::{BloomId, Checkpoint, ClaimOutcome, Digest, IntegrateOutcome, LandOutcome, WorkpieceId};
use aether_data::wire::{from_bytes, to_vec};

use super::SourceCapability;
use super::kinds::{
    ClaimResult, ClaimSeal, Integrate, IntegrateResult, Land, LandResult, ListCheckpoints, ListCheckpointsResult,
    RecordCheckpoint, RecordCheckpointResult, ReleaseSeal, Snapshot, SnapshotResult, TransferSeal,
};
use crate::bloomery::SourceShell;

/// Decode one `aether_data::wire`-encoded [`WorkpieceId`] per entry, or a
/// [`ClaimResult::Err`] naming the first decode failure.
fn decode_workpieces(encoded: &[Vec<u8>]) -> Result<Vec<WorkpieceId>, ClaimResult> {
    let mut workpieces = Vec::with_capacity(encoded.len());
    for bytes in encoded {
        match from_bytes(bytes) {
            Ok(workpiece) => workpieces.push(workpiece),
            Err(error) => return Err(ClaimResult::Err { error: error.to_string() }),
        }
    }
    Ok(workpieces)
}

/// Encode a [`ClaimOutcome`] into its [`ClaimResult`] reply — the shared
/// mapping the claim / transfer / release handlers converge on.
fn claim_result(outcome: ClaimOutcome) -> ClaimResult {
    match outcome {
        ClaimOutcome::Acquired => ClaimResult::Acquired,
        ClaimOutcome::Held { ref_kind, held_by } => match (to_vec(&ref_kind), to_vec(&held_by)) {
            (Ok(ref_kind), Ok(held_by)) => ClaimResult::Held { ref_kind, held_by },
            (Err(error), _) | (_, Err(error)) => ClaimResult::Err { error: error.to_string() },
        },
    }
}

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Runtime state for [`SourceCapability`]: the shell the dispatcher owns.
pub struct SourceCapabilityState {
    shell: SourceShell,
    /// Whether the inter-instance claim registry is live. Off when the GitHub
    /// connection is unconfigured (empty token / owner / repo), mirroring the
    /// mirror driver's "unconfigured → disabled" mount
    /// (`bloomery/mirror_driver/runtime.rs`): a solo / offline / test bin has no
    /// shared repository to hold refs in, so the claim ops no-op to
    /// [`ClaimResult::Acquired`] and the seal path relies on the store's
    /// active-membership uniqueness backstop alone (ADR-0150 demotes that
    /// constraint to the instance-local backstop; the ref namespace is the
    /// *inter-instance* truth, and there is none to coordinate against here).
    /// A configured bin enforces the refs.
    claims_enabled: bool,
}

impl SourceCapabilityState {
    /// Build state over an explicit shell with the claim registry live — the
    /// seam the handler tests drive (they assert real acquire / transfer /
    /// release behavior against a fake backend).
    #[must_use]
    pub fn new(shell: SourceShell) -> Self {
        Self { shell, claims_enabled: true }
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

    /// Decode `bloom` / `workpieces`, run the all-or-nothing seal `op` (acquire
    /// or release), and encode the outcome — the shared body of
    /// [`claim_seal`](Self::claim_seal) and [`release_seal`](Self::release_seal),
    /// which differ only in the shell op they drive.
    fn seal_op(
        &self,
        bloom: &[u8],
        workpieces: &[Vec<u8>],
        op: impl FnOnce(&SourceShell, &BloomId, &[WorkpieceId]) -> Result<ClaimOutcome, aether_bloomery_github::SourceError>,
    ) -> ClaimResult {
        let bloom: BloomId = match from_bytes(bloom) {
            Ok(bloom) => bloom,
            Err(error) => return ClaimResult::Err { error: error.to_string() },
        };
        let workpieces = match decode_workpieces(workpieces) {
            Ok(workpieces) => workpieces,
            Err(reply) => return reply,
        };
        // Claim registry disabled (unconfigured GitHub connection): no shared
        // repository to hold refs in, so the acquire / release is a no-op that
        // never reaches the network — the store backstop enforces single-instance
        // exclusivity on its own. The operands are decoded first regardless, so a
        // malformed request is rejected identically whether or not claims are
        // enabled (a decode failure must never read as `Acquired`).
        if !self.claims_enabled {
            return ClaimResult::Acquired;
        }
        match op(&self.shell, &bloom, &workpieces) {
            Ok(outcome) => claim_result(outcome),
            Err(error) => ClaimResult::Err { error: error.to_string() },
        }
    }

    /// Decode `bloom` / `workpieces`, acquire the seal, and encode the outcome.
    #[must_use]
    pub fn claim_seal(&self, bloom: &[u8], workpieces: &[Vec<u8>]) -> ClaimResult {
        self.seal_op(bloom, workpieces, SourceShell::claim_seal)
    }

    /// Decode `bloom` / `workpieces`, release the seal, and encode the outcome.
    #[must_use]
    pub fn release_seal(&self, bloom: &[u8], workpieces: &[Vec<u8>]) -> ClaimResult {
        self.seal_op(bloom, workpieces, SourceShell::release_seal)
    }

    /// Decode the operands, transfer the seal from predecessor to successor, and
    /// encode the outcome.
    #[must_use]
    pub fn transfer_seal(
        &self,
        predecessor: &[u8],
        successor: &[u8],
        carried: &[Vec<u8>],
        net_new: &[Vec<u8>],
        dropped: &[Vec<u8>],
    ) -> ClaimResult {
        let predecessor: BloomId = match from_bytes(predecessor) {
            Ok(predecessor) => predecessor,
            Err(error) => return ClaimResult::Err { error: error.to_string() },
        };
        let successor: BloomId = match from_bytes(successor) {
            Ok(successor) => successor,
            Err(error) => return ClaimResult::Err { error: error.to_string() },
        };
        let carried = match decode_workpieces(carried) {
            Ok(carried) => carried,
            Err(reply) => return reply,
        };
        let net_new = match decode_workpieces(net_new) {
            Ok(net_new) => net_new,
            Err(reply) => return reply,
        };
        let dropped = match decode_workpieces(dropped) {
            Ok(dropped) => dropped,
            Err(reply) => return reply,
        };
        // Claim registry disabled — see [`seal_op`](Self::seal_op): the transfer
        // is a no-op that never reaches the network, but the operands are decoded
        // first regardless so a malformed request never reads as `Acquired`.
        if !self.claims_enabled {
            return ClaimResult::Acquired;
        }
        match self.shell.transfer_seal(&predecessor, &successor, &carried, &net_new, &dropped) {
            Ok(outcome) => claim_result(outcome),
            Err(error) => ClaimResult::Err { error: error.to_string() },
        }
    }
}

#[runtime]
impl NativeActor for SourceCapability {
    type State = SourceCapabilityState;
    type Config = super::SourceConfig;

    const NAMESPACE: &'static str = "aether.source";

    fn init(config: super::SourceConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<SourceCapabilityState, BootError> {
        // Same "unconfigured → disabled" predicate the mirror driver mounts on
        // (`bloomery/mirror_driver/runtime.rs`): with no token / owner / repo
        // there is no shared repository to hold claim refs in, so the claim
        // registry is off and the seal path leans on the store backstop. The
        // shell is still connected (it opens no network until driven) so a
        // later-configured bin needs no re-mount.
        let claims_enabled = !(config.token.is_empty() || config.owner.is_empty() || config.repo.is_empty());
        let shell = SourceShell::connect(&config).map_err(|error| BootError::Other(Box::new(error)))?;
        tracing::info!(
            target: "aether_bloomery_host::source",
            claims_enabled,
            "source shell connected"
        );
        Ok(SourceCapabilityState { shell, claims_enabled })
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
    fn on_claim_seal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ClaimSeal) -> ClaimResult {
        state.claim_seal(&mail.bloom, &mail.workpieces)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_transfer_seal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: TransferSeal) -> ClaimResult {
        state.transfer_seal(&mail.predecessor, &mail.successor, &mail.carried, &mail.net_new, &mail.dropped)
    }

    #[allow(clippy::needless_pass_by_value)]
    #[handler::single]
    fn on_release_seal(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: ReleaseSeal) -> ClaimResult {
        state.release_seal(&mail.bloom, &mail.workpieces)
    }
}
