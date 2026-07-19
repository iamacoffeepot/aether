//! The `aether.source.*` claim transact-mail kinds the wasm control core sends
//! to the native source-port capability (ADR-0150 §The claim registry).
//!
//! These kinds — the seal/transfer/release ops ([`ClaimSeal`], [`TransferSeal`],
//! [`ReleaseSeal`]) with their shared [`ClaimResult`] reply, plus the
//! boot-reconcile deep-heal ops ([`EnumerateClaims`] with [`EnumerateClaimsResult`],
//! and [`CompleteTransfer`] / [`CompleteRelease`], amended PR #3556) — are
//! *defined here* in `aether-bloomery` rather
//! than in `aether-bloomery-host` alongside the rest of the `aether.source.*`
//! family, for the same reason the store's [`Commit`](super::Commit) family
//! lives here: the native `ControlCore` cap's `on_admit`
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

/// Enumerate every live claim ref, classified by holder (ADR-0150 §The claim
/// registry, amended PR #3556, mirroring
/// [`aether_bloomery::SourceBackend::enumerate_claims`](crate::port::SourceBackend::enumerate_claims)).
/// The boot reconcile drives this once after the V1 re-assert / re-release to
/// detect the deep-heal states — a tombstoned ref to sweep, a ref stranded on a
/// superseded predecessor to complete.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.enumerate_claims")]
pub struct EnumerateClaims;

/// Reply to [`EnumerateClaims`]: every live claim ref as a wire-encoded
/// [`ClaimRefState`](crate::port::ClaimRefState).
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[kind(name = "aether.source.enumerate_claims_result")]
pub enum EnumerateClaimsResult {
    /// The enumeration succeeded.
    Ok {
        /// One `aether_data::wire`-encoded [`ClaimRefState`](crate::port::ClaimRefState) per live ref.
        states: Vec<Vec<u8>>,
    },
    /// The enumeration failed.
    Err {
        /// A human-readable failure reason.
        error: String,
    },
}

/// Idempotent per-ref transfer completion — case-3 primitive **(a)** (ADR-0150
/// §The claim registry, amended PR #3556, mirroring
/// [`aether_bloomery::SourceBackend::complete_transfer`](crate::port::SourceBackend::complete_transfer)):
/// fast-forward one carried/admission ref from `predecessor` to `successor`. A
/// ref already at the successor is the no-op that lets the boot re-drive
/// converge. Shares the [`ClaimResult`] reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.complete_transfer")]
pub struct CompleteTransfer {
    /// The `aether_data::wire`-encoded predecessor [`BloomId`](crate::ids::BloomId).
    pub predecessor: Vec<u8>,
    /// The `aether_data::wire`-encoded successor [`BloomId`](crate::ids::BloomId).
    pub successor: Vec<u8>,
    /// The `aether_data::wire`-encoded [`ClaimRefKind`](crate::port::ClaimRefKind) to move.
    pub ref_kind: Vec<u8>,
}

/// Idempotent per-ref release completion (ADR-0150 §The claim registry, amended
/// PR #3556, mirroring
/// [`aether_bloomery::SourceBackend::complete_release`](crate::port::SourceBackend::complete_release)):
/// sweep a tombstoned ref or release a ref stranded on a superseded predecessor.
/// An **empty** `bloom` is the `None` holder — the tombstone-sweep case that
/// authorizes no live holder; a non-empty `bloom` releases that holder's ref.
/// Shares the [`ClaimResult`] reply.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.source.complete_release")]
pub struct CompleteRelease {
    /// The `aether_data::wire`-encoded expected-holder [`BloomId`](crate::ids::BloomId),
    /// or **empty** for the holder-agnostic tombstone sweep (`None`).
    pub bloom: Vec<u8>,
    /// The `aether_data::wire`-encoded [`ClaimRefKind`](crate::port::ClaimRefKind) to release.
    pub ref_kind: Vec<u8>,
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
