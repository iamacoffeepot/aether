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
    pub base: Vec<u8>,
}

/// Reply to [`Snapshot`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.snapshot_result")]
pub enum SnapshotResult {
    /// The snapshot succeeded.
    Ok {
        /// The `aether_data::wire`-encoded [`aether_bloomery::SourceSnapshot`].
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
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded integrated tree [`aether_bloomery::Digest`].
    pub tree: Vec<u8>,
}

/// Reply to [`RecordCheckpoint`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.checkpoint_result")]
pub enum RecordCheckpointResult {
    /// The checkpoint was recorded.
    Ok {
        /// The `aether_data::wire`-encoded [`aether_bloomery::Checkpoint`].
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
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded candidate [`aether_bloomery::Digest`].
    pub candidate: Vec<u8>,
    /// The `aether_data::wire`-encoded expected [`aether_bloomery::Checkpoint`].
    pub expected: Vec<u8>,
}

/// Reply to [`Integrate`], mirroring [`aether_bloomery::IntegrateOutcome`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.integrate_result")]
pub enum IntegrateResult {
    /// The candidate integrated; the branch now carries this tree.
    Integrated {
        /// The `aether_data::wire`-encoded resulting [`aether_bloomery::Digest`].
        tree: Vec<u8>,
    },
    /// The candidate conflicted and was not integrated.
    Conflict {
        /// The `aether_data::wire`-encoded conflicting [`aether_bloomery::Digest`].
        at: Vec<u8>,
    },
    /// The expected checkpoint was stale.
    StaleCheckpoint {
        /// The `aether_data::wire`-encoded actual [`aether_bloomery::Digest`].
        actual: Vec<u8>,
    },
    /// The integrate failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Compare-and-swap mainline from `expected_base` to `new_head` for `bloom`
/// (all `aether_data::wire`-encoded).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.land")]
pub struct Land {
    /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`].
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded expected base [`aether_bloomery::Digest`].
    pub expected_base: Vec<u8>,
    /// The `aether_data::wire`-encoded new head [`aether_bloomery::Digest`].
    pub new_head: Vec<u8>,
}

/// Reply to [`Land`], mirroring [`aether_bloomery::LandOutcome`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.land_result")]
pub enum LandResult {
    /// The swap succeeded; mainline moved and a receipt was issued.
    Landed {
        /// The `aether_data::wire`-encoded [`aether_bloomery::LandingReceipt`].
        receipt: Vec<u8>,
    },
    /// The swap was refused: mainline had moved off the expected base.
    BaseMoved {
        /// The `aether_data::wire`-encoded base the caller expected.
        expected: Vec<u8>,
        /// The `aether_data::wire`-encoded base mainline was actually at.
        actual: Vec<u8>,
    },
    /// The land failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Acquire `bloom`'s claim refs — one per member workpiece plus the single
/// mainline-admission ref — all-or-nothing (ADR-0150 §The claim registry,
/// mirroring [`aether_bloomery::SourceBackend::claim_seal`]).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.claim_seal")]
pub struct ClaimSeal {
    /// The `aether_data::wire`-encoded claiming [`aether_bloomery::BloomId`].
    pub bloom: Vec<u8>,
    /// One `aether_data::wire`-encoded [`aether_bloomery::WorkpieceId`] per member.
    pub workpieces: Vec<Vec<u8>>,
}

/// Transfer the seal from `predecessor` to `successor` on a supersession
/// (ADR-0150 §The claim registry, mirroring
/// [`aether_bloomery::SourceBackend::transfer_seal`]): fast-forward the
/// `carried` refs and the admission ref, fresh-acquire `net_new`, release
/// `dropped`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.transfer_seal")]
pub struct TransferSeal {
    /// The `aether_data::wire`-encoded predecessor [`aether_bloomery::BloomId`].
    pub predecessor: Vec<u8>,
    /// The `aether_data::wire`-encoded successor [`aether_bloomery::BloomId`].
    pub successor: Vec<u8>,
    /// The workpieces fast-forwarded from predecessor to successor, each
    /// `aether_data::wire`-encoded [`aether_bloomery::WorkpieceId`].
    pub carried: Vec<Vec<u8>>,
    /// The successor's fresh-acquired workpieces, each `aether_data::wire`-encoded.
    pub net_new: Vec<Vec<u8>>,
    /// The predecessor's released workpieces, each `aether_data::wire`-encoded.
    pub dropped: Vec<Vec<u8>>,
}

/// Release `bloom`'s claim refs — the member workpieces plus the admission ref —
/// each by a fast-forward CAS to a tombstone (ADR-0150 §The claim registry,
/// mirroring [`aether_bloomery::SourceBackend::release_seal`]).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.release_seal")]
pub struct ReleaseSeal {
    /// The `aether_data::wire`-encoded releasing [`aether_bloomery::BloomId`].
    pub bloom: Vec<u8>,
    /// One `aether_data::wire`-encoded [`aether_bloomery::WorkpieceId`] per member.
    pub workpieces: Vec<Vec<u8>>,
}

/// Reply to [`ClaimSeal`], [`TransferSeal`], and [`ReleaseSeal`], mirroring
/// [`aether_bloomery::ClaimOutcome`]. The three ops share one reply because they
/// return the same outcome — a clean [`ClaimOutcome::Held`] refusal is not an
/// error (the [`aether_bloomery::LandOutcome`] / [`LandResult`] shape).
///
/// [`ClaimOutcome::Held`]: aether_bloomery::ClaimOutcome::Held
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.claim_result")]
pub enum ClaimResult {
    /// Every targeted ref was acquired / transferred / released.
    Acquired,
    /// A targeted ref was already held by another bloom, so the operation was
    /// refused (rolled back to leave no partial claim).
    Held {
        /// The `aether_data::wire`-encoded conflicting [`aether_bloomery::ClaimRefKind`].
        ref_kind: Vec<u8>,
        /// The `aether_data::wire`-encoded [`aether_bloomery::BloomId`] holding it.
        held_by: Vec<u8>,
    },
    /// The operation failed for a non-refusal reason.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

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
}
