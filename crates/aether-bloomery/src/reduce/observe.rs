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
//! and compares, never reaching for the repository itself. A head the host
//! classified as a strict ancestor of current mainline arrives as
//! [`Fact::ObserveMainlineDiverged`](crate::Fact::ObserveMainlineDiverged) and
//! is refused by name (#4938); it does not advance. A rewritten (unrelated)
//! live ref is classified as followable at the host and arrives as
//! [`Fact::ObserveMainline`](crate::Fact::ObserveMainline), so a history
//! rewrite recovers by observation instead of pinning both pointers to a
//! commit the remote no longer has.

use super::{Decision, Decisions, Outcome, Snapshot, seal::active_unlanded_bloom};
use crate::digest::Digest;

pub(super) fn reduce_observe_mainline(snapshot: &Snapshot, head: &Digest) -> Decisions {
    // Record what the repository said before deciding what it means. An
    // observation always tells the truth about the head; only the *advance* is
    // conditional, and separating the two is what gives a held observation
    // somewhere to live (#4709). Recorded on every branch, so `observed` is
    // always the freshest head the coordinator has heard of rather than the
    // freshest one it was allowed to act on.
    let mut effects = alloc::vec![Decision::RecordObservation { head: *head }];
    if snapshot.mainline == *head {
        return Decisions { outcome: Outcome::MainlineUnchanged(*head), effects };
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
    // This is the conservative half of the policy. The other half is a
    // supersession that rebases onto the head recorded above — the resync
    // trigger, which lives with the machinery that can mint the successor
    // (`reduce_supersede`) rather than here, where the only available move is to
    // strand it.
    if let Some(bloom) = active_unlanded_bloom(snapshot) {
        return Decisions { outcome: Outcome::MainlineHeld { head: *head, by: bloom }, effects };
    }
    effects.push(Decision::AdvanceMainline { from: snapshot.mainline, to: *head });
    Decisions { outcome: Outcome::MainlineAdvanced { from: snapshot.mainline, to: *head }, effects }
}

/// An observation the host already classified as a strict ancestor of
/// current mainline (#4938). Record nothing: folding the stale head into
/// `observed` would poison the only base a supersession may rebase onto.
/// A rewritten live ref does not arrive here — the host follows it as
/// [`Fact::ObserveMainline`].
pub(super) fn reduce_observe_mainline_diverged(snapshot: &Snapshot, head: &Digest) -> Decisions {
    Decisions::rejected(Outcome::MainlineDiverged { head: *head, mainline: snapshot.mainline })
}
