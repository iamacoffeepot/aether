//! The composition workpiece's own vocabulary (ADR-0191).
//!
//! A bloom's members construct in parallel and each resolves on its own
//! evidence; the composition of their candidates is a workpiece in its own
//! right, and the one thing it owns that no member does is a finding about
//! *someone else's* code. A composition review judges whether each member's
//! intent survived the weave, so a defect it names may belong to the weave —
//! repaired in the weave — or to a member that has already passed its own
//! review and is immutable (ADR-0191 §4). The second class has nowhere to go
//! under the old model except back into the member, which is exactly the
//! transition ADR-0191 abolishes. It goes here instead.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::ids::WorkpieceId;

/// A composition-review observation recorded against the bloom rather than
/// routed into a member (ADR-0191 §4).
///
/// Fix-forward, as the team process treats mainline: a member workpiece that
/// has passed its review is done, so an observation about its code becomes new
/// work for a future bloom instead of re-opening finished, reviewed work. This
/// is the durable half of that — the record an operator (or the study that
/// files the follow-up) reads, naming what was judged, which members it points
/// at, and the artifact that says it.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionFinding {
    /// The weave tree the finding was raised against — the composition
    /// candidate under review when it was returned.
    pub subject: Digest,
    /// The returned verdict's artifact digest: the findings themselves.
    pub detail: Digest,
    /// The members the verdict implicated, in the verdict's order. Empty when
    /// the verdict named none — a finding about the weave as a whole.
    ///
    /// Advisory, and deliberately so: naming a member here files follow-up
    /// work, it does not dispatch anything against that member. The whole point
    /// of recording rather than routing is that no member reads this.
    pub implicated: Vec<WorkpieceId>,
}
