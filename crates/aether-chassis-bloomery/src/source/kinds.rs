//! The `aether.source.*` transact-mail kind family (ADR-0149 §The boundary).
//!
//! These are the typed requests the guest (or an external RPC client) sends to
//! the `aether.source` mailbox to reach the existing `SourceShell` port
//! operations — snapshot / checkpoint / checkpoints / integrate / land —
//! following the `aether.store.*` precedent (`store/kinds.rs`).
//!
//! The `aether-bloomery` port value types ([`aether_bloomery::Digest`],
//! [`aether_bloomery::BloomId`], [`aether_bloomery::SourceSnapshot`],
//! [`aether_bloomery::Checkpoint`], [`aether_bloomery::IntegrateOutcome`], and
//! [`aether_bloomery::LandOutcome`]) are carried as their canonical
//! [`aether_data::wire`] bytes rather than typed fields — exactly as
//! `AppendEvent.event: Vec<u8>` carries an encoded `Event` — because those
//! value types are serde-encoded but not `Schema`; this capability has no
//! reason to key or filter on any of these fields, so nothing here needs the
//! typed axis the store's `idempotency_key` / `members` fields have.

use serde::{Deserialize, Serialize};

/// Snapshot the source at `base` (an `aether_data::wire`-encoded
/// [`aether_bloomery::Digest`]).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.snapshot")]
pub struct Snapshot {
    /// The `aether_data::wire`-encoded base [`aether_bloomery::Digest`].
    #[serde(with = "aether_data::bytes")]
    pub base: Vec<u8>,
}

/// Reply to [`Snapshot`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.snapshot_result")]
pub enum SnapshotResult {
    /// The snapshot succeeded.
    Ok {
        /// The `aether_data::wire`-encoded [`aether_bloomery::SourceSnapshot`].
        #[serde(with = "aether_data::bytes")]
        snapshot: Vec<u8>,
    },
    /// The snapshot failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Record an integration checkpoint for `bloom` at `tree` (both
/// `aether_data::wire`-encoded).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.checkpoint")]
pub struct RecordCheckpoint {
    /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`].
    #[serde(with = "aether_data::bytes")]
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded integrated tree [`aether_bloomery::Digest`].
    #[serde(with = "aether_data::bytes")]
    pub tree: Vec<u8>,
}

/// Reply to [`RecordCheckpoint`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.checkpoint_result")]
pub enum RecordCheckpointResult {
    /// The checkpoint was recorded.
    Ok {
        /// The `aether_data::wire`-encoded [`aether_bloomery::Checkpoint`].
        #[serde(with = "aether_data::bytes")]
        checkpoint: Vec<u8>,
    },
    /// The checkpoint failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Enumerate `bloom`'s recorded checkpoints (for successor reuse).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.checkpoints")]
pub struct ListCheckpoints {
    /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`].
    #[serde(with = "aether_data::bytes")]
    pub bloom: Vec<u8>,
}

/// Reply to [`ListCheckpoints`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.checkpoints_result")]
pub enum ListCheckpointsResult {
    /// The recorded checkpoints, each `aether_data::wire`-encoded.
    Ok {
        /// One `aether_data::wire`-encoded [`aether_bloomery::Checkpoint`] per entry.
        checkpoints: Vec<Vec<u8>>,
    },
    /// The enumeration failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Integrate `candidate` onto `bloom`'s integration branch, guarded by the
/// `expected` checkpoint (all `aether_data::wire`-encoded).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.integrate")]
pub struct Integrate {
    /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`].
    #[serde(with = "aether_data::bytes")]
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded candidate [`aether_bloomery::Digest`].
    #[serde(with = "aether_data::bytes")]
    pub candidate: Vec<u8>,
    /// The `aether_data::wire`-encoded expected [`aether_bloomery::Checkpoint`].
    #[serde(with = "aether_data::bytes")]
    pub expected: Vec<u8>,
}

/// Reply to [`Integrate`], mirroring [`aether_bloomery::IntegrateOutcome`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.integrate_result")]
pub enum IntegrateResult {
    /// The candidate integrated; the branch now carries this tree.
    Integrated {
        /// The `aether_data::wire`-encoded resulting [`aether_bloomery::Digest`].
        #[serde(with = "aether_data::bytes")]
        tree: Vec<u8>,
        /// The `aether_data::wire`-encoded landable head commit's
        /// [`aether_bloomery::Digest`], distinct from `tree` (issue #3615).
        #[serde(with = "aether_data::bytes")]
        head: Vec<u8>,
    },
    /// The candidate conflicted and was not integrated.
    Conflict {
        /// The `aether_data::wire`-encoded conflicting [`aether_bloomery::Digest`].
        #[serde(with = "aether_data::bytes")]
        at: Vec<u8>,
    },
    /// The expected checkpoint was stale.
    StaleCheckpoint {
        /// The `aether_data::wire`-encoded actual [`aether_bloomery::Digest`].
        #[serde(with = "aether_data::bytes")]
        actual: Vec<u8>,
    },
    /// The integrate failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Propose landing `bloom`'s `new_head` onto mainline, guarded by
/// `expected_base` (all `aether_data::wire`-encoded).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.land")]
pub struct Land {
    /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`].
    #[serde(with = "aether_data::bytes")]
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded expected base [`aether_bloomery::Digest`].
    #[serde(with = "aether_data::bytes")]
    pub expected_base: Vec<u8>,
    /// The `aether_data::wire`-encoded new head [`aether_bloomery::Digest`].
    #[serde(with = "aether_data::bytes")]
    pub new_head: Vec<u8>,
}

