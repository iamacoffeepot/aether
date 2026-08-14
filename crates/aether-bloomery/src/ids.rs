//! The typed identifiers of the value vocabulary.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;

/// The stable identity of one intended change (ADR-0149 §The bloom).
///
/// A GitHub issue is one *projection* of a workpiece, not its identity — the
/// id is a native handle that outlives any scope revision. An umbrella is a
/// collection of workpieces, never a workpiece itself.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct WorkpieceId(pub String);

impl WorkpieceId {
    /// The reserved id of a bloom's **composition workpiece** (ADR-0191): the
    /// synthetic subject whose candidate is the weave of every member's
    /// candidate, and which walks the same line members walk.
    ///
    /// A reserved id rather than a second identifier type, because the whole
    /// point of ADR-0191 is one ontology: the composition takes a stage cursor
    /// in [`BloomRecord::progress`](crate::BloomRecord::progress), a wedge in
    /// [`BloomRecord::wedged`](crate::BloomRecord::wedged), and a slot in the
    /// dispatch ledger through the same maps a member does, keyed the same way.
    /// It is namespaced so a real workpiece cannot collide with it by accident,
    /// and the seal door refuses a membership that names it anyway
    /// ([`SealError::ReservedWorkpieceId`](crate::SealError::ReservedWorkpieceId)),
    /// so the collision is a refusal rather than a member silently sharing the
    /// composition's cursor.
    pub const COMPOSITION: &'static str = "aether.bloomery.composition";

    /// The composition workpiece's id.
    #[must_use]
    pub fn composition() -> Self {
        Self(String::from(Self::COMPOSITION))
    }

    /// Whether this id names the synthetic composition workpiece rather than a
    /// sealed member.
    #[must_use]
    pub fn is_composition(&self) -> bool {
        self.0 == Self::COMPOSITION
    }
}

/// A sealed bloom's identity: the digest of its canonical [`BloomSpec`]
/// bytes (ADR-0149 §The bloom). A bloom that differs in any sealed field is
/// a different bloom.
///
/// [`BloomSpec`]: crate::values::BloomSpec
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct BloomId(pub Digest);

/// A signer identity (ADR-0149 §The value vocabulary). The signature
/// *mechanism* is stubbed against a fake key provider in v1; the id shape
/// ships from the start so everything downstream binds to it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct KeyId(pub String);

/// An admitted fact's idempotency key (ADR-0149 §The control core). The
/// reducer treats a replayed key as a no-op, so recovery is journal replay
/// plus outbox republish without double-applying.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct IdempotencyKey(pub String);

/// A work order's idempotency nonce (ADR-0149 §The boundary, the executor
/// port). `workflow_dispatch` returns no run id, so the nonce is the durable
/// correlation key: the executor embeds it in the dispatched run's name and
/// resolves nonce → run on demand. Distinct from [`IdempotencyKey`], which
/// dedups admitted *facts* at the reducer; a nonce correlates a dispatched
/// *worker* at the executor boundary.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct Nonce(pub String);

/// Declares the closed [`StageId`] vocabulary and its complete, ordered
/// [`StageId::ALL`] enumeration from one variant list, so the set and its
/// array can never drift: a new stage extends both in lockstep, and every
/// consumer that maps over `ALL` (the stage catalog) picks it up
/// automatically rather than relying on a hand-maintained parallel list.
macro_rules! stage_vocabulary {
    ($($(#[$vmeta:meta])* $variant:ident),+ $(,)?) => {
        /// The closed stage vocabulary of the line (ADR-0149 §The line): the
        /// pipeline is these stages compiled into Rust, not a workflow language.
        /// The set is closed and exhaustively matched — a new stage is a code
        /// change, never a config entry.
        #[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        pub enum StageId {
            $($(#[$vmeta])* $variant),+
        }

        impl StageId {
            /// Every stage of the line, in declaration (execution) order.
            /// Generated with the enum from the same variant list, so it is
            /// complete and duplicate-free by construction — the catalog maps
            /// over it, so a new stage cannot be silently omitted.
            pub const ALL: &'static [StageId] = &[$(StageId::$variant),+];
        }
    };
}

stage_vocabulary! {
    Sketch,
    Scope,
    Approve,
    Construct,
    Verify,
    Refine,
    Review,
    Integrate,
    AggregateVerify,
    AggregateReview,
    Land,
    Study,
    /// Cross-member fold-conflict repair (ADR-0189): dispatched by a
    /// [`FoldConflict`](crate::Fact::FoldConflict) fact rather than line
    /// progression, against the folded checkpoint rather than the sealed base.
    Reconcile,
}

impl StageId {
    /// The attempt-scoped worker identity that runs this stage (`iama-{stage}`,
    /// ADR-0149 §The line): *who* runs, derived from the stage itself — as
    /// distinct from *how* it runs, the [`AgentProfile`] a binding references by
    /// digest. An exhaustive match, so a new stage must name its identity; the
    /// identity is never stored on a binding or receipt, only derived here.
    ///
    /// [`AgentProfile`]: crate::values::AgentProfile
    #[must_use]
    pub fn worker_identity(self) -> String {
        let slug = match self {
            Self::Sketch => "sketch",
            Self::Scope => "scope",
            Self::Approve => "approve",
            Self::Construct => "construct",
            Self::Verify => "verify",
            Self::Refine => "refine",
            Self::Review => "review",
            Self::Integrate => "integrate",
            Self::AggregateVerify => "aggregate-verify",
            Self::AggregateReview => "aggregate-review",
            Self::Land => "land",
            Self::Study => "study",
            Self::Reconcile => "reconcile",
        };
        let mut identity = String::from("iama-");
        identity.push_str(slug);
        identity
    }
}
