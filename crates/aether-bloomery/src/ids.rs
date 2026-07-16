//! The typed identifiers of the value vocabulary.

use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;

/// The stable identity of one intended change (ADR-0149 §The bloom).
///
/// A GitHub issue is one *projection* of a workpiece, not its identity — the
/// id is a native handle that outlives any scope revision. An umbrella is a
/// collection of workpieces, never a workpiece itself.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct WorkpieceId(pub String);

/// A sealed bloom's identity: the digest of its canonical [`BloomSpec`]
/// bytes (ADR-0149 §The bloom). A bloom that differs in any sealed field is
/// a different bloom.
///
/// [`BloomSpec`]: crate::values::BloomSpec
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
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
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
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
}