/// Reply to [`Land`], mirroring [`aether_bloomery::LandOutcome`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.land_result")]
pub enum LandResult {
    /// The resolved head was proposed; mainline has not moved yet. Watch the
    /// proposal with [`PollLand`] to see where it ends up.
    Proposed {
        /// The proposal's number on the backend.
        number: u64,
    },
    /// The land was refused: mainline had moved off the expected base.
    BaseMoved {
        /// The `aether_data::wire`-encoded base the caller expected.
        #[serde(with = "aether_data::bytes")]
        expected: Vec<u8>,
        /// The `aether_data::wire`-encoded base mainline was actually at.
        #[serde(with = "aether_data::bytes")]
        actual: Vec<u8>,
    },
    /// The land failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Read where the land proposal `number`, previously issued for `bloom` against
/// `expected_base`, has got to (digests `aether_data::wire`-encoded).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.poll_land")]
pub struct PollLand {
    /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`].
    #[serde(with = "aether_data::bytes")]
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded expected base [`aether_bloomery::Digest`].
    #[serde(with = "aether_data::bytes")]
    pub expected_base: Vec<u8>,
    /// The proposal number [`LandResult::Proposed`] handed back.
    pub number: u64,
}

/// Reply to [`PollLand`], mirroring [`aether_bloomery::LandProposal`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.poll_land_result")]
pub enum PollLandResult {
    /// Still open. Mainline has not moved; keep watching.
    Open,
    /// Accepted — mainline moved, and the receipt says where to.
    Landed {
        /// The `aether_data::wire`-encoded [`aether_bloomery::LandingReceipt`].
        #[serde(with = "aether_data::bytes")]
        receipt: Vec<u8>,
    },
    /// Terminated without landing — the proposal was declined.
    Declined,
    /// The proposal's own checks failed, so it cannot merge (#4689). Appended
    /// so the prior variants' wire discriminants are unchanged.
    ChecksFailed {
        /// The failing check names, in listing order.
        failing: Vec<String>,
    },
    /// The poll failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

// The `aether.source.{claim_seal,transfer_seal,release_seal}` request kinds and
// their shared `aether.source.claim_result` reply are defined in
// `aether-bloomery` (`control/source_mail.rs`), not here, so the wasm
// `ControlCore` actor's `on_admit` can construct and send them — the same
// cross-crate-cycle relocation the store's `Commit` family uses (ADR-0150 §The
// claim registry; owner directive on #3547). Re-exported inward so this
// capability's `SourceCapability` and its public API keep naming them under
// `aether.source::{…}` unchanged. The `#[kind(name = "…")]` wire identity rides
// the move; only the definition site changed.
pub use aether_bloomery::{
    ClaimResult, ClaimSeal, CompleteRelease, CompleteReleaseResult, CompleteTransfer, EnumerateClaims,
    EnumerateClaimsResult, ObserveMainline, ObserveMainlineResult, ReleaseSeal, TransferSeal,
};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_data::wire::{from_bytes, to_vec};

    use super::IntegrateResult;

    #[test]
    fn integrate_result_wire_round_trips_and_pins_its_encoded_shape() {
        // Tripwire: the wire layout — `u32` variant selector (declaration
        // order), then the `Conflict` variant's single `Vec<u8>` field as a
        // `u32` count followed by its bytes (ADR-0118) — is pinned so a
        // format or field-order drift trips this rather than surfacing only
        // as a cross-version decode failure.
        let value = IntegrateResult::Conflict { at: vec![1, 2, 3] };
        let mut expected = Vec::new();
        expected.extend_from_slice(&1u32.to_le_bytes()); // `Conflict` is declared second (index 1).
        expected.extend_from_slice(&3u32.to_le_bytes()); // `at`'s byte count.
        expected.extend_from_slice(&[1, 2, 3]);
        let bytes = to_vec(&value).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(from_bytes::<IntegrateResult>(&bytes).unwrap(), value);
    }

    #[test]
    fn integrated_variant_carries_a_distinct_head_and_pins_its_encoded_shape() {
        // Tripwire: the `Integrated` variant (declared first, index 0) carries
        // `tree` then `head` as two length-prefixed `Vec<u8>` fields (#3615) — a
        // field-order or missing-field drift trips the pinned layout here rather
        // than surfacing only as a cross-version decode mismatch.
        let value = IntegrateResult::Integrated { tree: vec![1, 2], head: vec![3, 4, 5] };
        let mut expected = Vec::new();
        expected.extend_from_slice(&0u32.to_le_bytes()); // `Integrated` is declared first (index 0).
        expected.extend_from_slice(&2u32.to_le_bytes()); // `tree`'s byte count.
        expected.extend_from_slice(&[1, 2]);
        expected.extend_from_slice(&3u32.to_le_bytes()); // `head`'s byte count.
        expected.extend_from_slice(&[3, 4, 5]);
        let bytes = to_vec(&value).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(from_bytes::<IntegrateResult>(&bytes).unwrap(), value);
    }
}
