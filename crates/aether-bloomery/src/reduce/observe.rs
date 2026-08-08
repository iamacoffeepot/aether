//! Mainline observation: the coordinator's mainline pointer follows the
//! repository, not only its own lands (#4667).
//!
//! `snapshot.mainline` is the base a land compare-and-swaps against, and until
//! now it moved for exactly one reason — [`Decision::AdvanceMainline`], emitted
//! by a successful land. That makes it a faithful mirror of the repository only
//! in a world where blooms are the sole authors of mainline. They are not: a
//! human merge moves the real head and the coordinator never hears about it, so
//! its pointer drifts arbitrarily far behind. Everything downstream inherits the
//! drift — a fresh draft bases on a stale head, its workers check out stale
//! code, and the land it eventually attempts compare-and-swaps against a base
//! the repository left long ago.
//!
//! An observation is the missing input: the host reads the live head, and the
//! reducer decides what it means. The reducer stays pure — it is handed a digest
//! and compares, never reaching for the repository itself.

use super::{Decision, Decisions, ObserveMainlineError, Outcome, Snapshot, seal::active_unlanded_bloom};
use crate::digest::Digest;

pub(super) fn reduce_observe_mainline(snapshot: &Snapshot, head: &Digest) -> Decisions {
    if snapshot.mainline == *head {
        return Decisions::rejected(Outcome::MainlineUnchanged(*head));
    }
    // Hold the advance while a bloom is in flight. A sealed bloom's base is the
    // one head it may land on, so moving mainline out from under it converts its
    // land into a `BaseMismatch` that only a hand-driven supersession clears —
    // the observation would strand work that was progressing fine. Nothing is
    // lost by waiting: the repository head is not going anywhere, the next
    // observation after the bloom leaves flight advances to whatever mainline is
    // by then, and a bloom whose base really has moved still discovers it at
    // land time through the same `BaseMismatch` it would have hit anyway.
    //
    // This is the conservative half of the policy. Advancing *through* a live
    // bloom — superseding it onto the new base automatically — is the resync
    // trigger, and it belongs with the machinery that can mint the successor
    // rather than here, where the only available move is to strand it.
    if let Some(bloom) = active_unlanded_bloom(snapshot) {
        return Decisions::rejected(Outcome::ObserveMainlineRejected(ObserveMainlineError::BloomInFlight(bloom)));
    }
    Decisions {
        outcome: Outcome::MainlineAdvanced { from: snapshot.mainline, to: *head },
        effects: alloc::vec![Decision::AdvanceMainline { from: snapshot.mainline, to: *head }],
    }
}
