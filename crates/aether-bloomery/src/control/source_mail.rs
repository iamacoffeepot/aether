//! The `aether.source.*` claim transact-mail kinds the wasm control core sends
//! to the native source-port capability (ADR-0150 §The claim registry).
//!
//! These four kinds — [`ClaimSeal`], [`TransferSeal`], [`ReleaseSeal`], and the
//! shared [`ClaimResult`] reply — are *defined here* in `aether-bloomery` rather
//! than in `aether-bloomery-host` alongside the rest of the `aether.source.*`
//! family, for the same reason the store's [`Commit`](super::Commit) family
//! lives here: the wasm [`ControlCore`](super::ControlCore) actor's `on_admit`
//! must construct and send them (the seal/supersede/release interposition,
//! ADR-0150 §The claim registry), and `aether-bloomery-host` depends on
//! `aether-bloomery`, so a reverse edge would be a package cycle. Defining them
//! here keeps one definition both sides share — the host's `SourceCapability`
//! re-exports them inward, cycle-free, exactly as it re-exports
//! [`Commit`](super::Commit). The wire contract is identical wherever the type
//! is declared: the `#[kind(name = "…")]` literal is the identity.
//!
//! Like the rest of the `aether.source.*` family, these carry the
//! `aether-bloomery` port value types ([`BloomId`](crate::ids::BloomId),
//! [`WorkpieceId`](crate::ids::WorkpieceId), [`ClaimRefKind`](crate::port::ClaimRefKind))
//! as their canonical [`aether_data::wire`] bytes rather than typed fields —
//! those value types are serde-encoded but not `Schema`, and this capability
//! keys on none of them.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// Acquire `bloom`'s claim refs — one per member workpiece plus the single
/// mainline-admission ref — all-or-nothing (ADR-0150 §The claim registry,
/// mirroring [`aether_bloomery::SourceBackend::claim_seal`](crate::port::SourceBackend::claim_seal)).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.claim_seal")]
pub struct ClaimSeal {
    /// The `aether_data::wire`-encoded claiming [`BloomId`](crate::ids::BloomId).
    pub bloom: Vec<u8>,
    /// One `aether_data::wire`-encoded [`WorkpieceId`](crate::ids::WorkpieceId) per member.
    pub workpieces: Vec<Vec<u8>>,
}

/// Transfer the seal from `predecessor` to `successor` on a supersession
/// (ADR-0150 §The claim registry, mirroring
/// [`aether_bloomery::SourceBackend::transfer_seal`](crate::port::SourceBackend::transfer_seal)):
/// fast-forward the `carried` refs and the admission ref, fresh-acquire
/// `net_new`, release `dropped`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.transfer_seal")]
pub struct TransferSeal {
    /// The `aether_data::wire`-encoded predecessor [`BloomId`](crate::ids::BloomId).
    pub predecessor: Vec<u8>,
    /// The `aether_data::wire`-encoded successor [`BloomId`](crate::ids::BloomId).
    pub successor: Vec<u8>,
    /// The workpieces fast-forwarded from predecessor to successor, each
    /// `aether_data::wire`-encoded [`WorkpieceId`](crate::ids::WorkpieceId).
    pub carried: Vec<Vec<u8>>,
    /// The successor's fresh-acquired workpieces, each `aether_data::wire`-encoded.
    pub net_new: Vec<Vec<u8>>,
    /// The predecessor's released workpieces, each `aether_data::wire`-encoded.
    pub dropped: Vec<Vec<u8>>,
}

/// Release `bloom`'s claim refs — the member workpieces plus the admission ref —
/// each by a fast-forward CAS to a tombstone (ADR-0150 §The claim registry,
/// mirroring [`aether_bloomery::SourceBackend::release_seal`](crate::port::SourceBackend::release_seal)).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.release_seal")]
pub struct ReleaseSeal {
    /// The `aether_data::wire`-encoded releasing [`BloomId`](crate::ids::BloomId).
    pub bloom: Vec<u8>,
    /// One `aether_data::wire`-encoded [`WorkpieceId`](crate::ids::WorkpieceId) per member.
    pub workpieces: Vec<Vec<u8>>,
}

/// Reply to [`ClaimSeal`], [`TransferSeal`], and [`ReleaseSeal`], mirroring
/// [`aether_bloomery::ClaimOutcome`](crate::port::ClaimOutcome). The three ops
/// share one reply because they return the same outcome — a clean
/// [`Held`](ClaimResult::Held) refusal is not an error (the
/// [`LandOutcome`](crate::port::LandOutcome) shape).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.claim_result")]
pub enum ClaimResult {
    /// Every targeted ref was acquired / transferred / released.
    Acquired,
    /// A targeted ref was already held by another bloom, so the operation was
    /// refused (rolled back to leave no partial claim).
    Held {
        /// The `aether_data::wire`-encoded conflicting [`ClaimRefKind`](crate::port::ClaimRefKind).
        ref_kind: Vec<u8>,
        /// The `aether_data::wire`-encoded [`BloomId`](crate::ids::BloomId) holding it.
        held_by: Vec<u8>,
    },
    /// The operation failed for a non-refusal reason.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}
