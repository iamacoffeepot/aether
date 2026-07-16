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
