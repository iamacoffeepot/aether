//! The dispatch-timeout evidence record (ADR-0177).
//!
//! A dispatched lane that never exits leaves nothing behind: no evidence, no
//! conclusion, no verdict. The deadline enforcement still owes the reducer a
//! detail artifact, because every [`Evidence`](super::Evidence) names one — so
//! the expiry synthesises this record and hands its address over as the
//! evidence's `detail`.
//!
//! Derived *only* from the expired order's own facts — the bloom, the member (a
//! bloom-level lane has none), the stage, the nonce, the digest Bloomery
//! displayed, and the deadline that was stored beside the order. No observation
//! time and no free-form text, deliberately: the same expired order must yield
//! the same content address on every attempt to handle it, so a crash between
//! storing the record and consuming the order re-derives the identical artifact
//! on restart rather than a second one.

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{BloomId, Nonce, StageId, WorkpieceId};

/// The durable account of one dispatched order that outlived its sealed
/// execution limit (ADR-0177).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TimeoutRecord {
    /// The bloom the expired order belonged to.
    pub bloom: BloomId,
    /// The member the order resolved, or `None` for a bloom-level lane (the
    /// aggregate stages carry no member axis).
    pub workpiece: Option<WorkpieceId>,
    /// The line stage the expired order dispatched.
    pub stage: StageId,
    /// The expired order's idempotency nonce.
    pub nonce: Nonce,
    /// The digest Bloomery displayed for the order — what the evidence this
    /// record details binds to.
    pub subject: Digest,
    /// The absolute deadline, in Unix milliseconds, the order was recorded
    /// with and then crossed.
    pub deadline_unix_millis: u64,
}

impl ContentAddressed for TimeoutRecord {
    const DOMAIN: &'static str = "aether.bloomery.timeout_record";
}

impl TimeoutRecord {
    /// The record's typed content-addressed identity in the ADR-0149 value
    /// vocabulary: sha256 over the domain tag above, length-prefixed, followed
    /// by the record's canonical wire bytes.
    ///
    /// Not the address an expiry details its evidence by. An evidence `detail`
    /// names bytes in the artifacts store, which keys on plain sha256 of the
    /// bytes alone — the domain tag makes this digest a different number, and a
    /// detail taken from here would resolve to nothing. The expiry mints that
    /// one with [`Digest::of_wire_bytes`] over the same bytes it stores, the way
    /// every other evidence detail is minted.
    #[must_use]
    pub fn id(&self) -> Digest {
        digest_of(self)
    }
}
